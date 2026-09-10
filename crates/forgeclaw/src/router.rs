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
    pub fn matches(&self, event: &ForgeEvent, bot_user: &str) -> bool {
        let matches_clause = |clause: &BTreeMap<String, String>| {
            clause.iter().all(|(field, pattern)| {
                let (wanted, pattern) = match pattern.strip_prefix('!') {
                    Some(pattern) => (false, pattern),
                    None => (true, pattern.as_str()),
                };
                let pattern = if pattern == "@me" { bot_user } else { pattern };
                let matched = event
                    .payload
                    .get(field)
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
}

impl OpenClawCli {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    async fn run(&self, session_key: &str, message: &str) -> Result<()> {
        let status = tokio::process::Command::new(&self.program)
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
            .stdout(Stdio::null())
            .status()
            .await?;
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
        let mut sessions = 0;
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
            self.run_event(&event).await?;
            sessions += 1;
        }
        Ok(if sessions == 0 {
            Routed::Ignored
        } else {
            Routed::Engaged { sessions }
        })
    }

    async fn run_event(&self, event: &ForgeEvent) -> Result<()> {
        let session_key = session_key(&self.forge_name, &event.repo, &event.thread());
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
        self.grants
            .insert(key.clone(), event.thread(), self.grant_ttl);
        let message = format!(
            "Triggered forge event {} on {}. This is a forge-triggered turn, not an ad-hoc \
             question. Act now according to the forgeclaw skill using the forge tools. Read the \
             exact subject first. For a mentioned question, post exactly one direct answer on \
             that subject. For requested code work, make the change, push a branch, open a pull \
             request, then post exactly one informative subject comment. For a review request, \
             inspect the diff and submit the review. For failed CI or requested changes, fix and \
             push the existing branch only when forge_read says its head_owner is your forge \
             username; never open a replacement PR. Do not only describe what you would do.",
            event.kind,
            event.thread()
        );
        let submit = self.gateway.submit(&session_key, &message).await;
        self.grants.remove(&key);
        let revoke = self.forge.revoke_token(&token).await;
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

    #[derive(Clone)]
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
                secret: "scoped".into(),
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
                "the zero-TTL grant is expired and removed during the turn"
            );
            Ok(())
        }
    }

    #[tokio::test]
    async fn successful_event_is_submitted_and_revoked() {
        let minted = Arc::new(AtomicUsize::new(0));
        let revoked = Arc::new(AtomicUsize::new(0));
        let forge = FakeForge {
            minted: minted.clone(),
            revoked: revoked.clone(),
        };
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
        for counter in [submitted, minted, revoked] {
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        assert!(
            router
                .grants()
                .authorized_token(
                    &SessionKey::new("agent:main:forgeclaw:forgejo/octo/repo#issue/7"),
                    &event(json!({})).thread(),
                )
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_expired_grant_is_removed_without_panicking() {
        let grants = Arc::new(GrantStore::default());
        let current = event(json!({"mentions": ["forgeclaw"]}));
        let router = Router::new(
            FakeForge,
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
}
