use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use forgeclaw_core::{ForgeEvent, RepoId, Result, ScopedToken, ThreadKey};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::grants::{GrantStore, SessionKey};

/// One deterministic rule from the operator-managed OpenClaw configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerRule {
    pub on: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub filter: TriggerFilter,
}

fn enabled() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(untagged)]
pub enum TriggerFilter {
    #[default]
    Any,
    One(BTreeMap<String, String>),
    Or(Vec<BTreeMap<String, String>>),
}

impl TriggerRule {
    pub fn validate_all(rules: &[Self]) -> Result<()> {
        if rules.len() > 64 {
            return Err(config_error("at most 64 trigger rules are allowed"));
        }
        for rule in rules {
            let fields = match rule.on.as_str() {
                "comment.created" => &["mentions", "assignees", "author", "body"][..],
                // `assignee` was emitted by an older editor. Keep it as an
                // alias so persisted configurations continue to work.
                "issue.assigned" => &["assignees", "assignee", "author"],
                "pull_request.review_requested" => &["reviewer", "author"],
                "pull_request.changes_requested" => &["reviewer", "body"],
                "ci.run_completed" => &["conclusion", "pr_author", "workflow"],
                "pull_request.opened" => &["author"],
                _ => return Err(config_error(format!("unknown trigger event: {}", rule.on))),
            };
            let clauses: &[BTreeMap<String, String>] = match &rule.filter {
                TriggerFilter::Any => &[],
                TriggerFilter::One(clause) => std::slice::from_ref(clause),
                TriggerFilter::Or(clauses) => clauses,
            };
            if clauses.len() > 16 {
                return Err(config_error(format!(
                    "trigger {} has more than 16 filter groups",
                    rule.on
                )));
            }
            for clause in clauses {
                if clause.is_empty() {
                    return Err(config_error(format!(
                        "trigger {} has an empty filter group",
                        rule.on
                    )));
                }
                if clause.len() > 16 {
                    return Err(config_error(format!(
                        "trigger {} has more than 16 conditions",
                        rule.on
                    )));
                }
                for (field, pattern) in clause {
                    if !fields.contains(&field.as_str()) {
                        return Err(config_error(format!(
                            "unknown field {field} for trigger {}",
                            rule.on
                        )));
                    }
                    if pattern.is_empty() || pattern == "!" {
                        return Err(config_error(format!(
                            "empty pattern for {field} in trigger {}",
                            rule.on
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn matches(&self, event: &ForgeEvent, bot_user: &str) -> bool {
        let matches_clause = |clause: &BTreeMap<String, String>| {
            clause.iter().all(|(field, pattern)| {
                let (wanted, pattern) = match pattern.strip_prefix('!') {
                    Some(pattern) => (false, pattern),
                    None => (true, pattern.as_str()),
                };
                let pattern = if pattern == "@me" { bot_user } else { pattern };
                let payload_field = if self.on == "issue.assigned" && field == "assignee" {
                    "assignees"
                } else {
                    field
                };
                let matched = event
                    .payload
                    .get(payload_field)
                    .is_some_and(|value| matches_value(value, pattern));
                matched == wanted
            })
        };
        self.enabled
            && self.on == event.kind
            && match &self.filter {
                TriggerFilter::Any => true,
                TriggerFilter::One(clause) => matches_clause(clause),
                TriggerFilter::Or(clauses) => clauses.iter().any(matches_clause),
            }
    }
}

fn config_error(message: impl Into<String>) -> forgeclaw_core::Error {
    forgeclaw_core::Error::Config(message.into())
}

fn matches_value(value: &Value, pattern: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| matches_value(value, pattern)),
        Value::String(value) => value == pattern,
        Value::Bool(value) => pattern.parse().ok() == Some(*value),
        Value::Number(value) => pattern.parse::<serde_json::Number>().ok().as_ref() == Some(value),
        _ => false,
    }
}

/// The small adapter a forge needs for webhook routing. Forge-specific crates
/// own signature verification and event normalization; the router owns no
/// forge protocol details.
#[async_trait]
pub trait WebhookForge: Send + Sync {
    async fn whoami(&self) -> Result<String>;
    async fn mint_token(&self, label: &str) -> Result<ScopedToken>;
    async fn revoke_token(&self, token: &ScopedToken) -> Result<()>;
}

/// Starts an OpenClaw turn. Its implementation is intentionally the only
/// place that knows how the gateway is reached.
#[async_trait]
pub trait Gateway: Send + Sync {
    async fn submit(&self, session_key: &str, message: &str) -> Result<()>;
}

/// Gateway submission through the supported OpenClaw CLI. The daemon image is
/// based on the gateway image, so this command uses the same persisted config
/// and agent database as the long-lived gateway service.
#[derive(Debug)]
pub struct OpenClawCli {
    program: PathBuf,
    secret_envs: Vec<String>,
}

impl OpenClawCli {
    pub fn new(program: impl Into<PathBuf>, secret_envs: Vec<String>) -> Self {
        Self {
            program: program.into(),
            secret_envs,
        }
    }

    async fn run(&self, session_key: &str, message: &str) -> Result<()> {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args([
                "agent",
                "--agent",
                "main",
                "--session-key",
                session_key,
                "--message",
                message,
                "--json",
            ])
            .stdout(Stdio::null());
        for name in &self.secret_envs {
            command.env_remove(name);
        }
        let status = command.status().await?;
        if status.success() {
            Ok(())
        } else {
            Err(forgeclaw_core::Error::Forge(format!(
                "openclaw agent exited with {status}"
            )))
        }
    }
}

#[async_trait]
impl Gateway for OpenClawCli {
    async fn submit(&self, session_key: &str, message: &str) -> Result<()> {
        self.run(session_key, message).await
    }
}
/// The outcome of accepting one delivery. It is useful for HTTP status/logging
/// but contains no token or prompt content.
#[derive(Debug, PartialEq, Eq)]
pub enum Routed {
    Ignored,
    Engaged { sessions: usize },
}

/// Routes verified forge deliveries to OpenClaw after deterministic gating.
pub struct Router<F, G> {
    forge: F,
    gateway: G,
    forge_name: String,
    rules: RwLock<Vec<TriggerRule>>,
    grants: Arc<GrantStore>,
    grant_ttl: Duration,
    turn_locks: Mutex<HashMap<SessionKey, Weak<Mutex<()>>>>,
}

impl<F, G> Router<F, G>
where
    F: WebhookForge,
    G: Gateway,
{
    pub fn new(
        forge: F,
        gateway: G,
        forge_name: impl Into<String>,
        rules: Vec<TriggerRule>,
        grants: Arc<GrantStore>,
        grant_ttl: Duration,
    ) -> Self {
        Self {
            forge,
            gateway,
            forge_name: forge_name.into(),
            rules: RwLock::new(rules),
            grants,
            grant_ttl,
            turn_locks: Mutex::new(HashMap::new()),
        }
    }

    pub async fn deliver(&self, events: Vec<ForgeEvent>) -> Result<Routed> {
        let bot_user = self.forge.whoami().await?;
        let mut turns: Vec<(ThreadKey, Vec<String>)> = Vec::new();
        for event in events {
            let matched = self
                .rules
                .read()
                .await
                .iter()
                .any(|rule| rule.matches(&event, &bot_user));
            if !matched {
                continue;
            }
            let thread = event.thread();
            if let Some((_, kinds)) = turns.iter_mut().find(|(key, _)| *key == thread) {
                if !kinds.contains(&event.kind) {
                    kinds.push(event.kind);
                }
            } else {
                turns.push((thread, vec![event.kind]));
            }
        }
        let sessions = turns.len();
        for (thread, kinds) in turns {
            self.run_turn(thread, kinds).await?;
        }
        Ok(if sessions == 0 {
            Routed::Ignored
        } else {
            Routed::Engaged { sessions }
        })
    }

    async fn run_turn(&self, thread: ThreadKey, kinds: Vec<String>) -> Result<()> {
        let session_key = session_key(&self.forge_name, &thread.repo, &thread);
        let key = SessionKey::new(session_key.clone());
        let turn_lock = {
            let mut locks = self.turn_locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(key.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        let _turn = turn_lock.lock().await;
        let token = self.forge.mint_token(&session_key).await?;
        self.grants
            .insert(key.clone(), thread.clone(), token, self.grant_ttl);
        let message = format!(
            "Triggered forge event(s) {} on {}. This is a forge-triggered turn, not an ad-hoc \
             question. Act now according to the forgeclaw skill using the forge tools. Read the \
             exact subject first. For a mentioned question, post exactly one direct answer on \
             that subject. For requested code work, make the change, push a branch, open a pull \
             request, then post exactly one informative subject comment. For a review request, \
             inspect the diff and submit the review. For failed CI or requested changes, fix and \
             push the existing branch only when forge_read says its head_owner is your forge \
             username; never open a replacement PR. Do not only describe what you would do.",
            kinds.join(", "),
            thread
        );
        let submit = self.gateway.submit(&session_key, &message).await;
        let grant = self
            .grants
            .take(&key)
            .expect("router inserted the active grant for this session");
        let revoke = self.forge.revoke_token(grant.token()).await;
        submit?;
        revoke
    }

    pub fn grants(&self) -> &Arc<GrantStore> {
        &self.grants
    }

    pub async fn replace_rules(&self, rules: Vec<TriggerRule>) {
        *self.rules.write().await = rules;
    }
}

/// OpenClaw requires the `agent:<id>:` prefix; the rest is a stable forge
/// thread identity, so a later event resumes this same agent session.
pub fn session_key(forge: &str, repo: &RepoId, thread: &ThreadKey) -> String {
    format!("agent:main:forgeclaw:{forge}/{repo}#{}", thread.subject)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use forgeclaw_core::Subject;
    use serde_json::json;

    fn event(payload: Value) -> ForgeEvent {
        ForgeEvent {
            repo: "octo/repo".parse().unwrap(),
            kind: "comment.created".into(),
            subject: Subject::Issue(7),
            payload,
        }
    }

    #[test]
    fn mention_rule_handles_arrays_and_negation() {
        let rule = TriggerRule {
            on: "comment.created".into(),
            enabled: true,
            filter: TriggerFilter::One(BTreeMap::from([
                ("mentions".into(), "@me".into()),
                ("author".into(), "!@me".into()),
            ])),
        };
        assert!(rule.matches(
            &event(json!({"mentions": ["forgeclaw"], "author": "alice"})),
            "forgeclaw"
        ));
        assert!(!rule.matches(
            &event(json!({"mentions": ["forgeclaw"], "author": "forgeclaw"})),
            "forgeclaw"
        ));
    }

    #[test]
    fn legacy_assignee_filter_matches_normalized_assignees() {
        let rule = TriggerRule {
            on: "issue.assigned".into(),
            enabled: true,
            filter: TriggerFilter::One(BTreeMap::from([("assignee".into(), "@me".into())])),
        };
        let mut event = event(json!({"assignees": ["forgeclaw"]}));
        event.kind = "issue.assigned".into();

        TriggerRule::validate_all(std::slice::from_ref(&rule)).unwrap();
        assert!(rule.matches(&event, "forgeclaw"));
    }

    #[test]
    fn alternative_filter_groups_and_enabled_state_are_preserved() {
        let rule: TriggerRule = serde_json::from_value(json!({
            "on": "comment.created",
            "filter": [
                {"mentions": "@me", "author": "!@me"},
                {"assignees": "@me", "author": "!@me"}
            ]
        }))
        .unwrap();
        assert!(rule.enabled);
        assert!(rule.matches(
            &event(json!({"assignees": ["forgeclaw"], "author": "alice"})),
            "forgeclaw"
        ));

        let disabled: TriggerRule = serde_json::from_value(json!({
            "on": "comment.created",
            "enabled": false
        }))
        .unwrap();
        assert!(!disabled.matches(&event(json!({})), "forgeclaw"));
    }

    #[test]
    fn trigger_validation_rejects_names_the_normalizer_cannot_emit() {
        let unknown_event: TriggerRule =
            serde_json::from_value(json!({"on": "issue.closed"})).unwrap();
        assert!(TriggerRule::validate_all(&[unknown_event]).is_err());

        let unknown_field: TriggerRule = serde_json::from_value(json!({
            "on": "comment.created",
            "filter": {"typo": "@me"}
        }))
        .unwrap();
        assert!(TriggerRule::validate_all(&[unknown_field]).is_err());

        let empty_group: TriggerRule = serde_json::from_value(json!({
            "on": "comment.created",
            "filter": {}
        }))
        .unwrap();
        assert!(TriggerRule::validate_all(&[empty_group]).is_err());
    }

    #[test]
    fn session_key_is_stable_and_names_the_exact_subject() {
        let thread = ThreadKey {
            repo: "octo/repo".parse().unwrap(),
            subject: Subject::Issue(7),
        };
        assert_eq!(
            session_key("forgejo", &thread.repo, &thread),
            "agent:main:forgeclaw:forgejo/octo/repo#issue/7"
        );
    }

    #[derive(Clone, Default)]
    struct FakeForge {
        minted: Arc<AtomicUsize>,
        revoked: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl WebhookForge for FakeForge {
        async fn whoami(&self) -> Result<String> {
            Ok("forgeclaw".into())
        }

        async fn mint_token(&self, _label: &str) -> Result<ScopedToken> {
            self.minted.fetch_add(1, Ordering::SeqCst);
            Ok(ScopedToken {
                id: 1,
                secret: "disposable".into(),
            })
        }

        async fn revoke_token(&self, _token: &ScopedToken) -> Result<()> {
            self.revoked.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeGateway {
        submitted: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Gateway for FakeGateway {
        async fn submit(&self, _session_key: &str, _message: &str) -> Result<()> {
            self.submitted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct ExpiringGateway {
        grants: Arc<GrantStore>,
        thread: ThreadKey,
    }

    #[async_trait]
    impl Gateway for ExpiringGateway {
        async fn submit(&self, session_key: &str, _message: &str) -> Result<()> {
            assert!(
                !self
                    .grants
                    .can_write(&SessionKey::new(session_key), &self.thread),
                "the zero-TTL grant must not authorize the turn"
            );
            Ok(())
        }
    }

    #[tokio::test]
    async fn successful_event_is_submitted_and_grant_is_removed() {
        let forge = FakeForge::default();
        let minted = forge.minted.clone();
        let revoked = forge.revoked.clone();
        let gateway = FakeGateway::default();
        let submitted = gateway.submitted.clone();
        let router = Router::new(
            forge,
            gateway,
            "forgejo",
            vec![TriggerRule {
                on: "comment.created".into(),
                enabled: true,
                filter: TriggerFilter::One(BTreeMap::from([("mentions".into(), "@me".into())])),
            }],
            Arc::new(GrantStore::default()),
            Duration::from_secs(60),
        );

        assert_eq!(
            router
                .deliver(vec![event(json!({"mentions": ["forgeclaw"]}))])
                .await
                .unwrap(),
            Routed::Engaged { sessions: 1 }
        );
        assert_eq!(submitted.load(Ordering::SeqCst), 1);
        assert_eq!(minted.load(Ordering::SeqCst), 1);
        assert_eq!(revoked.load(Ordering::SeqCst), 1);
        assert!(!router.grants().can_write(
            &SessionKey::new("agent:main:forgeclaw:forgejo/octo/repo#issue/7"),
            &event(json!({})).thread(),
        ));
    }

    #[tokio::test]
    async fn an_expired_grant_is_removed_without_panicking() {
        let grants = Arc::new(GrantStore::default());
        let current = event(json!({"mentions": ["forgeclaw"]}));
        let router = Router::new(
            FakeForge::default(),
            ExpiringGateway {
                grants: grants.clone(),
                thread: current.thread(),
            },
            "forgejo",
            vec![TriggerRule {
                on: "comment.created".into(),
                enabled: true,
                filter: TriggerFilter::One(BTreeMap::from([("mentions".into(), "@me".into())])),
            }],
            grants,
            Duration::ZERO,
        );

        assert_eq!(
            router.deliver(vec![current]).await.unwrap(),
            Routed::Engaged { sessions: 1 }
        );
    }

    #[tokio::test]
    async fn matching_events_for_one_thread_start_one_turn() {
        let forge = FakeForge::default();
        let minted = forge.minted.clone();
        let gateway = FakeGateway::default();
        let submitted = gateway.submitted.clone();
        let router = Router::new(
            forge,
            gateway,
            "forgejo",
            vec![
                TriggerRule {
                    on: "pull_request.opened".into(),
                    enabled: true,
                    filter: TriggerFilter::Any,
                },
                TriggerRule {
                    on: "comment.created".into(),
                    enabled: true,
                    filter: TriggerFilter::One(BTreeMap::from([("mentions".into(), "@me".into())])),
                },
            ],
            Arc::new(GrantStore::default()),
            Duration::from_secs(60),
        );
        let mut opened = event(json!({"author": "alice"}));
        opened.kind = "pull_request.opened".into();
        opened.subject = Subject::Pr(7);
        let mut mentioned = event(json!({"mentions": ["forgeclaw"]}));
        mentioned.subject = Subject::Pr(7);

        assert_eq!(
            router.deliver(vec![opened, mentioned]).await.unwrap(),
            Routed::Engaged { sessions: 1 }
        );
        assert_eq!(submitted.load(Ordering::SeqCst), 1);
        assert_eq!(minted.load(Ordering::SeqCst), 1);
    }
}
