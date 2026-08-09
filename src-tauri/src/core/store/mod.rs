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

/// One unit of daemon work, keyed for idempotency (see `daemon_tasks` migration).
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DaemonTask {
    pub key: String,
    /// `"issue"` or `"pr_comment"`.
    pub kind: String,
    /// `queued | running | done | failed | dismissed`.
    pub state: String,
    /// Human-readable line for the queue panel.
    pub title: String,
    /// JSON blob the flow needs (issue body, comment text, head ref, …).
    pub payload: String,
    /// Worktree branch, once the task got one.
    pub branch: Option<String>,
    /// Session it spawned, once running.
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A named (model, effort, permission_mode, tools_profile, session_type) combo
/// a user can reuse when starting a new session instead of reconfiguring the
/// same setup for every repeat workflow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionPreset {
    pub id: String,
    pub name: String,
    pub session_type: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub tools_profile: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Cost/turns/tokens summed across sessions, for one time window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct UsageTotals {
    pub cost_usd: f64,
    pub turns: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Aggregate spend for the header/Settings usage view: today (UTC) and all time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct UsageSummary {
    pub today: UsageTotals,
    pub all_time: UsageTotals,
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
    /// Persist a runtime change so history and the UI keep telling the truth. `None`
    /// leaves a column untouched.
    fn set_session_runtime(
        &self,
        id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        permission_mode: Option<&str>,
        thinking: Option<&str>,
    ) -> Result<()>;
    fn get_session(&self, id: &str) -> Result<Option<Session>>;
    fn list_sessions(&self, branch: &str) -> Result<Vec<Session>>;
    /// Sessions in a non-terminal status (spawning/streaming/awaiting_input).
    fn list_active_sessions(&self) -> Result<Vec<Session>>;

    /// Delete a session row. Callers are responsible for ensuring it is terminal.
    fn delete_session(&self, id: &str) -> Result<()>;

    /// Overwrite the transcript the frontend renders for this session — its own
    /// serialized `TranscriptItem[]`, so a restart can restore it verbatim.
    fn save_transcript(&self, session_id: &str, items_json: &str) -> Result<()>;
    fn get_transcript(&self, session_id: &str) -> Result<Option<String>>;

    fn get_setting(&self, key: &str) -> Result<Option<String>>;
    fn set_setting(&self, key: &str, value: &str) -> Result<()>;

    // ---------- session presets ----------

    fn list_session_presets(&self) -> Result<Vec<SessionPreset>>;
    fn insert_session_preset(&self, preset: &SessionPreset) -> Result<()>;
    fn delete_session_preset(&self, id: &str) -> Result<()>;

    // ---------- usage ----------

    /// Overwrite the latest known usage for a session — one row per session,
    /// not a history; each turn's report simply replaces the last.
    #[allow(clippy::too_many_arguments)]
    fn upsert_session_usage(
        &self,
        session_id: &str,
        branch: &str,
        cost_usd: Option<f64>,
        turns: Option<u32>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Result<()>;
    fn usage_summary(&self) -> Result<UsageSummary>;

    // ---------- daemon task queue ----------

    /// Insert a new task in `queued` state. Returns `false` (and changes
    /// nothing) when the key was already seen — the idempotency contract.
    fn insert_daemon_task(&self, task: &DaemonTask) -> Result<bool>;
    /// Every task, newest first — the queue panel's listing.
    fn list_daemon_tasks(&self) -> Result<Vec<DaemonTask>>;
    /// Update a task's state (and optionally attach a branch / session id).
    fn update_daemon_task(
        &self,
        key: &str,
        state: &str,
        branch: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<()>;
    /// Oldest task still in `queued` state, if any.
    fn next_queued_daemon_task(&self) -> Result<Option<DaemonTask>>;
    /// The task currently in `running` state, if any (the daemon runs one at a time).
    fn running_daemon_task(&self) -> Result<Option<DaemonTask>>;
    /// Put any `running` tasks back to `queued` — app restart: their sessions
    /// are already failed by `fail_stale_sessions`, the work is not lost.
    fn requeue_running_daemon_tasks(&self) -> Result<usize>;
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
        thinking: row.get("thinking")?,
        tools_profile: row.get("tools_profile")?,
        sdk_session_id: row.get("sdk_session_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const SESSION_COLUMNS: &str = "id, branch, type, status, model, effort, permission_mode, \
     thinking, tools_profile, sdk_session_id, created_at, updated_at";

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
                "INSERT INTO sessions (id, branch, type, status, model, effort, permission_mode, thinking, tools_profile, sdk_session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    session.id,
                    session.branch,
                    session.session_type.as_str(),
                    session.status.as_str(),
                    session.model,
                    session.effort,
                    session.permission_mode,
                    session.thinking,
                    session.tools_profile,
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

    fn set_session_runtime(
        &self,
        id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        permission_mode: Option<&str>,
        thinking: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE sessions SET
                    model = COALESCE(?1, model),
                    effort = COALESCE(?2, effort),
                    permission_mode = COALESCE(?3, permission_mode),
                    thinking = COALESCE(?4, thinking),
                    updated_at = ?5
                 WHERE id = ?6",
                params![model, effort, permission_mode, thinking, Utc::now(), id],
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
            conn.execute(
                "DELETE FROM session_transcripts WHERE session_id = ?1",
                params![id],
            )?;
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    fn save_transcript(&self, session_id: &str, items_json: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO session_transcripts (session_id, items_json, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET items_json = ?2, updated_at = ?3",
                params![session_id, items_json, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
    }

    fn get_transcript(&self, session_id: &str) -> Result<Option<String>> {
        self.with_conn(|conn| {
            let value = conn
                .query_row(
                    "SELECT items_json FROM session_transcripts WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(value)
        })
    }

    fn list_session_presets(&self) -> Result<Vec<SessionPreset>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, session_type, model, effort, permission_mode, tools_profile, created_at
                 FROM session_presets ORDER BY created_at",
            )?;
            let mut rows = stmt.query([])?;
            let mut presets = Vec::new();
            while let Some(row) = rows.next()? {
                presets.push(SessionPreset {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    session_type: row.get(2)?,
                    model: row.get(3)?,
                    effort: row.get(4)?,
                    permission_mode: row.get(5)?,
                    tools_profile: row.get(6)?,
                    created_at: row.get(7)?,
                });
            }
            Ok(presets)
        })
    }

    fn insert_session_preset(&self, preset: &SessionPreset) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO session_presets (id, name, session_type, model, effort, permission_mode, tools_profile, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    preset.id,
                    preset.name,
                    preset.session_type,
                    preset.model,
                    preset.effort,
                    preset.permission_mode,
                    preset.tools_profile,
                    preset.created_at,
                ],
            )?;
            Ok(())
        })
    }

    fn delete_session_preset(&self, id: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM session_presets WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    fn upsert_session_usage(
        &self,
        session_id: &str,
        branch: &str,
        cost_usd: Option<f64>,
        turns: Option<u32>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO session_usage (session_id, branch, cost_usd, turns, input_tokens, output_tokens, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(session_id) DO UPDATE SET
                    branch = ?2, cost_usd = ?3, turns = ?4, input_tokens = ?5, output_tokens = ?6, updated_at = ?7",
                params![
                    session_id,
                    branch,
                    cost_usd,
                    turns.map(|v| v as i64),
                    input_tokens.map(|v| v as i64),
                    output_tokens.map(|v| v as i64),
                    Utc::now().to_rfc3339(),
                ],
            )?;
            Ok(())
        })
    }

    fn usage_summary(&self) -> Result<UsageSummary> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT cost_usd, turns, input_tokens, output_tokens, date(updated_at) = date('now')
                 FROM session_usage",
            )?;
            let mut rows = stmt.query([])?;
            let mut summary = UsageSummary::default();
            while let Some(row) = rows.next()? {
                let cost: Option<f64> = row.get(0)?;
                let turns: Option<i64> = row.get(1)?;
                let input: Option<i64> = row.get(2)?;
                let output: Option<i64> = row.get(3)?;
                let is_today: bool = row.get(4)?;

                summary.all_time.cost_usd += cost.unwrap_or(0.0);
                summary.all_time.turns += turns.unwrap_or(0) as u64;
                summary.all_time.input_tokens += input.unwrap_or(0) as u64;
                summary.all_time.output_tokens += output.unwrap_or(0) as u64;
                if is_today {
                    summary.today.cost_usd += cost.unwrap_or(0.0);
                    summary.today.turns += turns.unwrap_or(0) as u64;
                    summary.today.input_tokens += input.unwrap_or(0) as u64;
                    summary.today.output_tokens += output.unwrap_or(0) as u64;
                }
            }
            Ok(summary)
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

    fn insert_daemon_task(&self, task: &DaemonTask) -> Result<bool> {
        self.with_conn(|conn| {
            let inserted = conn.execute(
                "INSERT OR IGNORE INTO daemon_tasks
                     (key, kind, state, title, payload, branch, session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    task.key,
                    task.kind,
                    task.state,
                    task.title,
                    task.payload,
                    task.branch,
                    task.session_id,
                    task.created_at,
                    task.updated_at,
                ],
            )?;
            Ok(inserted > 0)
        })
    }

    fn list_daemon_tasks(&self) -> Result<Vec<DaemonTask>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {DAEMON_TASK_COLUMNS} FROM daemon_tasks ORDER BY created_at DESC"
            ))?;
            let tasks = stmt
                .query_map([], daemon_task_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(tasks)
        })
    }

    fn update_daemon_task(
        &self,
        key: &str,
        state: &str,
        branch: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE daemon_tasks SET
                     state = ?2,
                     branch = COALESCE(?3, branch),
                     session_id = COALESCE(?4, session_id),
                     updated_at = ?5
                 WHERE key = ?1",
                params![key, state, branch, session_id, Utc::now()],
            )?;
            Ok(())
        })
    }

    fn next_queued_daemon_task(&self) -> Result<Option<DaemonTask>> {
        self.with_conn(|conn| {
            let task = conn
                .query_row(
                    &format!(
                        "SELECT {DAEMON_TASK_COLUMNS} FROM daemon_tasks
                         WHERE state = 'queued' ORDER BY created_at ASC LIMIT 1"
                    ),
                    [],
                    daemon_task_from_row,
                )
                .optional()?;
            Ok(task)
        })
    }

    fn running_daemon_task(&self) -> Result<Option<DaemonTask>> {
        self.with_conn(|conn| {
            let task = conn
                .query_row(
                    &format!(
                        "SELECT {DAEMON_TASK_COLUMNS} FROM daemon_tasks
                         WHERE state = 'running' ORDER BY created_at ASC LIMIT 1"
                    ),
                    [],
                    daemon_task_from_row,
                )
                .optional()?;
            Ok(task)
        })
    }

    fn requeue_running_daemon_tasks(&self) -> Result<usize> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE daemon_tasks SET state = 'queued', session_id = NULL, updated_at = ?1
                 WHERE state = 'running'",
                params![Utc::now()],
            )?;
            Ok(n)
        })
    }
}

const DAEMON_TASK_COLUMNS: &str =
    "key, kind, state, title, payload, branch, session_id, created_at, updated_at";

fn daemon_task_from_row(row: &Row) -> rusqlite::Result<DaemonTask> {
    Ok(DaemonTask {
        key: row.get("key")?,
        kind: row.get("kind")?,
        state: row.get("state")?,
        title: row.get("title")?,
        payload: row.get("payload")?,
        branch: row.get("branch")?,
        session_id: row.get("session_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::SessionType;

    fn store() -> SqliteStore {
        SqliteStore::open_in_memory().expect("open in-memory store")
    }

    #[test]
    fn daemon_task_queue_round_trip_with_idempotency_and_requeue() {
        let s = store();
        let task = DaemonTask {
            key: "issue:owner/repo#7".into(),
            kind: "issue".into(),
            state: "queued".into(),
            title: "Fix the flaky retry".into(),
            payload: r#"{"number":7}"#.into(),
            branch: None,
            session_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        assert!(s.insert_daemon_task(&task).expect("insert"));
        assert!(
            !s.insert_daemon_task(&task).expect("re-insert"),
            "the same key must never enqueue twice"
        );

        let next = s.next_queued_daemon_task().expect("next").expect("some");
        assert_eq!(next.key, task.key);
        assert!(s.running_daemon_task().expect("running").is_none());

        s.update_daemon_task(
            &task.key,
            "running",
            Some("research/GH-7-x"),
            Some("sess-1"),
        )
        .expect("update");
        let running = s.running_daemon_task().expect("running").expect("some");
        assert_eq!(running.branch.as_deref(), Some("research/GH-7-x"));
        assert_eq!(running.session_id.as_deref(), Some("sess-1"));
        assert!(s.next_queued_daemon_task().expect("next").is_none());

        // App restart: running goes back to queued, the branch it made is kept.
        assert_eq!(s.requeue_running_daemon_tasks().expect("requeue"), 1);
        let requeued = s.next_queued_daemon_task().expect("next").expect("some");
        assert_eq!(requeued.state, "queued");
        assert_eq!(requeued.branch.as_deref(), Some("research/GH-7-x"));
        assert!(requeued.session_id.is_none());

        s.update_daemon_task(&task.key, "done", None, None)
            .expect("done");
        assert!(s.next_queued_daemon_task().expect("next").is_none());
        let all = s.list_daemon_tasks().expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].state, "done");
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
    fn session_presets_round_trip_and_list_oldest_first() {
        let s = store();
        assert_eq!(s.list_session_presets().expect("list"), Vec::new());

        let first = SessionPreset {
            id: "p1".into(),
            name: "Quick research".into(),
            session_type: Some("research".into()),
            model: Some("sonnet".into()),
            effort: Some("high".into()),
            permission_mode: Some("plan".into()),
            tools_profile: None,
            created_at: Utc::now(),
        };
        s.insert_session_preset(&first).expect("insert first");
        let second = SessionPreset {
            id: "p2".into(),
            name: "Full auto".into(),
            session_type: Some("implementation".into()),
            model: None,
            effort: None,
            permission_mode: Some("acceptEdits".into()),
            tools_profile: None,
            created_at: Utc::now(),
        };
        s.insert_session_preset(&second).expect("insert second");

        let presets = s.list_session_presets().expect("list");
        assert_eq!(presets.len(), 2);
        assert_eq!(presets[0].id, "p1");
        assert_eq!(presets[0].name, "Quick research");
        assert_eq!(presets[1].id, "p2");

        s.delete_session_preset("p1").expect("delete");
        let presets = s.list_session_presets().expect("list");
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].id, "p2");
    }

    #[test]
    fn usage_summary_sums_across_sessions_and_a_later_report_replaces_not_adds() {
        let s = store();
        s.upsert_session_usage(
            "sess-1",
            "impl/T-1-x",
            Some(1.5),
            Some(2),
            Some(100),
            Some(50),
        )
        .expect("insert usage 1");
        s.upsert_session_usage(
            "sess-2",
            "impl/T-2-x",
            Some(0.5),
            Some(1),
            Some(20),
            Some(10),
        )
        .expect("insert usage 2");

        let summary = s.usage_summary().expect("summary");
        assert!((summary.all_time.cost_usd - 2.0).abs() < 1e-9);
        assert_eq!(summary.all_time.turns, 3);
        assert_eq!(summary.all_time.input_tokens, 120);
        assert_eq!(summary.all_time.output_tokens, 60);
        // Freshly inserted rows carry today's date, so all_time and today agree here.
        assert_eq!(summary.today, summary.all_time);

        // A later report for the same session overwrites, it does not add.
        s.upsert_session_usage(
            "sess-1",
            "impl/T-1-x",
            Some(3.0),
            Some(4),
            Some(200),
            Some(100),
        )
        .expect("update usage 1");
        let summary = s.usage_summary().expect("summary again");
        assert!((summary.all_time.cost_usd - 3.5).abs() < 1e-9);
        assert_eq!(summary.all_time.turns, 5);
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
    fn transcript_round_trip_and_overwrite() {
        let s = store();
        s.upsert_branch("impl/T-3-x", None, None).expect("branch");
        let session = Session::new("impl/T-3-x", SessionType::Manual, None, None, None);
        s.insert_session(&session).expect("insert session");

        assert_eq!(s.get_transcript(&session.id).expect("get"), None);

        s.save_transcript(&session.id, r#"[{"kind":"user","text":"hi"}]"#)
            .expect("save");
        assert_eq!(
            s.get_transcript(&session.id).expect("get"),
            Some(r#"[{"kind":"user","text":"hi"}]"#.to_string())
        );

        // A later save overwrites rather than accumulating a second row.
        s.save_transcript(
            &session.id,
            r#"[{"kind":"user","text":"hi"},{"kind":"text","text":"hey"}]"#,
        )
        .expect("overwrite");
        assert_eq!(
            s.get_transcript(&session.id).expect("get"),
            Some(r#"[{"kind":"user","text":"hi"},{"kind":"text","text":"hey"}]"#.to_string())
        );
    }

    #[test]
    fn deleting_a_session_drops_its_transcript_too() {
        let s = store();
        s.upsert_branch("impl/T-4-x", None, None).expect("branch");
        let session = Session::new("impl/T-4-x", SessionType::Manual, None, None, None);
        s.insert_session(&session).expect("insert session");
        s.save_transcript(&session.id, "[]").expect("save");

        s.delete_session(&session.id).expect("delete");

        assert_eq!(s.get_transcript(&session.id).expect("get"), None);
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
