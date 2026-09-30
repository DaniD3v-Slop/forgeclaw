//! Durable, at-least-once queue for verified webhook deliveries.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forgeclaw_core::{Error, ForgeEvent, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub struct Outbox {
    path: PathBuf,
}

pub struct Pending {
    pub id: String,
    pub events: Vec<ForgeEvent>,
    pub attempts: u32,
}

#[derive(Serialize)]
pub struct OutboxStatus {
    pub pending: u64,
    pub oldest_pending_ms: Option<i64>,
    pub last_error: Option<String>,
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
               ON deliveries(completed, next_attempt);
               CREATE TABLE IF NOT EXISTS completed_subjects (
                 delivery_id TEXT NOT NULL,
                 subject TEXT NOT NULL,
                 PRIMARY KEY (delivery_id, subject)
               );
               CREATE TABLE IF NOT EXISTS delivery_failures (
                 delivery_id TEXT PRIMARY KEY,
                 message TEXT NOT NULL,
                 failed_at INTEGER NOT NULL
               );
               CREATE TABLE IF NOT EXISTS delivery_received (
                 delivery_id TEXT PRIMARY KEY,
                 received_at INTEGER NOT NULL
               );",
            )
            .map_err(db_error)?;
        outbox
            .connect()?
            .execute(
                "INSERT OR IGNORE INTO delivery_received (delivery_id, received_at)
             SELECT id, ?1 FROM deliveries WHERE completed = 0",
                [now_millis()],
            )
            .map_err(db_error)?;
        Ok(outbox)
    }

    pub fn enqueue(&self, id: &str, events: &[ForgeEvent]) -> Result<()> {
        let events =
            serde_json::to_string(events).map_err(|error| Error::Forge(error.to_string()))?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO deliveries (id, events) VALUES (?1, ?2)",
                params![id, events],
            )
            .map_err(db_error)?;
        transaction.execute(
            "INSERT OR IGNORE INTO delivery_received (delivery_id, received_at) VALUES (?1, ?2)",
            params![id, now_millis()],
        ).map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
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
            let completed = self.completed_subjects(&id)?;
            Ok(Pending {
                id,
                events: serde_json::from_str::<Vec<ForgeEvent>>(&events)
                    .map_err(|error| Error::Forge(format!("outbox event: {error}")))?
                    .into_iter()
                    .filter(|event: &ForgeEvent| !completed.contains(&event.thread().to_string()))
                    .collect(),
                attempts,
            })
        })
        .transpose()
    }

    pub fn complete(&self, id: &str) -> Result<()> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE deliveries SET completed = 1, events = '[]' WHERE id = ?1",
                [id],
            )
            .map_err(db_error)?;
        transaction
            .execute(
                "DELETE FROM completed_subjects WHERE delivery_id = ?1",
                [id],
            )
            .map_err(db_error)?;
        transaction
            .execute("DELETE FROM delivery_failures WHERE delivery_id = ?1", [id])
            .map_err(db_error)?;
        transaction
            .execute("DELETE FROM delivery_received WHERE delivery_id = ?1", [id])
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(())
    }

    pub fn complete_subject(&self, id: &str, subject: &str) -> Result<()> {
        self.connect()?
            .execute(
                "INSERT OR IGNORE INTO completed_subjects (delivery_id, subject) VALUES (?1, ?2)",
                params![id, subject],
            )
            .map_err(db_error)?;
        Ok(())
    }

    fn completed_subjects(&self, id: &str) -> Result<std::collections::HashSet<String>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT subject FROM completed_subjects WHERE delivery_id = ?1")
            .map_err(db_error)?;
        let rows = statement
            .query_map([id], |row| row.get(0))
            .map_err(db_error)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(db_error)
    }

    pub fn retry(&self, id: &str, attempts: u32, error: &str) -> Result<()> {
        let delay_secs = 5_i64.saturating_mul(1_i64 << attempts.min(6)).min(300);
        let mut connection = self.connect()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE deliveries SET attempts = attempts + 1, next_attempt = ?2 WHERE id = ?1",
                params![id, now_millis() + delay_secs * 1000],
            )
            .map_err(db_error)?;
        transaction.execute(
            "INSERT INTO delivery_failures (delivery_id, message, failed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(delivery_id) DO UPDATE SET message = excluded.message, failed_at = excluded.failed_at",
            params![id, error, now_millis()],
        ).map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(())
    }

    pub fn retry_now(&self) -> Result<usize> {
        self.connect()?
            .execute(
                "UPDATE deliveries SET next_attempt = 0 WHERE completed = 0 AND next_attempt > 0",
                [],
            )
            .map_err(db_error)
    }

    pub fn status(&self) -> Result<OutboxStatus> {
        let connection = self.connect()?;
        let (pending, oldest_pending_ms): (u64, Option<i64>) = connection
            .query_row(
                "SELECT COUNT(*), MIN(r.received_at) FROM deliveries d
             LEFT JOIN delivery_received r ON r.delivery_id = d.id WHERE d.completed = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(db_error)?;
        let last_error = connection
            .query_row(
                "SELECT message FROM delivery_failures ORDER BY failed_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        Ok(OutboxStatus {
            pending,
            oldest_pending_ms,
            last_error,
        })
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
        outbox
            .retry(&pending.id, pending.attempts, "test failure")
            .unwrap();
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

    #[test]
    fn completed_subject_is_skipped_when_later_subject_retries() {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Outbox::open(dir.path().join("outbox.sqlite")).unwrap();
        let first = ForgeEvent {
            repo: "owner/repo".parse().unwrap(),
            kind: "comment.created".into(),
            subject: Subject::Issue(1),
            payload: json!({"body": "first"}),
        };
        let second = ForgeEvent {
            subject: Subject::Issue(2),
            ..first.clone()
        };
        outbox.enqueue("delivery", &[first, second]).unwrap();
        outbox
            .complete_subject("delivery", "owner/repo#issue/1")
            .unwrap();
        let pending = outbox.ready().unwrap().unwrap();
        assert_eq!(pending.events.len(), 1);
        assert_eq!(pending.events[0].subject, Subject::Issue(2));
        outbox
            .retry("delivery", pending.attempts, "gateway unavailable")
            .unwrap();
        let status = outbox.status().unwrap();
        assert_eq!(status.pending, 1);
        assert!(status.oldest_pending_ms.is_some());
        assert_eq!(status.last_error.as_deref(), Some("gateway unavailable"));
        assert_eq!(outbox.retry_now().unwrap(), 1);
        assert_eq!(outbox.ready().unwrap().unwrap().events.len(), 1);
        outbox.complete("delivery").unwrap();
        assert_eq!(outbox.status().unwrap().pending, 0);
        assert!(outbox.status().unwrap().last_error.is_none());
    }
}
