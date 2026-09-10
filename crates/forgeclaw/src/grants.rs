use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use forgeclaw_core::{ScopedToken, ThreadKey};

/// The OpenClaw session key that the router selected for a forge thread.
///
/// This is an opaque value. It must come from OpenClaw's trusted plugin-tool
/// context, never from a tool argument supplied by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A currently valid authority to mutate one forge thread.
///
pub struct Grant {
    thread: ThreadKey,
    token: ScopedToken,
    expires_at: Instant,
}

impl Grant {
    pub fn token(&self) -> &ScopedToken {
        &self.token
    }
}

/// In-memory authority store shared by the webhook router and tool server.
#[derive(Default)]
pub struct GrantStore {
    grants: Mutex<HashMap<SessionKey, Grant>>,
}

impl GrantStore {
    pub fn insert(
        &self,
        session: SessionKey,
        thread: ThreadKey,
        token: ScopedToken,
        ttl: Duration,
    ) {
        let grant = Grant {
            thread,
            token,
            expires_at: Instant::now() + ttl,
        };
        self.lock().insert(session, grant);
    }

    pub fn take(&self, session: &SessionKey) -> Option<Grant> {
        self.lock().remove(session)
    }

    pub fn can_write(&self, session: &SessionKey, thread: &ThreadKey) -> bool {
        let grants = self.lock();
        let Some(grant) = grants.get(session) else {
            return false;
        };
        if grant.expires_at <= Instant::now() {
            return false;
        }
        grant.thread == *thread
    }

    pub fn authorized_token(
        &self,
        session: &SessionKey,
        thread: &ThreadKey,
    ) -> Option<ScopedToken> {
        let grants = self.lock();
        let grant = grants.get(session)?;
        (grant.expires_at > Instant::now() && grant.thread == *thread).then(|| grant.token.clone())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionKey, Grant>> {
        self.grants.lock().expect("grant store mutex poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgeclaw_core::{RepoId, Subject};

    fn thread(number: u64) -> ThreadKey {
        ThreadKey {
            repo: RepoId {
                owner: "octo".into(),
                name: "repo".into(),
            },
            subject: Subject::Issue(number),
        }
    }

    fn token() -> ScopedToken {
        ScopedToken {
            id: 1,
            secret: "disposable".into(),
        }
    }

    #[test]
    fn no_grant_is_read_only() {
        let store = GrantStore::default();
        assert!(!store.can_write(&SessionKey::new("session-a"), &thread(1)));
    }

    #[test]
    fn grant_only_authorizes_its_exact_subject() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        store.insert(session.clone(), thread(1), token(), Duration::from_secs(60));

        assert!(store.can_write(&session, &thread(1)));
        assert!(!store.can_write(&session, &thread(2)));
        assert!(!store.can_write(&SessionKey::new("session-b"), &thread(1)));
    }

    #[test]
    fn expired_grant_is_removed() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        store.insert(session.clone(), thread(1), token(), Duration::ZERO);

        assert!(!store.can_write(&session, &thread(1)));
    }

    #[test]
    fn completing_turn_removes_its_grant() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        store.insert(session.clone(), thread(1), token(), Duration::from_secs(60));

        store.take(&session);
        assert!(!store.can_write(&session, &thread(1)));
    }
}
