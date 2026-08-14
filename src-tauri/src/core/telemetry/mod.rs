//! Conversation telemetry: every prompt sent to an agent and the text it answered
//! with, appended to a durable, greppable log — independent of the frontend's
//! in-memory transcript, and independent of whether anyone is even looking.
//!
//! Layout, chosen for a future analyst agent to walk without needing this crate:
//!
//! ```text
//! {MAESTRO_HOME}/telemetry/
//!   README.md                          — this module's docs, written once
//!   sessions/
//!     2026-08-09/
//!       impl-T-42-retry-logic__a1b2c3d4.jsonl
//! ```
//!
//! One file per session per UTC day, one JSON object per line (append-only, so a
//! crash mid-write loses at most the last partial line). Each line:
//! `{"ts", "session_id", "branch", "session_type", "role", "text"}` with
//! `role` one of `user` / `assistant` / `thinking`. Turns are recorded whole —
//! streamed deltas are buffered in memory and flushed as one line when the turn
//! ends, not one line per chunk.
//!
//! Gated by the `telemetry_enabled` setting (default on); [`SessionManager`](crate::core::session::SessionManager)
//! checks it before ever calling in here, so disabling it costs nothing at runtime.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::Utc;
use serde::Serialize;

/// `"true"` (the default when unset) records every prompt/reply; `"false"` turns
/// it off entirely.
pub const SETTING_TELEMETRY_ENABLED: &str = "telemetry_enabled";

/// Days of telemetry to keep. Absent/`0` = keep everything (the default —
/// deleting a user's conversation history must be an explicit opt-in).
pub const SETTING_TELEMETRY_RETENTION_DAYS: &str = "telemetry_retention_days";

#[derive(Serialize)]
struct TelemetryLine<'a> {
    ts: String,
    session_id: &'a str,
    branch: &'a str,
    session_type: &'a str,
    role: &'a str,
    text: &'a str,
}

#[derive(Default)]
struct PendingTurn {
    text: String,
    thinking: String,
}

/// Appends conversation telemetry under `root`. Cheap to construct; all state is
/// the small in-flight buffer of the current turn per live session.
pub struct TelemetryManager {
    root: PathBuf,
    pending: Mutex<HashMap<String, PendingTurn>>,
}

const README: &str = r#"# MaestroIDE conversation telemetry

This directory is written by MaestroIDE itself (`core/telemetry`), not by any
external tool — every prompt a user or the daemon sends to an agent, and every
reply that agent gives, lands here as it happens.

## Layout

    sessions/{YYYY-MM-DD}/{branch-slug}__{session-id-prefix}.jsonl

One file per session per UTC day. Files are append-only JSON Lines — safe to
`tail -f`, safe to `grep`, safe to load with any JSONL reader.

## Line shape

    {"ts": "<RFC3339>", "session_id": "...", "branch": "...", "session_type": "...", "role": "user|assistant|thinking", "text": "..."}

- `role: "user"` — a prompt sent to the agent (typed by a person, or composed by
  the daemon).
- `role: "assistant"` — the agent's reply for one turn, coalesced: everything it
  streamed during that turn, joined into one entry rather than one line per chunk.
- `role: "thinking"` — the agent's reasoning for that same turn, when the model
  produced any and thinking wasn't off.

Turns are recorded whole, at the point they end — there is no partial-turn noise
in here, and no tool-call/permission/gate detail either. This is intentionally a
thin slice (the Q&A, plus reasoning) rather than a full activity log.

## Toggle

Controlled by the `telemetry_enabled` setting (default on). Turning it off stops
new writes; it does not touch anything already on disk.

## Retention

`telemetry_retention_days` in config.toml (0 or absent = keep everything, the
default) deletes day folders older than the window at every app start.
"#;

impl TelemetryManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// A prompt just sent to `session_id` — the start of a turn.
    pub fn record_user(&self, session_id: &str, branch: &str, session_type: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.append(session_id, branch, session_type, "user", text);
    }

    /// One more chunk of the assistant's streamed reply for the turn in flight.
    pub fn record_assistant_delta(&self, session_id: &str, text: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending
                .entry(session_id.to_string())
                .or_default()
                .text
                .push_str(text);
        }
    }

    /// One more chunk of the assistant's reasoning for the turn in flight.
    pub fn record_thinking_delta(&self, session_id: &str, text: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending
                .entry(session_id.to_string())
                .or_default()
                .thinking
                .push_str(text);
        }
    }

    /// The turn ended — write whatever accumulated as one assistant line (and one
    /// thinking line, if there was any), then drop the buffer. A no-op if nothing
    /// was ever buffered (a turn that made only tool calls, no text).
    pub fn flush_turn(&self, session_id: &str, branch: &str, session_type: &str) {
        let entry = match self.pending.lock() {
            Ok(mut pending) => pending.remove(session_id),
            Err(_) => None,
        };
        let Some(entry) = entry else {
            return;
        };
        if !entry.text.trim().is_empty() {
            self.append(
                session_id,
                branch,
                session_type,
                "assistant",
                entry.text.trim(),
            );
        }
        if !entry.thinking.trim().is_empty() {
            self.append(
                session_id,
                branch,
                session_type,
                "thinking",
                entry.thinking.trim(),
            );
        }
    }

    fn append(&self, session_id: &str, branch: &str, session_type: &str, role: &str, text: &str) {
        if let Err(err) = self.try_append(session_id, branch, session_type, role, text) {
            tracing::debug!(error = %err, session_id, role, "telemetry write failed");
        }
    }

    fn try_append(
        &self,
        session_id: &str,
        branch: &str,
        session_type: &str,
        role: &str,
        text: &str,
    ) -> std::io::Result<()> {
        let day_dir = self
            .root
            .join("sessions")
            .join(Utc::now().format("%Y-%m-%d").to_string());
        std::fs::create_dir_all(&day_dir)?;
        self.ensure_readme()?;

        let short_id: String = session_id.chars().take(8).collect();
        let path = day_dir.join(format!("{}__{short_id}.jsonl", slugify(branch)));
        let line = TelemetryLine {
            ts: Utc::now().to_rfc3339(),
            session_id,
            branch,
            session_type,
            role,
            text,
        };
        let json = serde_json::to_string(&line)
            .unwrap_or_else(|err| format!(r#"{{"error":"serialize failed: {err}"}}"#));
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{json}")
    }

    fn ensure_readme(&self) -> std::io::Result<()> {
        let path = self.root.join("README.md");
        if path.exists() {
            return Ok(());
        }
        std::fs::write(path, README)
    }

    /// Delete day directories older than `retention_days`. `0` keeps everything.
    /// Only directories whose name parses as `%Y-%m-%d` are considered — anything
    /// else under `sessions/` is left alone. Errors are logged, never fatal:
    /// retention is housekeeping, not a startup dependency.
    pub fn sweep(&self, retention_days: u32) {
        if retention_days == 0 {
            return;
        }
        let sessions = self.root.join("sessions");
        let entries = match std::fs::read_dir(&sessions) {
            Ok(entries) => entries,
            Err(_) => return, // nothing recorded yet
        };
        let cutoff = Utc::now().date_naive() - chrono::Days::new(u64::from(retention_days));
        let mut removed = 0usize;
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name();
            let Some(day) = name
                .to_str()
                .and_then(|n| chrono::NaiveDate::parse_from_str(n, "%Y-%m-%d").ok())
            else {
                continue;
            };
            // `> cutoff` keeps exactly `retention_days` days, today included.
            if day > cutoff {
                continue;
            }
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(err) => {
                    tracing::warn!(error = %err, day = %day, "telemetry retention sweep failed")
                }
            }
        }
        if removed > 0 {
            tracing::info!(removed, retention_days, "telemetry retention sweep");
        }
    }
}

/// Filesystem-safe stand-in for a branch name: anything that isn't
/// alphanumeric/`-`/`_` becomes `-`, so `impl/T-42-x` reads as `impl-T-42-x`.
fn slugify(branch: &str) -> String {
    branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_turn_writes_user_assistant_and_thinking_lines_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().to_path_buf());

        mgr.record_user("sess-1", "impl/T-1-x", "manual", "say hi");
        mgr.record_thinking_delta("sess-1", "let me think ");
        mgr.record_thinking_delta("sess-1", "about it");
        mgr.record_assistant_delta("sess-1", "Hello ");
        mgr.record_assistant_delta("sess-1", "there!");
        mgr.flush_turn("sess-1", "impl/T-1-x", "manual");

        let lines = read_lines(tmp.path(), "impl-T-1-x", "sess-1");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(lines[0]["text"], "say hi");
        assert_eq!(lines[1]["role"], "assistant");
        assert_eq!(lines[1]["text"], "Hello there!");
        assert_eq!(lines[2]["role"], "thinking");
        assert_eq!(lines[2]["text"], "let me think about it");
    }

    #[test]
    fn flushing_an_empty_turn_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().to_path_buf());
        mgr.flush_turn("sess-1", "impl/T-1-x", "manual");
        assert!(!tmp.path().join("sessions").exists());
    }

    #[test]
    fn a_blank_prompt_is_not_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().to_path_buf());
        mgr.record_user("sess-1", "impl/T-1-x", "manual", "   ");
        assert!(!tmp.path().join("sessions").exists());
    }

    #[test]
    fn the_readme_is_written_once_on_first_use() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().to_path_buf());
        mgr.record_user("sess-1", "impl/T-1-x", "manual", "hi");
        let readme = tmp.path().join("README.md");
        assert!(readme.exists());
        assert!(std::fs::read_to_string(readme)
            .unwrap()
            .contains("sessions/{YYYY-MM-DD}"));
    }

    #[test]
    fn two_sessions_on_the_same_branch_get_separate_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().to_path_buf());
        mgr.record_user("aaaaaaaa-1111-session", "impl/T-1-x", "manual", "one");
        mgr.record_user("bbbbbbbb-2222-session", "impl/T-1-x", "manual", "two");

        let day = Utc::now().format("%Y-%m-%d").to_string();
        let dir = tmp.path().join("sessions").join(day);
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "README.md")
            .collect();
        names.sort();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn sweep_removes_only_day_dirs_past_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().to_path_buf());
        let sessions = tmp.path().join("sessions");

        let today = Utc::now().date_naive();
        let old = (today - chrono::Days::new(10))
            .format("%Y-%m-%d")
            .to_string();
        let edge = (today - chrono::Days::new(7))
            .format("%Y-%m-%d")
            .to_string();
        let fresh = today.format("%Y-%m-%d").to_string();
        for day in [&old, &edge, &fresh] {
            std::fs::create_dir_all(sessions.join(day)).unwrap();
            std::fs::write(sessions.join(day).join("x.jsonl"), "{}\n").unwrap();
        }
        // Not a day dir: must survive any window.
        std::fs::create_dir_all(sessions.join("not-a-date")).unwrap();

        mgr.sweep(7);

        assert!(
            !sessions.join(&old).exists(),
            "10 days old is past a 7-day window"
        );
        assert!(
            !sessions.join(&edge).exists(),
            "day 7 is the first one out of the window"
        );
        assert!(sessions.join(&fresh).exists(), "today always survives");
        assert!(sessions.join("not-a-date").exists());
    }

    #[test]
    fn sweep_zero_keeps_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().to_path_buf());
        let sessions = tmp.path().join("sessions");
        let ancient = "2020-01-01";
        std::fs::create_dir_all(sessions.join(ancient)).unwrap();

        mgr.sweep(0);
        assert!(sessions.join(ancient).exists());
    }

    #[test]
    fn sweep_without_a_telemetry_dir_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = TelemetryManager::new(tmp.path().join("never-written"));
        mgr.sweep(7); // must not panic or create anything
        assert!(!tmp.path().join("never-written").exists());
    }

    fn read_lines(
        root: &std::path::Path,
        branch_slug: &str,
        session_id: &str,
    ) -> Vec<serde_json::Value> {
        let day = Utc::now().format("%Y-%m-%d").to_string();
        let short_id: String = session_id.chars().take(8).collect();
        let path = root
            .join("sessions")
            .join(day)
            .join(format!("{branch_slug}__{short_id}.jsonl"));
        let raw = std::fs::read_to_string(path).expect("telemetry file exists");
        raw.lines()
            .map(|l| serde_json::from_str(l).expect("valid json line"))
            .collect()
    }
}
