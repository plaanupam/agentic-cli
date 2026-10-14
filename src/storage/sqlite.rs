use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::policy::canonical_json;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    agent_id TEXT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS idempotency (
    key TEXT PRIMARY KEY,
    tool_name TEXT NOT NULL,
    result TEXT NOT NULL,
    created_at TEXT NOT NULL
);
";

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub goal: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct EventRow {
    pub created_at: String,
    pub agent_id: String,
    pub kind: String,
    pub payload: String,
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite database {}", path.display()))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|err| anyhow::anyhow!("sqlite lock poisoned: {err}"))
    }

    pub fn create_session(&self, id: &str, goal: &str) -> anyhow::Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO sessions (id, goal, created_at) VALUES (?1, ?2, ?3)",
            params![id, goal, now()],
        )?;
        Ok(())
    }

    pub fn update_goal(&self, id: &str, goal: &str) -> anyhow::Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE sessions SET goal = ?1 WHERE id = ?2",
            params![goal, id],
        )?;
        Ok(())
    }

    pub fn append_event(
        &self,
        session_id: &str,
        agent_id: &str,
        kind: &str,
        payload: &str,
    ) -> anyhow::Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO events (session_id, created_at, agent_id, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, now(), agent_id, kind, payload],
        )?;
        Ok(())
    }

    pub fn get_idempotent(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.lock()?;
        let result = conn
            .query_row(
                "SELECT result FROM idempotency WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(result)
    }

    pub fn put_idempotent(&self, key: &str, tool_name: &str, result: &str) -> anyhow::Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO idempotency (key, tool_name, result, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![key, tool_name, result, now()],
        )?;
        Ok(())
    }

    pub fn recent_sessions(&self, limit: usize) -> anyhow::Result<Vec<SessionRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, goal, created_at FROM sessions ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                goal: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn latest_session_id(&self) -> anyhow::Result<Option<String>> {
        let conn = self.lock()?;
        let id = conn
            .query_row(
                "SELECT id FROM sessions ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    pub fn session(&self, id: &str) -> anyhow::Result<Option<SessionRow>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT id, goal, created_at FROM sessions WHERE id = ?1",
                [id],
                |row| {
                    Ok(SessionRow {
                        id: row.get(0)?,
                        goal: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn events_for(&self, session_id: &str) -> anyhow::Result<Vec<EventRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT created_at, agent_id, kind, payload FROM events WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok(EventRow {
                created_at: row.get(0)?,
                agent_id: row.get(1)?,
                kind: row.get(2)?,
                payload: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

pub fn idempotency_key(session_id: &str, tool_name: &str, args: &Value) -> String {
    let canonical = serde_json::to_string(&canonical_json(args)).unwrap_or_else(|_| args.to_string());
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update([0]);
    hasher.update(tool_name.as_bytes());
    hasher.update([0]);
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idempotency_key_ignores_object_key_order() {
        let left = json!({"b": 1, "a": {"d": 2, "c": 3}});
        let right = json!({"a": {"c": 3, "d": 2}, "b": 1});
        assert_eq!(
            idempotency_key("session", "gmail_send_message", &left),
            idempotency_key("session", "gmail_send_message", &right)
        );
        assert_ne!(
            idempotency_key("session", "gmail_send_message", &left),
            idempotency_key("other", "gmail_send_message", &left)
        );
    }

    #[test]
    fn sqlite_round_trip_session_event_and_idempotency() {
        let path = std::env::temp_dir().join(format!(
            "agentic-cli-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = Store::open(&path.join("t.db")).unwrap();
        store.create_session("s1", "read mail").unwrap();
        store
            .append_event("s1", "agent", "spawned", "{\"summary\":\"started\"}")
            .unwrap();
        store
            .put_idempotent("abc", "gmail_send_message", "sent id=1")
            .unwrap();
        assert_eq!(
            store.get_idempotent("abc").unwrap().as_deref(),
            Some("sent id=1")
        );
        let events = store.events_for("s1").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "spawned");
        let _ = std::fs::remove_dir_all(path);
    }
}
