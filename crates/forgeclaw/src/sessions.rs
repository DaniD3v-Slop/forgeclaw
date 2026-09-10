use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use serde_json::Value;

use crate::grants::SessionKey;

/// Resolves an MCP turn id to the OpenClaw session key that owns it.
///
/// OpenClaw writes this relationship in `session.started` runtime events. The
/// daemon is only a reader; it does not create or modify gateway state.
#[derive(Debug)]
pub struct SessionResolver {
    database: PathBuf,
    cache: Mutex<HashMap<String, SessionKey>>,
}

impl SessionResolver {
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            database: database.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Looks up `turn_id`, caching a successful result because both ids are
    /// stable for a session. An unknown turn deliberately has no authority.
    pub fn resolve(&self, turn_id: &str) -> Result<Option<SessionKey>, rusqlite::Error> {
        if let Some(session) = self
            .cache
            .lock()
            .expect("session cache mutex poisoned")
            .get(turn_id)
            .cloned()
        {
            return Ok(Some(session));
        }

        let session = find_session_key(&self.database, turn_id)?;
        if let Some(session) = &session {
            self.cache
                .lock()
                .expect("session cache mutex poisoned")
                .insert(turn_id.into(), session.clone());
        }
        Ok(session)
    }
}

fn find_session_key(database: &Path, turn_id: &str) -> Result<Option<SessionKey>, rusqlite::Error> {
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = connection.prepare(
        "SELECT event_json
         FROM trajectory_runtime_events
         WHERE event_json LIKE '%' || ?1 || '%'
         ORDER BY rowid DESC",
    )?;
    let events = statement.query_map([turn_id], |row| row.get::<_, String>(0))?;
    for event in events {
        let event = event?;
        if let Some(key) = session_key_from_event(&event, turn_id) {
            return Ok(Some(key));
        }
    }
    Ok(None)
}

fn session_key_from_event(event: &str, turn_id: &str) -> Option<SessionKey> {
    let value: Value = serde_json::from_str(event).ok()?;
    let is_started = value.get("type").and_then(Value::as_str) == Some("session.started");
    let data = value.get("data");
    let event_turn = value
        .get("sessionId")
        .or_else(|| data.and_then(|data| data.get("threadId")))
        .and_then(Value::as_str)?;
    if !is_started || event_turn != turn_id {
        return None;
    }
    value
        .get("sessionKey")
        .or_else(|| data.and_then(|data| data.get("sessionKey")))
        .and_then(Value::as_str)
        .map(SessionKey::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn accepts_the_session_started_shape() {
        let event = r#"{"type":"session.started","sessionKey":"agent:main:forgeclaw/issue-1","data":{"threadId":"turn-1"}}"#;
        assert_eq!(
            session_key_from_event(event, "turn-1"),
            Some(SessionKey::new("agent:main:forgeclaw/issue-1"))
        );
    }

    #[test]
    fn accepts_current_openclaw_top_level_session_id() {
        let event = r#"{"type":"session.started","sessionId":"session-1","sessionKey":"agent:main:forgeclaw/issue-1","data":{}}"#;
        assert_eq!(
            session_key_from_event(event, "session-1"),
            Some(SessionKey::new("agent:main:forgeclaw/issue-1"))
        );
    }

    #[test]
    fn rejects_foreign_or_non_start_events() {
        let event =
            r#"{"type":"session.ended","sessionKey":"agent:main:x","data":{"threadId":"turn-1"}}"#;
        assert_eq!(session_key_from_event(event, "turn-1"), None);
        let event = r#"{"type":"session.started","sessionKey":"agent:main:x","data":{"threadId":"turn-2"}}"#;
        assert_eq!(session_key_from_event(event, "turn-1"), None);
    }

    #[test]
    fn database_lookup_caches_a_resolved_turn() {
        let file = NamedTempFile::new().unwrap();
        let connection = Connection::open(file.path()).unwrap();
        connection
            .execute(
                "CREATE TABLE trajectory_runtime_events (event_json TEXT NOT NULL)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO trajectory_runtime_events (event_json) VALUES (?1)",
                [r#"{"type":"session.started","data":{"threadId":"turn-1","sessionKey":"agent:main:repo#issue/1"}}"#],
            )
            .unwrap();
        drop(connection);

        let resolver = SessionResolver::new(file.path());
        assert_eq!(
            resolver.resolve("turn-1").unwrap(),
            Some(SessionKey::new("agent:main:repo#issue/1"))
        );
    }
}
