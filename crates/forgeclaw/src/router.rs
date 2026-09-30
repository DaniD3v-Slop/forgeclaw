use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use forgeclaw_core::{ForgeEvent, RepoId, Result, ScopedToken, ThreadKey};
use serde::Deserialize;
use serde_json::{Map, Value, json};
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
                "comment.created" => &["mentions", "assignees", "head_owner", "author", "body"][..],
                // `assignee` was emitted by an older editor. Keep it as an
                // alias so persisted configurations continue to work.
                "issue.assigned" => &["assignees", "assignee", "author"],
                "pull_request.review_requested" => &["reviewer", "author"],
                "pull_request.changes_requested" => &["reviewer", "body"],
                "pull_request.review_commented" => &["reviewer", "pr_author", "body"],
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
    async fn add_reaction(
        &self,
        token: &ScopedToken,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()>;
    async fn remove_reaction(
        &self,
        token: &ScopedToken,
        thread: &ThreadKey,
        comment_id: Option<u64>,
        emoji: &str,
    ) -> Result<()>;
}

const RUNNING_REACTION: &str = "🧑‍🍳";

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
        self.group_session(session_key).await?;
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

    async fn group_session(&self, session_key: &str) -> Result<()> {
        self.session_call(
            "sessions.create",
            json!({ "key": session_key, "category": "ForgeClaw" }),
        )
        .await?;
        self.session_call("sessions.patch", session_group_params(session_key))
            .await
    }

    async fn session_call(&self, method: &str, params: Value) -> Result<()> {
        let mut command = tokio::process::Command::new(&self.program);
        command.args([
            "gateway",
            "call",
            method,
            "--params",
            &params.to_string(),
            "--json",
        ]);
        for name in &self.secret_envs {
            command.env_remove(name);
        }
        let output = command.output().await?;
        let response: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
            forgeclaw_core::Error::Forge(format!("OpenClaw {method} returned invalid JSON"))
        })?;
        if !output.status.success() || response["ok"] == false {
            return Err(forgeclaw_core::Error::Forge(format!(
                "OpenClaw {method} rejected session request"
            )));
        }
        Ok(())
    }
}

fn session_group_params(session_key: &str) -> Value {
    json!({
        "key": session_key,
        "category": "ForgeClaw",
    })
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
    ) -> Self {
        Self {
            forge,
            gateway,
            forge_name: forge_name.into(),
            rules: RwLock::new(rules),
            grants,
            turn_locks: Mutex::new(HashMap::new()),
        }
    }

    pub async fn deliver(&self, events: Vec<ForgeEvent>) -> Result<Routed> {
        self.deliver_with(events, |_| Ok(())).await
    }

    pub async fn deliver_with(
        &self,
        events: Vec<ForgeEvent>,
        mut completed: impl FnMut(&ThreadKey) -> Result<()>,
    ) -> Result<Routed> {
        let bot_user = self.forge.whoami().await?;
        let mut turns: Vec<(ThreadKey, Vec<ForgeEvent>)> = Vec::new();
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
            if let Some((_, matching)) = turns.iter_mut().find(|(key, _)| *key == thread) {
                matching.push(event);
            } else {
                turns.push((thread, vec![event]));
            }
        }
        let sessions = turns.len();
        for (thread, events) in turns {
            self.run_turn(thread.clone(), events, &bot_user).await?;
            completed(&thread)?;
        }
        Ok(if sessions == 0 {
            Routed::Ignored
        } else {
            Routed::Engaged { sessions }
        })
    }

    pub async fn preview(&self, rules: &[TriggerRule], event: &ForgeEvent) -> Result<Vec<usize>> {
        TriggerRule::validate_all(rules)?;
        let bot_user = self.forge.whoami().await?;
        Ok(rules
            .iter()
            .enumerate()
            .filter_map(|(index, rule)| rule.matches(event, &bot_user).then_some(index))
            .collect())
    }

    async fn run_turn(
        &self,
        thread: ThreadKey,
        events: Vec<ForgeEvent>,
        bot_user: &str,
    ) -> Result<()> {
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
        // A previous process may have died before revoking its token for this
        // session. Forgejo requires token names to be unique per user.
        let token = self
            .forge
            .mint_token(&format!("forgeclaw-temp-turn-{}", uuid::Uuid::new_v4()))
            .await?;
        let grant_lease = self
            .grants
            .insert(key.clone(), thread.clone(), token.clone());
        let comment_id = reaction_comment_id(&events);
        if let Err(error) = self
            .forge
            .add_reaction(&token, &thread, comment_id, RUNNING_REACTION)
            .await
        {
            eprintln!("could not mark {thread} as running: {error}");
        }
        let message = trigger_message(&thread, bot_user, &events);
        let submit = self.gateway.submit(&session_key, &message).await;
        drop(grant_lease);
        if submit.is_ok() {
            if let Err(error) = self
                .forge
                .remove_reaction(&token, &thread, comment_id, RUNNING_REACTION)
                .await
            {
                eprintln!("could not clear running reaction on {thread}: {error}");
            }
        }
        let revoke = self.forge.revoke_token(&token).await;
        submit?;
        // A failed cleanup must not replay a turn that OpenClaw already ran.
        if let Err(error) = revoke {
            eprintln!("could not revoke completed turn token: {error}");
        }
        Ok(())
    }

    pub fn grants(&self) -> &Arc<GrantStore> {
        &self.grants
    }

    pub async fn replace_rules(&self, rules: Vec<TriggerRule>) {
        *self.rules.write().await = rules;
    }
}

fn reaction_comment_id(events: &[ForgeEvent]) -> Option<u64> {
    events.iter().find_map(|event| {
        (event.kind == "comment.created" && event.payload["source"] == "comment")
            .then(|| event.payload["comment_id"].as_u64())
            .flatten()
    })
}

fn trigger_message(thread: &ThreadKey, bot_user: &str, events: &[ForgeEvent]) -> String {
    let snapshot: Vec<Value> = events
        .iter()
        .filter(|event| {
            !(event.kind == "comment.created"
                && event.payload["source"] == "review"
                && !event.payload["review_id"].is_null()
                && events.iter().any(|other| {
                    other.kind == "pull_request.review_commented"
                        && other.payload["review_id"] == event.payload["review_id"]
                }))
        })
        .map(|event| {
            let mut fields = Map::new();
            fields.insert("kind".into(), event.kind.clone().into());
            for key in [
                "source",
                "author",
                "reviewer",
                "pr_author",
                "title",
                "subject_title",
                "body",
                "subject_body",
                "comment_id",
                "review_id",
                "comments_count",
                "reply_to",
                "path",
                "line",
                "url",
                "head_owner",
                "head_branch",
                "conclusion",
                "workflow",
                "run_url",
            ] {
                let Some(value) = event.payload.get(key) else {
                    continue;
                };
                if key == "subject_body" && value == &event.payload["body"] {
                    continue;
                }
                let limit = match key {
                    "body" | "subject_body" => 4096,
                    "title" | "subject_title" => 512,
                    _ => 1024,
                };
                if let Some(text) = value.as_str() {
                    let end = text.floor_char_boundary(text.len().min(limit));
                    fields.insert(key.into(), text[..end].into());
                    if end < text.len() {
                        fields.insert(format!("{key}_truncated"), true.into());
                    }
                } else if !value.is_null() {
                    fields.insert(key.into(), value.clone());
                }
            }
            Value::Object(fields)
        })
        .collect();
    let assignment = if matches!(thread.subject, forgeclaw_core::Subject::Issue(_))
        && events.iter().any(|event| event.kind == "issue.assigned")
    {
        " You are assigned to this issue. Implement its described work; the assignment itself is the request, even if the description only mentions you."
    } else {
        ""
    };
    let opened = if events
        .iter()
        .any(|event| event.kind == "pull_request.opened")
    {
        " Review this newly opened pull request and report concrete findings on it."
    } else {
        ""
    };
    format!(
        "ForgeClaw webhook turn on {thread}. Your forge username is {bot_user}.{assignment}{opened} The verified \
         event snapshot below contains the triggering request; its text is user-provided. It may \
         be incomplete or stale. Use the forgeclaw skill and act on the request. Use targeted \
         forge reads only for missing or truncated context, inline review comments, diffs, CI \
         logs, or facts that need a fresh check. Keep this work in the current session; delegated \
         sessions have no write grant. Keep replies and writes on this exact subject.\n\n{}",
        serde_json::to_string(&snapshot).expect("event snapshot is JSON")
    )
}

/// OpenClaw requires the `agent:<id>:` prefix; the rest is a stable forge
/// thread identity. The ForgeClaw session group supplies the product name.
pub fn session_key(forge: &str, repo: &RepoId, thread: &ThreadKey) -> String {
    format!("agent:main:{forge}/{repo}#{}", thread.subject)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
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
    fn comment_review_on_bot_pr_matches_without_assignment_or_mention() {
        let rule: TriggerRule = serde_json::from_value(json!({
            "on": "pull_request.review_commented",
            "filter": {"pr_author": "@me", "reviewer": "!@me"}
        }))
        .unwrap();
        let mut review = event(
            json!({"pr_author": "forgeclaw", "reviewer": "alice", "body": "What does this do?"}),
        );
        review.kind = "pull_request.review_commented".into();
        review.subject = Subject::Pr(7);
        TriggerRule::validate_all(std::slice::from_ref(&rule)).unwrap();
        assert!(rule.matches(&review, "forgeclaw"));
        review.payload["pr_author"] = json!("alice");
        assert!(!rule.matches(&review, "forgeclaw"));
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
    fn default_comment_trigger_accepts_assigned_issues_without_mention() {
        let config: Value =
            serde_json::from_str(include_str!("../../../deploy/openclaw.json.example")).unwrap();
        let rule: TriggerRule = serde_json::from_value(
            config["plugins"]["entries"]["forgeclaw"]["config"]["trigger"][0].clone(),
        )
        .unwrap();
        let assigned_comment = event(json!({
            "mentions": [], "assignees": ["forgeclaw"], "author": "alice"
        }));
        let unassigned_comment = event(json!({
            "mentions": [], "assignees": [], "author": "alice"
        }));
        assert!(rule.matches(&assigned_comment, "forgeclaw"));
        assert!(!rule.matches(&unassigned_comment, "forgeclaw"));
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
            "agent:main:forgejo/octo/repo#issue/7"
        );
    }

    #[test]
    fn forge_sessions_are_grouped_with_structured_json() {
        let params = session_group_params("agent:main:forgejo/o/r#issue/7");
        assert_eq!(params["key"], "agent:main:forgejo/o/r#issue/7");
        assert_eq!(params["category"], "ForgeClaw");
    }

    #[test]
    fn trigger_snapshot_bounds_long_text_but_keeps_review_routing_details() {
        let thread = ThreadKey {
            repo: "o/r".parse().unwrap(),
            subject: Subject::Pr(7),
        };
        let event = ForgeEvent {
            repo: thread.repo.clone(),
            kind: "pull_request.changes_requested".into(),
            subject: thread.subject,
            payload: json!({
                "body": "é".repeat(3000),
                "review_id": 12,
                "head_owner": "forgeclaw",
                "head_branch": "fix-review",
                "unneeded": "x".repeat(3000)
            }),
        };
        let message = trigger_message(&thread, "forgeclaw", &[event]);
        assert!(message.contains("\"review_id\":12"));
        assert!(message.contains("\"head_branch\":\"fix-review\""));
        assert!(message.contains("\"body_truncated\":true"));
        assert!(!message.contains("unneeded"));
        assert!(message.len() < 6000);
    }

    #[test]
    fn issue_assignment_prompt_requests_implementation() {
        let thread = ThreadKey {
            repo: "o/r".parse().unwrap(),
            subject: Subject::Issue(7),
        };
        let assignment = ForgeEvent {
            repo: thread.repo.clone(),
            kind: "issue.assigned".into(),
            subject: thread.subject,
            payload: json!({"title": "Add export", "body": "@forgeclaw"}),
        };
        let message = trigger_message(&thread, "forgeclaw", &[assignment]);
        assert!(message.contains("Implement its described work"));
        assert!(message.contains("assignment itself is the request"));
    }

    #[test]
    fn review_snapshot_does_not_repeat_the_same_review_as_a_comment() {
        let thread = ThreadKey {
            repo: "o/r".parse().unwrap(),
            subject: Subject::Pr(7),
        };
        let events = [
            ForgeEvent {
                repo: thread.repo.clone(),
                kind: "pull_request.review_commented".into(),
                subject: thread.subject,
                payload: json!({"review_id": 12, "body": "What does this do?"}),
            },
            ForgeEvent {
                repo: thread.repo.clone(),
                kind: "comment.created".into(),
                subject: thread.subject,
                payload: json!({"source": "review", "review_id": 12, "body": "What does this do?"}),
            },
        ];
        let message = trigger_message(&thread, "forgeclaw", &events);
        assert_eq!(message.matches("What does this do?").count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_grouping_prevents_the_agent_turn() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("openclaw");
        std::fs::write(
            &program,
            "#!/bin/sh\ncase \"$3\" in\n  sessions.create) echo '{\"ok\":true}';;\n  sessions.patch) echo '{\"ok\":false}';;\n  *) exit 99;;\nesac\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).unwrap();

        let gateway = OpenClawCli::new(program, vec![]);
        let error = gateway
            .run("agent:main:forgejo/o/r#issue/7", "test")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("sessions.patch"));
    }

    type ReactionLog = Arc<Mutex<Vec<(String, Option<u64>, String)>>>;

    #[derive(Clone, Default)]
    struct FakeForge {
        minted: Arc<AtomicUsize>,
        revoked: Arc<AtomicUsize>,
        labels: Arc<Mutex<Vec<String>>>,
        reactions: ReactionLog,
    }

    #[async_trait]
    impl WebhookForge for FakeForge {
        async fn whoami(&self) -> Result<String> {
            Ok("forgeclaw".into())
        }

        async fn mint_token(&self, label: &str) -> Result<ScopedToken> {
            let mut labels = self.labels.lock().await;
            if labels.iter().any(|existing| existing == label) {
                return Err(forgeclaw_core::Error::Forge(
                    "access token name has been used already".into(),
                ));
            }
            labels.push(label.into());
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

        async fn add_reaction(
            &self,
            _token: &ScopedToken,
            thread: &ThreadKey,
            comment_id: Option<u64>,
            emoji: &str,
        ) -> Result<()> {
            self.reactions
                .lock()
                .await
                .push((thread.to_string(), comment_id, format!("+{emoji}")));
            Ok(())
        }

        async fn remove_reaction(
            &self,
            _token: &ScopedToken,
            thread: &ThreadKey,
            comment_id: Option<u64>,
            emoji: &str,
        ) -> Result<()> {
            self.reactions
                .lock()
                .await
                .push((thread.to_string(), comment_id, format!("-{emoji}")));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct FakeGateway {
        submitted: Arc<AtomicUsize>,
        messages: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Gateway for FakeGateway {
        async fn submit(&self, _session_key: &str, message: &str) -> Result<()> {
            self.submitted.fetch_add(1, Ordering::SeqCst);
            self.messages.lock().await.push(message.to_owned());
            Ok(())
        }
    }

    struct WaitingGateway {
        grants: Arc<GrantStore>,
        thread: ThreadKey,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    struct FailingGateway;

    #[async_trait]
    impl Gateway for FailingGateway {
        async fn submit(&self, _session_key: &str, _message: &str) -> Result<()> {
            Err(forgeclaw_core::Error::Forge("agent unavailable".into()))
        }
    }

    #[async_trait]
    impl Gateway for WaitingGateway {
        async fn submit(&self, session_key: &str, _message: &str) -> Result<()> {
            assert!(
                self.grants
                    .can_write(&SessionKey::new(session_key), &self.thread),
                "the active turn must have its write grant"
            );
            self.started.notify_one();
            self.release.notified().await;
            assert!(
                self.grants
                    .can_write(&SessionKey::new(session_key), &self.thread)
            );
            Ok(())
        }
    }

    #[tokio::test]
    async fn successful_event_is_submitted_and_grant_is_removed() {
        let forge = FakeForge::default();
        let minted = forge.minted.clone();
        let revoked = forge.revoked.clone();
        let reactions = forge.reactions.clone();
        let gateway = FakeGateway::default();
        let submitted = gateway.submitted.clone();
        let messages = gateway.messages.clone();
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
        );

        assert_eq!(
            router
                .deliver(vec![event(json!({
                    "mentions": ["forgeclaw"],
                    "source": "description",
                    "subject_title": "Explain the build",
                    "body": "@forgeclaw what does this build do?",
                    "author": "alice"
                }))])
                .await
                .unwrap(),
            Routed::Engaged { sessions: 1 }
        );
        assert_eq!(submitted.load(Ordering::SeqCst), 1);
        let messages = messages.lock().await;
        assert!(messages[0].contains("what does this build do?"));
        assert!(messages[0].contains("Explain the build"));
        assert!(messages[0].contains("forgeclaw"));
        assert_eq!(minted.load(Ordering::SeqCst), 1);
        assert_eq!(revoked.load(Ordering::SeqCst), 1);
        assert_eq!(
            *reactions.lock().await,
            vec![
                ("octo/repo#issue/7".into(), None, "+🧑‍🍳".into()),
                ("octo/repo#issue/7".into(), None, "-🧑‍🍳".into()),
            ]
        );
        assert!(!router.grants().can_write(
            &SessionKey::new("agent:main:forgejo/octo/repo#issue/7"),
            &event(json!({})).thread(),
        ));
    }

    #[tokio::test]
    async fn delivery_stops_after_progress_recording_fails() {
        let gateway = FakeGateway::default();
        let submitted = gateway.submitted.clone();
        let router = Router::new(
            FakeForge::default(),
            gateway,
            "forgejo",
            vec![TriggerRule {
                on: "comment.created".into(),
                enabled: true,
                filter: TriggerFilter::Any,
            }],
            Arc::new(GrantStore::default()),
        );
        let first = event(json!({"body": "first"}));
        let mut second = event(json!({"body": "second"}));
        second.subject = Subject::Issue(8);
        let mut recorded = Vec::new();
        let result = router
            .deliver_with(vec![first, second], |thread| {
                recorded.push(thread.to_string());
                Err(forgeclaw_core::Error::Forge("outbox unavailable".into()))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(recorded, ["octo/repo#issue/7"]);
        assert_eq!(submitted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn another_turn_can_start_after_a_previous_token_name_was_left_behind() {
        let forge = FakeForge::default();
        let labels = forge.labels.clone();
        let session = "agent:main:forgejo/octo/repo#issue/7";
        labels.lock().await.push(session.into());
        let router = Router::new(
            forge,
            FakeGateway::default(),
            "forgejo",
            vec![TriggerRule {
                on: "comment.created".into(),
                enabled: true,
                filter: TriggerFilter::Any,
            }],
            Arc::new(GrantStore::default()),
        );

        for _ in 0..2 {
            assert_eq!(
                router.deliver(vec![event(json!({}))]).await.unwrap(),
                Routed::Engaged { sessions: 1 }
            );
        }
        let labels = labels.lock().await;
        assert_eq!(labels.len(), 3);
        assert_ne!(labels[1], labels[2]);
    }

    #[test]
    fn discussion_comments_receive_reactions_on_the_comment() {
        assert_eq!(
            reaction_comment_id(&[event(json!({"source": "comment", "comment_id": 42}))]),
            Some(42)
        );
        assert_eq!(
            reaction_comment_id(&[event(json!({"source": "description", "comment_id": 42}))]),
            None
        );
    }

    #[tokio::test]
    async fn failed_turn_keeps_running_reaction_for_retry() {
        let forge = FakeForge::default();
        let reactions = forge.reactions.clone();
        let router = Router::new(
            forge,
            FailingGateway,
            "forgejo",
            vec![TriggerRule {
                on: "comment.created".into(),
                enabled: true,
                filter: TriggerFilter::Any,
            }],
            Arc::new(GrantStore::default()),
        );
        assert!(
            router
                .deliver(vec![event(json!({"source": "comment", "comment_id": 42}))])
                .await
                .is_err()
        );
        assert_eq!(
            *reactions.lock().await,
            vec![("octo/repo#issue/7".into(), Some(42), "+🧑‍🍳".into())]
        );
    }

    #[tokio::test]
    async fn grant_stays_active_until_the_turn_finishes() {
        let grants = Arc::new(GrantStore::default());
        let current = event(json!({"mentions": ["forgeclaw"]}));
        let session = SessionKey::new(session_key("forgejo", &current.repo, &current.thread()));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let router = Arc::new(Router::new(
            FakeForge::default(),
            WaitingGateway {
                grants: grants.clone(),
                thread: current.thread(),
                started: started.clone(),
                release: release.clone(),
            },
            "forgejo",
            vec![TriggerRule {
                on: "comment.created".into(),
                enabled: true,
                filter: TriggerFilter::One(BTreeMap::from([("mentions".into(), "@me".into())])),
            }],
            grants.clone(),
        ));

        let task = tokio::spawn(async move { router.deliver(vec![current]).await });
        started.notified().await;
        assert!(grants.can_write(&session, &event(json!({})).thread()));
        release.notify_one();
        assert_eq!(
            task.await.unwrap().unwrap(),
            Routed::Engaged { sessions: 1 }
        );
        assert!(!grants.can_write(&session, &event(json!({})).thread()));
    }

    #[tokio::test]
    async fn cancelling_a_turn_drops_its_grant() {
        let grants = Arc::new(GrantStore::default());
        let current = event(json!({}));
        let thread = current.thread();
        let session = SessionKey::new(session_key("forgejo", &current.repo, &thread));
        let started = Arc::new(tokio::sync::Notify::new());
        let router = Arc::new(Router::new(
            FakeForge::default(),
            WaitingGateway {
                grants: grants.clone(),
                thread: thread.clone(),
                started: started.clone(),
                release: Arc::new(tokio::sync::Notify::new()),
            },
            "forgejo",
            vec![TriggerRule {
                on: "comment.created".into(),
                enabled: true,
                filter: TriggerFilter::Any,
            }],
            grants.clone(),
        ));

        let task = tokio::spawn(async move { router.deliver(vec![current]).await });
        started.notified().await;
        assert!(grants.can_write(&session, &thread));
        task.abort();
        assert!(task.await.is_err());
        assert!(!grants.can_write(&session, &thread));
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
