//! The QA antagonist's launch logic, shared between the IPC command (one click
//! in the UI) and the auto-trigger loop (`red_team_auto`): a child worktree
//! branched off the parent's committed state, plus a writer session whose only
//! goal is to break those changes — edge cases, race conditions, failing tests
//! as proof, findings in REDTEAM.md. Production code stays untouched; findings
//! return to the parent's main agent with a human in the middle.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::config::SETTING_RED_TEAM_AUTO;
use crate::core::diff::{DiffManager, DiffScope};
use crate::core::impact::ImpactManager;
use crate::core::notes::NotesManager;
use crate::core::prompts::PromptManager;
use crate::core::session::manager::SpawnParams;
use crate::core::session::{Session, SessionManager, SessionStatus, SessionType};
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;
use crate::error::{MaestroError, Result};

/// Child worktrees of the antagonist live under this branch prefix.
pub const RED_TEAM_PREFIX: &str = "redteam/";

pub struct RedTeamManager {
    worktrees: Arc<WorktreeManager>,
    sessions: Arc<SessionManager>,
    diffs: Arc<DiffManager>,
    notes: Arc<NotesManager>,
    prompts: Arc<PromptManager>,
    store: Arc<dyn Store>,
    impact: Arc<ImpactManager>,
}

impl RedTeamManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worktrees: Arc<WorktreeManager>,
        sessions: Arc<SessionManager>,
        diffs: Arc<DiffManager>,
        notes: Arc<NotesManager>,
        prompts: Arc<PromptManager>,
        store: Arc<dyn Store>,
        impact: Arc<ImpactManager>,
    ) -> Self {
        Self {
            worktrees,
            sessions,
            diffs,
            notes,
            prompts,
            store,
            impact,
        }
    }

    /// The `red_team_auto` trigger: whenever an implementation session finishes
    /// (`done`, not failed/cancelled) on a normal branch, attack its committed
    /// changes without being asked. Run as a background task.
    pub async fn run_auto_loop(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(Event::SessionStatusChanged {
                    session_id,
                    branch,
                    status: SessionStatus::Done,
                }) => self.maybe_auto_launch(&session_id, &branch),
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "red-team auto loop lagged behind the bus");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    fn maybe_auto_launch(&self, session_id: &str, branch: &str) {
        if branch.starts_with(RED_TEAM_PREFIX) {
            return;
        }
        let enabled = self
            .store
            .get_setting(SETTING_RED_TEAM_AUTO)
            .ok()
            .flatten()
            .is_some_and(|v| v == "true");
        if !enabled {
            return;
        }
        // Only a finished *implementation* session means "new code to attack" —
        // main-agent turns, reviews, and manual chats end constantly.
        let is_implementation = self
            .store
            .get_session(session_id)
            .ok()
            .flatten()
            .is_some_and(|s| s.session_type == SessionType::Implementation);
        if !is_implementation {
            return;
        }
        // A live antagonist is already on it; re-arming now would just fight it.
        let child_name = format!("{RED_TEAM_PREFIX}{}", branch.replace('/', "-"));
        let attack_in_progress = self
            .store
            .list_sessions(&child_name)
            .ok()
            .into_iter()
            .flatten()
            .any(|s| s.session_type == SessionType::RedTeam && !s.status.is_terminal());
        if attack_in_progress {
            tracing::debug!(branch, "red_team_auto: attack already live, skipping");
            return;
        }
        match self.launch(branch) {
            Ok(session) => {
                tracing::info!(branch, session = %session.id, "red_team_auto launched the antagonist");
            }
            // "No committed changes" is a normal outcome (the session may have
            // only edited notes) — a skip, not an error worth surfacing.
            Err(err) => tracing::info!(branch, error = %err, "red_team_auto skipped"),
        }
    }

    /// Spawn (or re-arm) the antagonist for `branch`. Fails when `branch` is
    /// itself a red-team worktree or has no committed changes vs its base.
    pub fn launch(&self, branch: &str) -> Result<Session> {
        if branch.starts_with(RED_TEAM_PREFIX) {
            return Err(MaestroError::InvalidData {
                message: "this already is a red-team worktree — red-team its parent instead".into(),
            });
        }
        // The child branches off the parent's *committed* state, so the file
        // list under attack is the committed diff, not the working tree —
        // recomputed, not cached: the user typically commits right before
        // red-teaming, and a stale snapshot would see an empty branch diff.
        let snapshot = self.diffs.refresh(branch, DiffScope::Branch)?;
        let files: String = snapshot
            .files
            .iter()
            .map(|f| format!("{} {}", f.status, f.path))
            .collect::<Vec<_>>()
            .join("\n");
        if files.is_empty() {
            return Err(MaestroError::InvalidData {
                message: format!(
                    "no committed changes on {branch} vs {} — commit first; the red team attacks committed code",
                    snapshot.base
                ),
            });
        }

        let child_name = format!("{RED_TEAM_PREFIX}{}", branch.replace('/', "-"));
        let child = self.worktrees.ensure_named(&child_name, branch)?;
        let cwd = child.path.to_string_lossy().into_owned();
        // Every worktree gets its main agent, this one included.
        self.sessions.ensure_main(&child_name, &cwd, None, None)?;

        // The dependents of the change are prime hunting ground; best-effort —
        // a failed scan must never block the attack itself.
        let impacted = match self.impact.analyze(branch) {
            Ok(report) => {
                let lines: Vec<String> = report
                    .impacted
                    .iter()
                    .take(30)
                    .map(|f| format!("{} ({}, ring {})", f.path, f.kind, f.distance))
                    .collect();
                if lines.is_empty() {
                    "(no outside dependents found)".to_string()
                } else {
                    lines.join("\n")
                }
            }
            Err(err) => {
                tracing::debug!(error = %err, "impact scan for the red-team prompt failed");
                "(blast-radius scan unavailable)".to_string()
            }
        };

        let branch_row = self.store.get_branch(branch).ok().flatten();
        let mut vars = std::collections::HashMap::new();
        vars.insert("parent_branch".to_string(), branch.to_string());
        vars.insert("base".to_string(), snapshot.base.clone());
        vars.insert("impacted".to_string(), impacted);
        vars.insert(
            "task_id".to_string(),
            branch_row
                .and_then(|b| b.task_id)
                .unwrap_or_else(|| "(none)".to_string()),
        );
        vars.insert("files".to_string(), files);
        vars.insert(
            "notes".to_string(),
            self.notes.current_text(branch).unwrap_or_else(|| {
                "No TASK_NOTES.md on the parent branch — the implementer left no record.".into()
            }),
        );
        let prompt = self.prompts.render("red-team", &vars)?;

        // Configurable bucket, same idea as the daemon's: absent/empty means
        // the session default.
        let setting = |key: &str| {
            self.store
                .get_setting(key)
                .ok()
                .flatten()
                .filter(|v| !v.trim().is_empty())
        };
        self.sessions.spawn(SpawnParams {
            branch: child_name,
            cwd,
            session_type: SessionType::RedTeam,
            model: setting(crate::core::config::SETTING_RED_TEAM_MODEL),
            effort: setting(crate::core::config::SETTING_RED_TEAM_EFFORT),
            // Writes tests autonomously in its own isolated worktree; bash still
            // prompts, and commit/push still hit the gate like everyone else's.
            permission_mode: Some("acceptEdits".into()),
            thinking: None,
            // The review profile carries ask_original_agent — "is this behavior
            // intended?" goes to the parent's implementer instead of being
            // guessed. The profile's other tool posts PR comments; that's not
            // the antagonist's job, so it's withheld.
            tools_profile: Some(crate::core::session::REVIEW_TOOLS_PROFILE.to_string()),
            disallowed_tools: vec!["mcp__maestro__submit_review_comments".to_string()],
            prompt,
            resume_from: None,
        })
    }
}
