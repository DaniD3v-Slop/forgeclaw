use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A repository on the forge, `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoId {
    pub owner: String,
    pub name: String,
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl std::str::FromStr for RepoId {
    type Err = crate::Error;

    fn from_str(s: &str) -> crate::Result<Self> {
        let (owner, name) = s
            .split_once('/')
            .ok_or_else(|| crate::Error::Forge(format!("repo id without owner: {s}")))?;
        // Forgejo restricts names to this set; enforcing it here keeps webhook
        // payloads from smuggling shell/URL metacharacters downstream (clone
        // command, volume names, MCP subject).
        let valid = |p: &str| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        if !valid(owner) || !valid(name) {
            return Err(crate::Error::Forge(format!("invalid repo id: {s}")));
        }
        Ok(Self {
            owner: owner.into(),
            name: name.into(),
        })
    }
}

/// What a conversation hangs off. Review threads belong to their PR's thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Subject {
    Issue(u64),
    Pr(u64),
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Subject::Issue(n) => write!(f, "issue/{n}"),
            Subject::Pr(n) => write!(f, "pr/{n}"),
        }
    }
}

/// One subject in one repo; owns at most one agent session and one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    pub repo: RepoId,
    pub subject: Subject,
}

impl fmt::Display for ThreadKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.subject)
    }
}

impl std::str::FromStr for ThreadKey {
    type Err = crate::Error;

    /// Inverse of [`Display`](fmt::Display) — the store round-trips thread keys
    /// through their display string.
    fn from_str(s: &str) -> crate::Result<Self> {
        let bad = || crate::Error::Forge(format!("invalid thread key: {s}"));
        let (repo, subject) = s.split_once('#').ok_or_else(bad)?;
        let (kind, num) = subject.split_once('/').ok_or_else(bad)?;
        let n = num.parse().map_err(|_| bad())?;
        let subject = match kind {
            "issue" => Subject::Issue(n),
            "pr" => Subject::Pr(n),
            _ => return Err(bad()),
        };
        Ok(Self {
            repo: repo.parse()?,
            subject,
        })
    }
}

/// One normalized event, from webhook or resync alike — the only input to
/// the trigger pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeEvent {
    pub repo: RepoId,
    /// Dotted kind, e.g. `issue.assigned`.
    pub kind: String,
    pub subject: Subject,
    pub actor: String,
    /// Forge-side id; makes `(trigger, thread, event_id)` idempotent when
    /// webhook and resync race.
    pub event_id: String,
    /// Flat fields that filters match on and templates render with.
    pub payload: Value,
}

impl ForgeEvent {
    pub fn thread(&self) -> ThreadKey {
        ThreadKey {
            repo: self.repo.clone(),
            subject: self.subject,
        }
    }
}

/// One AND-clause: every field pattern must match. `@me` expands to the bot
/// user, a `!` prefix negates, array fields match if any element does.
type Clause = BTreeMap<String, String>;

/// A trigger's `filter`, accepting both TOML shapes: a single inline table
/// (one AND-clause) or an array of tables (OR over clauses). Absent filter
/// matches every event of the trigger's kind.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(untagged)]
enum Filter {
    #[default]
    Any,
    One(Clause),
    Or(Vec<Clause>),
}

impl Filter {
    fn matches(&self, event: &ForgeEvent, me: &str) -> bool {
        let clause_matches = |clause: &Clause| {
            clause.iter().all(|(field, pat)| {
                let (want, pat) = match pat.strip_prefix('!') {
                    Some(rest) => (false, rest),
                    None => (true, pat.as_str()),
                };
                let pat = if pat == "@me" { me } else { pat };
                let hit = match event.payload.get(field) {
                    Some(Value::Array(xs)) => xs.iter().any(|x| value_eq(x, pat)),
                    Some(v) => value_eq(v, pat),
                    None => false,
                };
                hit == want
            })
        };
        match self {
            Filter::Any => true,
            Filter::One(clause) => clause_matches(clause),
            Filter::Or(clauses) => clauses.iter().any(clause_matches),
        }
    }
}

/// Pure routing config: match an event, name a prompt. What happens next is
/// the agent's decision.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    pub on: String,
    #[serde(default)]
    filter: Filter,
    pub prompt: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
}

fn enabled() -> bool {
    true
}

impl Trigger {
    /// Matching ignores `enabled`: tasks are created even for disabled
    /// triggers so a repo's `.forgebot.toml` can enable them — delivery
    /// re-checks `enabled` under the repo layer and no-ops when still off.
    pub fn matches(&self, event: &ForgeEvent, me: &str) -> bool {
        self.on == event.kind && self.filter.matches(event, me)
    }
}

fn value_eq(v: &Value, pat: &str) -> bool {
    match v {
        Value::String(s) => s == pat,
        Value::Bool(b) => pat.parse().ok() == Some(*b),
        Value::Number(n) => pat.parse::<serde_json::Number>().ok().as_ref() == Some(n),
        _ => false,
    }
}

/// Task lifecycle is retry machinery only — no domain states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Done,
    Failed,
    /// Failure limit reached; a templated notice was posted.
    Abandoned,
}

/// One delivery attempt unit: get this event's rendered prompt in front of
/// the thread's agent session.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: i64,
    /// Trigger name that matched.
    pub trigger: String,
    pub event: ForgeEvent,
    pub status: TaskStatus,
    pub attempts: u32,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl Task {
    pub fn new(trigger: &str, event: ForgeEvent) -> Self {
        Self {
            id: 0,
            trigger: trigger.into(),
            event,
            status: TaskStatus::Queued,
            attempts: 0,
            next_retry_at: None,
            last_error: None,
        }
    }

    /// Webhook and resync racing on the same event must produce one task.
    pub fn idem_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.trigger,
            self.event.thread(),
            self.event.event_id
        )
    }
}

/// Exponential backoff for failed attempts: 1m, 4m, 16m … capped at 1h.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(60u64.saturating_mul(4u64.saturating_pow(attempt)).min(3600))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repo_id_rejects_metacharacters() {
        for bad in ["a b/c", "a/$(x)", "a/", "/c", "a/c;d", "ä/c"] {
            assert!(bad.parse::<RepoId>().is_err(), "{bad} must be rejected");
        }
        assert!("some-org.x/repo_1".parse::<RepoId>().is_ok());
    }

    #[test]
    fn thread_key_display_from_str_round_trip() {
        for subject in [Subject::Issue(7), Subject::Pr(12)] {
            let key = ThreadKey {
                repo: RepoId {
                    owner: "o".into(),
                    name: "r".into(),
                },
                subject,
            };
            assert_eq!(key.to_string().parse::<ThreadKey>().unwrap(), key);
        }
        for bad in ["o/r", "o/r#issue", "o/r#issue/x", "o/r#task/3", "#issue/3"] {
            assert!(bad.parse::<ThreadKey>().is_err(), "{bad} must be rejected");
        }
    }

    fn event(kind: &str, payload: Value) -> ForgeEvent {
        ForgeEvent {
            repo: RepoId {
                owner: "o".into(),
                name: "r".into(),
            },
            kind: kind.into(),
            subject: Subject::Issue(1),
            actor: "alice".into(),
            event_id: "e1".into(),
            payload,
        }
    }

    fn clause(fields: &[(&str, &str)]) -> Clause {
        fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn trigger(on: &str, filter: &[(&str, &str)]) -> Trigger {
        Trigger {
            on: on.into(),
            filter: Filter::One(clause(filter)),
            prompt: "p".into(),
            enabled: true,
        }
    }

    #[test]
    fn trigger_kind_and_filters() {
        let t = trigger("issue.assigned", &[("assignee", "@me")]);
        assert!(t.matches(&event("issue.assigned", json!({"assignee": "bot"})), "bot"));
        assert!(!t.matches(&event("issue.closed", json!({"assignee": "bot"})), "bot"));
        assert!(!t.matches(&event("issue.assigned", json!({"assignee": "eve"})), "bot"));
    }

    #[test]
    fn negation_and_arrays() {
        let t = trigger(
            "comment.created",
            &[("mentions", "@me"), ("author", "!@me")],
        );
        let m = |payload| t.matches(&event("comment.created", payload), "bot");
        assert!(m(json!({"mentions": ["x", "bot"], "author": "alice"})));
        assert!(
            !m(json!({"mentions": ["bot"], "author": "bot"})),
            "must not answer itself"
        );
        assert!(!m(json!({"mentions": ["x"], "author": "alice"})));
    }

    #[test]
    fn or_clauses_match_if_any_clause_does() {
        let t = Trigger {
            on: "comment.created".into(),
            filter: Filter::Or(vec![
                clause(&[("mentions", "@me"), ("author", "!@me")]),
                clause(&[("assignees", "@me"), ("author", "!@me")]),
            ]),
            prompt: "p".into(),
            enabled: true,
        };
        let m = |payload| t.matches(&event("comment.created", payload), "bot");
        // Matches via the assignees clause with no mention present.
        assert!(m(
            json!({"assignees": ["bot"], "author": "alice", "mentions": ["x"]})
        ));
        // Matches via the mentions clause with no assignees present.
        assert!(m(json!({"mentions": ["bot"], "author": "alice"})));
        // The bot's own comment is excluded by `author = "!@me"` in both clauses.
        assert!(!m(json!({"assignees": ["bot"], "author": "bot"})));
        assert!(!m(json!({"mentions": ["bot"], "author": "bot"})));
        // Neither clause matches.
        assert!(!m(
            json!({"assignees": ["x"], "mentions": ["y"], "author": "alice"})
        ));
    }

    #[test]
    fn absent_filter_matches_every_event_of_the_kind() {
        let t = Trigger {
            on: "a.b".into(),
            filter: Filter::Any,
            prompt: "p".into(),
            enabled: true,
        };
        assert!(t.matches(&event("a.b", json!({})), "bot"));
        assert!(!t.matches(&event("x.y", json!({})), "bot"));
    }

    #[test]
    fn missing_field_matches_only_negated() {
        assert!(trigger("a.b", &[("author", "!@me")]).matches(&event("a.b", json!({})), "bot"));
        assert!(!trigger("a.b", &[("author", "@me")]).matches(&event("a.b", json!({})), "bot"));
    }

    #[test]
    fn non_string_values_match_parsed_patterns() {
        let t = trigger(
            "ci.run_completed",
            &[("conclusion", "failure"), ("run", "42")],
        );
        assert!(t.matches(
            &event(
                "ci.run_completed",
                json!({"conclusion": "failure", "run": 42})
            ),
            "bot"
        ));
    }

    #[test]
    fn disabled_still_matches_so_a_repo_layer_can_enable_it() {
        let mut t = trigger("a.b", &[]);
        t.enabled = false;
        assert!(t.matches(&event("a.b", json!({})), "bot"));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff(0), Duration::from_secs(60));
        assert_eq!(backoff(1), Duration::from_secs(240));
        assert_eq!(backoff(3), Duration::from_secs(3600));
        assert_eq!(backoff(30), Duration::from_secs(3600), "overflow-safe");
    }

    #[test]
    fn idem_key_covers_trigger_thread_event() {
        let t = Task::new("implement-issue", event("issue.assigned", json!({})));
        assert_eq!(t.idem_key(), "implement-issue:o/r#issue/1:e1");
    }
}
