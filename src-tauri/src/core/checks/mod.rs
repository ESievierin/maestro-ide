//! Check runner: one configurable command (`check_command`, e.g. `npm test` or
//! `dotnet build`) run inside a worktree, with the verdict surfaced as a badge.
//!
//! The point is closing the loop after an agent finishes: "the diff looks fine —
//! but does it build?" is answerable without leaving the app. Runs are manual by
//! default; `check_auto = true` also fires one whenever a session on the branch
//! reaches `done`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::session::SessionStatus;
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;
use crate::error::{MaestroError, Result};

/// The command to run. Empty/absent disables the whole feature (buttons hidden).
pub const SETTING_CHECK_COMMAND: &str = "check_command";
/// `"true"` also runs the check automatically when a session finishes.
pub const SETTING_CHECK_AUTO: &str = "check_auto";

/// A run is killed after this long — a hung test suite must not wedge the app.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Only the tail of the output is kept; a full test log can be enormous.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Running,
    Passed,
    Failed,
}

/// Result (or progress) of the latest check run on one branch.
#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    pub branch: String,
    pub status: CheckStatus,
    pub exit_code: Option<i32>,
    /// The command that was run, so the output panel can say what it's showing.
    pub command: String,
    /// Last `MAX_OUTPUT_BYTES` of interleaved stdout+stderr.
    pub output_tail: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub struct ChecksManager {
    store: Arc<dyn Store>,
    worktrees: Arc<WorktreeManager>,
    bus: EventBus,
    results: Mutex<HashMap<String, CheckResult>>,
}

impl ChecksManager {
    pub fn new(store: Arc<dyn Store>, worktrees: Arc<WorktreeManager>, bus: EventBus) -> Self {
        Self {
            store,
            worktrees,
            bus,
            results: Mutex::new(HashMap::new()),
        }
    }

    /// The configured check command, if any. The frontend uses this to decide
    /// whether to show check UI at all.
    pub fn command(&self) -> Result<Option<String>> {
        Ok(self
            .store
            .get_setting(SETTING_CHECK_COMMAND)?
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty()))
    }

    /// Latest result for `branch`, if a check ever ran this app session.
    pub fn get(&self, branch: &str) -> Option<CheckResult> {
        self.results.lock().ok()?.get(branch).cloned()
    }

    /// Start the configured check in `branch`'s worktree. Returns immediately;
    /// progress and the verdict travel the bus (`check.started`/`check.finished`).
    /// A second start while one is running on the same branch is refused.
    pub fn run(self: &Arc<Self>, branch: &str) -> Result<()> {
        let command = self.command()?.ok_or_else(|| MaestroError::Config {
            message: "no check command configured — set `check_command` in ~/.maestro/config.toml"
                .into(),
        })?;
        let worktree = self
            .worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;

        {
            let mut results = self.lock_results()?;
            if results
                .get(branch)
                .map(|r| r.status == CheckStatus::Running)
                .unwrap_or(false)
            {
                return Err(MaestroError::InvalidData {
                    message: format!("a check is already running for '{branch}'"),
                });
            }
            results.insert(
                branch.to_string(),
                CheckResult {
                    branch: branch.to_string(),
                    status: CheckStatus::Running,
                    exit_code: None,
                    command: command.clone(),
                    output_tail: String::new(),
                    started_at: Utc::now(),
                    finished_at: None,
                },
            );
        }
        self.bus.publish(Event::CheckStarted {
            branch: branch.to_string(),
        });
        tracing::info!(branch, command, "check started");

        let manager = Arc::clone(self);
        let branch = branch.to_string();
        tauri::async_runtime::spawn(async move {
            let (passed, exit_code, output) =
                run_command(&command, &worktree.path.to_string_lossy()).await;
            if let Ok(mut results) = manager.lock_results() {
                if let Some(entry) = results.get_mut(&branch) {
                    entry.status = if passed {
                        CheckStatus::Passed
                    } else {
                        CheckStatus::Failed
                    };
                    entry.exit_code = exit_code;
                    entry.output_tail = output;
                    entry.finished_at = Some(Utc::now());
                }
            }
            tracing::info!(branch, passed, ?exit_code, "check finished");
            manager.bus.publish(Event::CheckFinished {
                branch,
                passed,
                exit_code,
            });
        });
        Ok(())
    }

    /// Bus loop: with `check_auto = true`, a session reaching `done` kicks off a
    /// check on its branch. Failures to start (unconfigured, already running)
    /// are logged, not raised — auto mode must never nag.
    pub async fn run_auto_loop(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(Event::SessionStatusChanged {
                    branch,
                    status: SessionStatus::Done,
                    ..
                }) => {
                    let auto = self
                        .store
                        .get_setting(SETTING_CHECK_AUTO)
                        .ok()
                        .flatten()
                        .map(|v| v == "true")
                        .unwrap_or(false);
                    if !auto {
                        continue;
                    }
                    if let Err(err) = self.run(&branch) {
                        tracing::debug!(branch, error = %err, "auto check not started");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "checks auto loop lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    fn lock_results(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, CheckResult>>> {
        self.results.lock().map_err(|_| MaestroError::InvalidData {
            message: "checks lock poisoned".into(),
        })
    }
}

/// Run `command` through the platform shell in `cwd`, with a hard timeout.
/// Returns `(passed, exit_code, output_tail)` — never an error: a launch
/// failure or timeout is a failed check with the reason in the output.
async fn run_command(command: &str, cwd: &str) -> (bool, Option<i32>, String) {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", command]);
        #[allow(unused_imports)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.args(["-c", command]);
        c
    };
    cmd.current_dir(cwd);
    cmd.kill_on_drop(true);

    match tokio::time::timeout(CHECK_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                text.push_str("\n--- stderr ---\n");
                text.push_str(&stderr);
            }
            (
                output.status.success(),
                output.status.code(),
                tail(&text, MAX_OUTPUT_BYTES),
            )
        }
        Ok(Err(err)) => (
            false,
            None,
            format!("failed to launch check command: {err}"),
        ),
        Err(_) => (
            false,
            None,
            format!(
                "check timed out after {} minutes and was killed",
                CHECK_TIMEOUT.as_secs() / 60
            ),
        ),
    }
}

/// Last `max` bytes of `text`, cut at a char boundary, with a marker when trimmed.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("… (output trimmed)\n{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_short_text_and_trims_long_text() {
        assert_eq!(tail("short", 100), "short");
        let long = "x".repeat(200);
        let trimmed = tail(&long, 50);
        assert!(trimmed.starts_with("… (output trimmed)"));
        assert!(trimmed.ends_with(&"x".repeat(50)));
    }

    #[tokio::test]
    async fn run_command_reports_success_failure_and_output() {
        // `exit 0` and `exit 3` behave identically under cmd /C and sh -c.
        let (passed, code, _) = run_command("exit 0", ".").await;
        assert!(passed);
        assert_eq!(code, Some(0));

        let (passed, code, _) = run_command("exit 3", ".").await;
        assert!(!passed);
        assert_eq!(code, Some(3));

        let (passed, _, out) = run_command("echo check-output-marker", ".").await;
        assert!(passed);
        assert!(out.contains("check-output-marker"), "{out}");
    }
}
