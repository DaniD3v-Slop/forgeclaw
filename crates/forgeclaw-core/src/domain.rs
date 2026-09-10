use std::fmt;

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
                && p != "."
                && p != ".."
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

/// One normalized webhook event — the only input to the trigger pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeEvent {
    pub repo: RepoId,
    /// Dotted kind, e.g. `issue.assigned`.
    pub kind: String,
    pub subject: Subject,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_id_rejects_metacharacters() {
        for bad in [
            "a b/c", "a/$(x)", "a/", "/c", "a/c;d", "ä/c", "../repo", "./repo",
        ] {
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
}
