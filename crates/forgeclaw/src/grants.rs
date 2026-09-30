use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use forgeclaw_core::{ScopedToken, ThreadKey};

/// The OpenClaw session key that the router selected for a forge thread.
///
/// This is an opaque value. It must come from OpenClaw's trusted plugin-tool
/// context, never from a tool argument supplied by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey(String);

impl SessionKey {
    pub fn new(value: impl Into<String>) -> Self {
        // OpenClaw canonicalizes session keys to lowercase before passing them
        // to plugin tools, including keys supplied to `openclaw agent`.
        Self(value.into().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A currently valid authority to mutate one forge thread.
struct Grant {
    thread: ThreadKey,
    token: ScopedToken,
}

/// In-memory authority store shared by the webhook router and tool server.
#[derive(Default)]
pub struct GrantStore {
    grants: Mutex<HashMap<SessionKey, Arc<Grant>>>,
}

/// Keeps a grant active for the lifetime of one agent turn.
pub struct GrantLease<'a> {
    store: &'a GrantStore,
    session: SessionKey,
    grant: Arc<Grant>,
}

impl Drop for GrantLease<'_> {
    fn drop(&mut self) {
        let mut grants = self.store.lock();
        if grants
            .get(&self.session)
            .is_some_and(|current| Arc::ptr_eq(current, &self.grant))
        {
            grants.remove(&self.session);
        }
    }
}

impl GrantStore {
    pub fn insert(
        &self,
        session: SessionKey,
        thread: ThreadKey,
        token: ScopedToken,
    ) -> GrantLease<'_> {
        let grant = Arc::new(Grant { thread, token });
        self.lock().insert(session.clone(), grant.clone());
        GrantLease {
            store: self,
            session,
            grant,
        }
    }

    pub fn can_write(&self, session: &SessionKey, thread: &ThreadKey) -> bool {
        let grants = self.lock();
        let Some(grant) = grants.get(session) else {
            return false;
        };
        same_subject(&grant.thread, thread)
    }

    pub fn authorized_token(
        &self,
        session: &SessionKey,
        thread: &ThreadKey,
    ) -> Option<ScopedToken> {
        let grants = self.lock();
        let grant = grants.get(session)?;
        same_subject(&grant.thread, thread).then(|| grant.token.clone())
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionKey, Arc<Grant>>> {
        self.grants.lock().expect("grant store mutex poisoned")
    }
}

fn same_subject(a: &ThreadKey, b: &ThreadKey) -> bool {
    a.subject == b.subject
        && a.repo.owner.eq_ignore_ascii_case(&b.repo.owner)
        && a.repo.name.eq_ignore_ascii_case(&b.repo.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgeclaw_core::{RepoId, Subject};
    use std::time::Duration;

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
    fn grant_only_authorizes_its_exact_subject() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        let _lease = store.insert(session.clone(), thread(1), token());

        assert!(store.can_write(&session, &thread(1)));
        assert!(!store.can_write(&session, &thread(2)));
        assert!(!store.can_write(&SessionKey::new("session-b"), &thread(1)));
    }

    #[test]
    fn mixed_case_forgejo_repo_matches_openclaw_session_key() {
        let store = GrantStore::default();
        let mut original = thread(2);
        original.repo.owner = "SrinoHosting".into();
        original.repo.name = "Infra".into();
        let _lease = store.insert(
            SessionKey::new("agent:main:forgejo/SrinoHosting/Infra#issue/2"),
            original,
            token(),
        );

        let session = SessionKey::new("agent:main:forgejo/srinohosting/infra#issue/2");
        let mut requested = thread(2);
        requested.repo.owner = "srinohosting".into();
        requested.repo.name = "infra".into();
        assert!(store.authorized_token(&session, &requested).is_some());
        requested.repo.name = "other".into();
        assert!(store.authorized_token(&session, &requested).is_none());
    }

    #[test]
    fn grant_is_removed_when_turn_ends() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        let lease = store.insert(session.clone(), thread(1), token());

        assert!(store.can_write(&session, &thread(1)));
        drop(lease);
        assert!(!store.can_write(&session, &thread(1)));
    }

    #[test]
    fn older_turn_cannot_remove_a_replacement_grant() {
        let store = GrantStore::default();
        let session = SessionKey::new("session-a");
        let old = store.insert(session.clone(), thread(1), token());
        let current = store.insert(session.clone(), thread(2), token());

        drop(old);
        assert!(store.can_write(&session, &thread(2)));
        drop(current);
        assert!(!store.can_write(&session, &thread(2)));
    }

    #[test]
    #[ignore = "checks the real 15-minute boundary"]
    fn active_grant_survives_fifteen_minutes() {
        let store = GrantStore::default();
        let session = SessionKey::new("long-running-turn");
        let lease = store.insert(session.clone(), thread(1), token());

        std::thread::sleep(Duration::from_secs(901));
        assert!(store.can_write(&session, &thread(1)));
        drop(lease);
        assert!(!store.can_write(&session, &thread(1)));
    }
}
