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
/// The token remains private so callers can ask whether an action is allowed
/// without accidentally logging or serializing a forge credential.
pub struct Grant {
    thread: ThreadKey,
    token: ScopedToken,
    expires_at: Instant,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("thread", &self.thread)
            .field("token_id", &self.token.id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Grant {
    pub fn thread(&self) -> &ThreadKey {
        &self.thread
    }

    pub fn token(&self) -> &ScopedToken {
        &self.token
    }
}

/// In-memory authority store shared by the webhook router and tool server.
#[derive(Debug, Default)]
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

    /// Removes a grant at turn completion. The returned token is for the
    /// router to revoke using its privileged forge client.
    pub fn take(&self, session: &SessionKey) -> Option<Grant> {
        self.lock().remove(session)
    }

    /// Returns whether this session may mutate exactly `thread` right now.
    /// A missing, expired, or mismatched grant is deliberately indistinguishable
    /// to the caller: all are read-only.
    pub fn can_write(&self, session: &SessionKey, thread: &ThreadKey) -> bool {
        self.authorized_token(session, thread).is_some()
    }

    /// Returns a scoped credential only after the same exact-subject check
    /// used for authorization. This is daemon-internal; it is never serialized
    /// into a tool response.
    pub fn authorized_token(
        &self,
        session: &SessionKey,
        thread: &ThreadKey,
    ) -> Option<ScopedToken> {
        let mut grants = self.lock();
        let grant = grants.get(session)?;
        if grant.expires_at <= Instant::now() {
            grants.remove(session);
            return None;
        }
        (grant.thread == *thread).then(|| grant.token.clone())
    }

    /// Drops expired grants and returns their tokens for revocation.
    pub fn reap_expired(&self) -> Vec<ScopedToken> {
        let now = Instant::now();
        let mut grants = self.lock();
        let expired = grants
            .iter()
            .filter(|(_, grant)| grant.expires_at <= now)
            .map(|(session, _)| session.clone())
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|session| grants.remove(&session).map(|grant| grant.token))
            .collect()
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
            secret: "not-a-real-token".into(),
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
    fn expired_grant_is_read_only_and_reaped() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        store.insert(session.clone(), thread(1), token(), Duration::ZERO);

        assert!(!store.can_write(&session, &thread(1)));
        assert!(store.take(&session).is_none());
    }

    #[test]
    fn completing_turn_removes_its_grant() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        store.insert(session.clone(), thread(1), token(), Duration::from_secs(60));

        let grant = store.take(&session).expect("active grant");
        assert_eq!(grant.thread(), &thread(1));
        assert!(!store.can_write(&session, &thread(1)));
    }
}
