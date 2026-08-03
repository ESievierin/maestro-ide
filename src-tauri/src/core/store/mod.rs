//! Persistence layer.
//!
//! SQLite, keyed by **branch name** — the branch is the primary key linking
//! worktree ↔ task ↔ PR. Worktrees are disposable; branch state survives worktree
//! re-creation. All core logic depends on the [`Store`] trait, never on SQLite directly.

mod migrations;

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::core::session::{Session, SessionStatus, SessionType};
use crate::error::{MaestroError, Result};

/// Persisted branch row. Created when a worktree is first created for the branch;
/// survives worktree removal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    pub task_id: Option<String>,
    pub base_branch: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Storage boundary. Concrete impl: SQLite. Test doubles implement this trait.
pub trait Store: Send + Sync {
    /// Insert the branch if new. On conflict, `task_id`/`base_branch` are only
    /// overwritten when a new value is provided (`COALESCE`), and `created_at`
    /// is preserved — branch state survives worktree re-creation.
    fn upsert_branch(
        &self,
        name: &str,
        task_id: Option<&str>,
        base_branch: Option<&str>,
    ) -> Result<Branch>;
    fn get_branch(&self, name: &str) -> Result<Option<Branch>>;
    fn list_branches(&self) -> Result<Vec<Branch>>;

    fn insert_session(&self, session: &Session) -> Result<()>;
    fn update_session_status(&self, id: &str, status: SessionStatus) -> Result<()>;
    fn set_session_sdk_id(&self, id: &str, sdk_session_id: &str) -> Result<()>;
    fn get_session(&self, id: &str) -> Result<Option<Session>>;
    fn list_sessions(&self, branch: &str) -> Result<Vec<Session>>;
    /// Sessions in a non-terminal status (spawning/streaming/awaiting_input).
    fn list_active_sessions(&self) -> Result<Vec<Session>>;

    /// Delete a session row. Callers are responsible for ensuring it is terminal.
    fn delete_session(&self, id: &str) -> Result<()>;

    fn get_setting(&self, key: &str) -> Result<Option<String>>;
    fn set_setting(&self, key: &str, value: &str) -> Result<()>;
}

/// SQLite-backed store. A single connection behind a mutex is sufficient for the
/// short, infrequent queries the app makes; revisit if profiling says otherwise.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (creating if needed) the database at `path` and run pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// In-memory database for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        migrations::runner().to_latest(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| MaestroError::InvalidData {
            message: "store mutex poisoned".into(),
        })?;
        f(&conn)
    }
}

fn branch_from_row(row: &Row) -> rusqlite::Result<Branch> {
    Ok(Branch {
        name: row.get("name")?,
        task_id: row.get("task_id")?,
        base_branch: row.get("base_branch")?,
        created_at: row.get("created_at")?,
    })
}

fn session_from_row(row: &Row) -> Result<Session> {
    let type_str: String = row.get("type")?;
    let status_str: String = row.get("status")?;
    let session_type = SessionType::parse(&type_str).ok_or_else(|| MaestroError::InvalidData {
        message: format!("unknown session type in store: {type_str}"),
    })?;
    let status = SessionStatus::parse(&status_str).ok_or_else(|| MaestroError::InvalidData {
        message: format!("unknown session status in store: {status_str}"),
    })?;
    Ok(Session {
        id: row.get("id")?,
        branch: row.get("branch")?,
        session_type,
        status,
        model: row.get("model")?,
        effort: row.get("effort")?,
        permission_mode: row.get("permission_mode")?,
        sdk_session_id: row.get("sdk_session_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const SESSION_COLUMNS: &str =
    "id, branch, type, status, model, effort, permission_mode, sdk_session_id, created_at, updated_at";

impl Store for SqliteStore {
    fn upsert_branch(
        &self,
        name: &str,
        task_id: Option<&str>,
        base_branch: Option<&str>,
    ) -> Result<Branch> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO branches (name, task_id, base_branch, created_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET
                    task_id = COALESCE(excluded.task_id, task_id),
                    base_branch = COALESCE(excluded.base_branch, base_branch)",
                params![name, task_id, base_branch, Utc::now()],
            )?;
            let branch = conn.query_row(
                "SELECT name, task_id, base_branch, created_at FROM branches WHERE name = ?1",
                params![name],
                branch_from_row,
            )?;
            Ok(branch)
        })
    }

    fn get_branch(&self, name: &str) -> Result<Option<Branch>> {
        self.with_conn(|conn| {
            let branch = conn
                .query_row(
                    "SELECT name, task_id, base_branch, created_at FROM branches WHERE name = ?1",
                    params![name],
                    branch_from_row,
                )
                .optional()?;
            Ok(branch)
        })
    }

    fn list_branches(&self) -> Result<Vec<Branch>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT name, task_id, base_branch, created_at FROM branches ORDER BY created_at",
            )?;
            let rows = stmt.query_map([], branch_from_row)?;
            let mut branches = Vec::new();
            for row in rows {
                branches.push(row?);
            }
            Ok(branches)
        })
    }

    fn insert_session(&self, session: &Session) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO sessions (id, branch, type, status, model, effort, permission_mode, sdk_session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    session.id,
                    session.branch,
                    session.session_type.as_str(),
                    session.status.as_str(),
                    session.model,
                    session.effort,
                    session.permission_mode,
                    session.sdk_session_id,
                    session.created_at,
                    session.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    fn update_session_status(&self, id: &str, status: SessionStatus) -> Result<()> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status.as_str(), Utc::now(), id],
            )?;
            if changed == 0 {
                return Err(MaestroError::InvalidData {
                    message: format!("session not found: {id}"),
                });
            }
            Ok(())
        })
    }

    fn set_session_sdk_id(&self, id: &str, sdk_session_id: &str) -> Result<()> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE sessions SET sdk_session_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![sdk_session_id, Utc::now(), id],
            )?;
            if changed == 0 {
                return Err(MaestroError::InvalidData {
                    message: format!("session not found: {id}"),
                });
            }
            Ok(())
        })
    }

    fn get_session(&self, id: &str) -> Result<Option<Session>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"
            ))?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(session_from_row(row)?)),
                None => Ok(None),
            }
        })
    }

    fn list_sessions(&self, branch: &str) -> Result<Vec<Session>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions WHERE branch = ?1 ORDER BY created_at"
            ))?;
            let mut rows = stmt.query(params![branch])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                sessions.push(session_from_row(row)?);
            }
            Ok(sessions)
        })
    }

    fn list_active_sessions(&self) -> Result<Vec<Session>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SESSION_COLUMNS} FROM sessions
                 WHERE status NOT IN ('done', 'failed', 'cancelled') ORDER BY created_at"
            ))?;
            let mut rows = stmt.query([])?;
            let mut sessions = Vec::new();
            while let Some(row) = rows.next()? {
                sessions.push(session_from_row(row)?);
            }
            Ok(sessions)
        })
    }

    fn delete_session(&self, id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|conn| {
            let value = conn
                .query_row(
                    "SELECT value FROM settings WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value)
        })
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::SessionType;

    fn store() -> SqliteStore {
        SqliteStore::open_in_memory().expect("open in-memory store")
    }

    #[test]
    fn migrations_are_valid_and_idempotent() {
        migrations::runner().validate().expect("migrations valid");

        // Running the migration runner twice on the same database must be a no-op.
        let mut conn = Connection::open_in_memory().expect("conn");
        migrations::runner()
            .to_latest(&mut conn)
            .expect("first run");
        migrations::runner()
            .to_latest(&mut conn)
            .expect("second run");
    }

    #[test]
    fn upsert_branch_preserves_created_at() {
        let s = store();
        let first = s
            .upsert_branch("impl/T-1-demo", Some("T-1"), Some("main"))
            .expect("insert");
        let second = s
            .upsert_branch("impl/T-1-demo", Some("T-99"), None)
            .expect("update");
        assert_eq!(first.created_at, second.created_at);
        assert_eq!(second.task_id.as_deref(), Some("T-99"));
        assert_eq!(s.list_branches().expect("list").len(), 1);
    }

    #[test]
    fn upsert_branch_keeps_state_when_no_new_values() {
        let s = store();
        s.upsert_branch("impl/T-5-x", Some("T-5"), Some("main"))
            .expect("insert");
        // Re-attach scenario: no task metadata supplied — stored values survive.
        let reattached = s.upsert_branch("impl/T-5-x", None, None).expect("reattach");
        assert_eq!(reattached.task_id.as_deref(), Some("T-5"));
        assert_eq!(reattached.base_branch.as_deref(), Some("main"));
    }

    #[test]
    fn settings_round_trip() {
        let s = store();
        assert_eq!(s.get_setting("repo_path").expect("get"), None);
        s.set_setting("repo_path", "C:/work/repo").expect("set");
        s.set_setting("repo_path", "C:/work/other")
            .expect("overwrite");
        assert_eq!(
            s.get_setting("repo_path").expect("get").as_deref(),
            Some("C:/work/other")
        );
    }

    #[test]
    fn session_round_trip() {
        let s = store();
        s.upsert_branch("impl/T-2-x", None, None).expect("branch");
        let session = Session::new(
            "impl/T-2-x",
            SessionType::Manual,
            Some("claude-opus-5".into()),
            Some("high".into()),
            Some("default".into()),
        );
        s.insert_session(&session).expect("insert session");

        let loaded = s
            .get_session(&session.id)
            .expect("get")
            .expect("session exists");
        assert_eq!(loaded.branch, "impl/T-2-x");
        assert_eq!(loaded.status, SessionStatus::Spawning);
        assert_eq!(loaded.session_type, SessionType::Manual);

        s.update_session_status(&session.id, SessionStatus::Streaming)
            .expect("update status");
        let sessions = s.list_sessions("impl/T-2-x").expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Streaming);
        assert!(sessions[0].updated_at >= loaded.updated_at);
    }

    #[test]
    fn session_requires_existing_branch() {
        let s = store();
        let session = Session::new("no-such-branch", SessionType::Manual, None, None, None);
        assert!(s.insert_session(&session).is_err());
    }

    #[test]
    fn update_missing_session_is_an_error() {
        let s = store();
        let err = s
            .update_session_status("nope", SessionStatus::Done)
            .expect_err("should fail");
        assert_eq!(err.code(), "invalid_data");
    }
}
