//! Durable, at-least-once queue for verified webhook deliveries.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forgeclaw_core::{Error, ForgeEvent, Result};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Outbox {
    path: PathBuf,
}

pub struct Pending {
    pub id: String,
    pub events: Vec<ForgeEvent>,
    pub attempts: u32,
}

impl Outbox {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let outbox = Self { path };
        outbox
            .connect()?
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS deliveries (
                id TEXT PRIMARY KEY,
                events TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt INTEGER NOT NULL DEFAULT 0,
                completed INTEGER NOT NULL DEFAULT 0
            ); CREATE INDEX IF NOT EXISTS deliveries_ready
               ON deliveries(completed, next_attempt);",
            )
            .map_err(db_error)?;
        Ok(outbox)
    }

    pub fn enqueue(&self, id: &str, events: &[ForgeEvent]) -> Result<()> {
        let events =
            serde_json::to_string(events).map_err(|error| Error::Forge(error.to_string()))?;
        self.connect()?
            .execute(
                "INSERT OR IGNORE INTO deliveries (id, events) VALUES (?1, ?2)",
                params![id, events],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn ready(&self) -> Result<Option<Pending>> {
        let row: Option<(String, String, u32)> = self
            .connect()?
            .query_row(
                "SELECT id, events, attempts FROM deliveries
                 WHERE completed = 0 AND next_attempt <= ?1
                 ORDER BY next_attempt, rowid LIMIT 1",
                [now_millis()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(db_error)?;
        row.map(|(id, events, attempts)| {
            Ok(Pending {
                id,
                events: serde_json::from_str(&events)
                    .map_err(|error| Error::Forge(format!("outbox event: {error}")))?,
                attempts,
            })
        })
        .transpose()
    }

    pub fn complete(&self, id: &str) -> Result<()> {
        self.connect()?
            .execute(
                "UPDATE deliveries SET completed = 1, events = '[]' WHERE id = ?1",
                [id],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn retry(&self, id: &str, attempts: u32) -> Result<()> {
        let delay_secs = 5_i64.saturating_mul(1_i64 << attempts.min(6)).min(300);
        self.connect()?
            .execute(
                "UPDATE deliveries SET attempts = attempts + 1, next_attempt = ?2 WHERE id = ?1",
                params![id, now_millis() + delay_secs * 1000],
            )
            .map_err(db_error)?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(Path::new(&self.path)).map_err(db_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        Ok(connection)
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn db_error(error: rusqlite::Error) -> Error {
    Error::Forge(format!("outbox database: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgeclaw_core::Subject;
    use serde_json::json;

    #[test]
    fn queued_delivery_survives_restart_and_is_not_replayed_after_completion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("outbox.sqlite");
        let event = ForgeEvent {
            repo: "owner/repo".parse().unwrap(),
            kind: "comment.created".into(),
            subject: Subject::Issue(7),
            payload: json!({"body": "hello"}),
        };
        let outbox = Outbox::open(&path).unwrap();
        outbox
            .enqueue("delivery-1", std::slice::from_ref(&event))
            .unwrap();
        outbox
            .enqueue("delivery-1", std::slice::from_ref(&event))
            .unwrap();
        drop(outbox);

        let outbox = Outbox::open(&path).unwrap();
        let pending = outbox.ready().unwrap().unwrap();
        assert_eq!(pending.id, "delivery-1");
        assert_eq!(pending.events.len(), 1);
        assert_eq!(pending.events[0].payload, event.payload);
        outbox.retry(&pending.id, pending.attempts).unwrap();
        assert!(outbox.ready().unwrap().is_none());
        // Simulate the retry deadline having elapsed, then a repaired downstream.
        outbox
            .connect()
            .unwrap()
            .execute(
                "UPDATE deliveries SET next_attempt = 0 WHERE id = ?1",
                ["delivery-1"],
            )
            .unwrap();
        outbox.complete("delivery-1").unwrap();
        drop(outbox);

        let outbox = Outbox::open(&path).unwrap();
        assert!(outbox.ready().unwrap().is_none());
        outbox.enqueue("delivery-1", &[event]).unwrap();
        assert!(outbox.ready().unwrap().is_none());
    }
}
