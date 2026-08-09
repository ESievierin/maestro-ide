//! IPC bridge: Tauri commands for frontend → core, and event forwarding core → frontend.
//!
//! Commands are thin: they translate the invoke into a core call and map errors.
//! No business logic lives here. Git/store work is blocking, so commands hop onto
//! the blocking pool via [`run_core`].

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::broadcast::error::RecvError;

use crate::core::agent::protocol::Attachment;
use crate::core::attention::{AttentionItem, AttentionManager, SETTING_OS_NOTIFICATIONS};
use crate::core::bus::{Event, EventBus};
use crate::core::checks::{CheckResult, ChecksManager};
use crate::core::compose::ComposeManager;
use crate::core::daemon::{DaemonManager, DaemonStatus};
use crate::core::diff::{DiffManager, DiffScope, DiffSnapshot, FileDiff};
use crate::core::gate::{GateManager, GateParam, PendingGate};
use crate::core::notes::{Notes, NotesManager};
use crate::core::pr::{CreatedPr, PrComment, PrManager, ReplyOutcome};
use crate::core::prompts::{PromptFile, PromptManager};
use crate::core::questions::{LineQuestionInfo, LineQuestionManager};
use crate::core::session::manager::{
    DEFAULT_FINALIZE_TIMEOUT_SECS, DEFAULT_SINGLE_WRITER_POLICY, SETTING_NOTES_FINALIZE_TIMEOUT,
    SETTING_SINGLE_WRITER_POLICY,
};
use crate::core::session::{Session, SessionManager, SessionType, SpawnParams};
use crate::core::store::{Branch, DaemonTask, Store};
use crate::core::telemetry::SETTING_TELEMETRY_ENABLED;
use crate::core::worktree::{
    BlameLine, CreateWorktreeRequest, LogEntry, MergeReport, RemoveOutcome, RepoInfo,
    RestoreOutcome, Snapshot, WorktreeInfo, WorktreeManager,
};
use crate::error::MaestroError;

/// Tauri event channel that carries every core event to the frontend.
pub const EVENT_CHANNEL: &str = "maestro:event";

pub struct AppState {
    pub bus: EventBus,
    pub store: Arc<dyn Store>,
    pub worktrees: Arc<WorktreeManager>,
    pub sessions: Arc<SessionManager>,
    pub diffs: Arc<DiffManager>,
    pub gates: Arc<GateManager>,
    pub questions: Arc<LineQuestionManager>,
    pub prompts: Arc<PromptManager>,
    pub notes: Arc<NotesManager>,
    pub attention: Arc<AttentionManager>,
    pub checks: Arc<ChecksManager>,
    pub daemon: Arc<DaemonManager>,
    pub compose: Arc<ComposeManager>,
    pub prs: Arc<PrManager>,
}

/// Forward every bus event to the frontend over a single Tauri event channel.
/// The frontend zustand store subscribes to this channel and fans out by `type`.
pub fn spawn_event_forwarder(app: AppHandle, bus: EventBus) {
    tauri::async_runtime::spawn(async move {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(err) = app.emit(EVENT_CHANNEL, &event) {
                        tracing::error!(error = %err, "failed to emit event to frontend");
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "event forwarder lagged behind the bus");
                }
                Err(RecvError::Closed) => {
                    tracing::info!("event bus closed; stopping forwarder");
                    break;
                }
            }
        }
    });
}

/// Run a blocking core operation off the async runtime; on failure the error is
/// published as an `error.raised` event and returned to the caller as a string.
async fn run_core<T, F>(bus: EventBus, op: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> crate::error::Result<T> + Send + 'static,
{
    let result = tauri::async_runtime::spawn_blocking(op)
        .await
        .map_err(|join_err| format!("internal error: {join_err}"))?;
    result.map_err(|err: MaestroError| {
        crate::error::report(&bus, &err);
        err.to_string()
    })
}

/// Smoke-test command: publishes a `system.test` event on the core bus. The frontend
/// should see it come back through the event channel, proving the whole pipeline.
#[tauri::command]
pub fn emit_test_event(state: State<'_, AppState>, message: Option<String>) {
    state.bus.publish(Event::Test {
        message: message.unwrap_or_else(|| "test event from core".into()),
    });
}

/// Branch rows from the store (branch state survives worktree removal).
#[tauri::command]
pub async fn list_branches(state: State<'_, AppState>) -> Result<Vec<Branch>, String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || store.list_branches()).await
}

/// Currently selected repository, or null when none is set.
#[tauri::command]
pub async fn get_workspace(state: State<'_, AppState>) -> Result<Option<RepoInfo>, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.repo_info()).await
}

/// Select the repository to orchestrate (validated as a git repo, persisted).
#[tauri::command]
pub async fn set_repo(state: State<'_, AppState>, path: String) -> Result<RepoInfo, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || {
        mgr.set_repo(&PathBuf::from(path))
    })
    .await
}

#[tauri::command]
pub async fn list_worktrees(state: State<'_, AppState>) -> Result<Vec<WorktreeInfo>, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.list()).await
}

#[tauri::command]
pub async fn create_worktree(
    state: State<'_, AppState>,
    request: CreateWorktreeRequest,
) -> Result<WorktreeInfo, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.create(request)).await
}

#[tauri::command]
pub async fn remove_worktree(
    state: State<'_, AppState>,
    branch: String,
    force: bool,
) -> Result<RemoveOutcome, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.remove(&branch, force)).await
}

/// Open a worktree's directory in an external tool: `"explorer"` or `"editor"`
/// (the configured/auto-detected editor — Rider by default). With `file` set,
/// the editor opens that file inside the worktree's project.
#[tauri::command]
pub async fn open_worktree(
    state: State<'_, AppState>,
    branch: String,
    target: String,
    file: Option<String>,
) -> Result<(), String> {
    let mgr = state.worktrees.clone();
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        let worktree = mgr
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch.as_str()))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;
        match target.as_str() {
            "explorer" => crate::core::launcher::open_in_explorer(&worktree.path),
            "editor" => crate::core::launcher::open_in_editor(
                store.as_ref(),
                &worktree.path,
                file.as_deref(),
            ),
            other => Err(MaestroError::InvalidData {
                message: format!("unknown open target: {other}"),
            }),
        }
    })
    .await
}

/// Merge `source_branch` into `target_branch` — in the target's own worktree
/// when it has one, otherwise in the primary worktree (switched to the target
/// first, so the result is visible in the editor that has the primary open).
#[tauri::command]
pub async fn merge_worktree(
    state: State<'_, AppState>,
    source_branch: String,
    target_branch: String,
) -> Result<MergeReport, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || {
        mgr.merge_into(&source_branch, &target_branch)
    })
    .await
}

/// Merge `branch`'s (freshly fetched) base branch into it — "my branch is
/// behind develop", fixed in one click per worktree.
#[tauri::command]
pub async fn sync_worktree(
    state: State<'_, AppState>,
    branch: String,
) -> Result<MergeReport, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.sync_with_base(&branch)).await
}

/// Stage everything and commit in `branch`'s worktree (the user's own commit
/// button — agent commits still pass the gate). Returns `<short-sha> <subject>`.
#[tauri::command]
pub async fn commit_worktree(
    state: State<'_, AppState>,
    branch: String,
    message: String,
) -> Result<String, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.commit_all(&branch, &message)).await
}

/// Rewrite one worktree file's line endings — the diff viewer's Rider-style
/// picker. Direct and ungated, same trust level as `commit_worktree`.
#[tauri::command]
pub async fn set_line_ending(
    state: State<'_, AppState>,
    branch: String,
    path: String,
    eol: String,
) -> Result<(), String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || {
        mgr.set_line_ending(&branch, &path, &eol)
    })
    .await
}

/// Push `branch` to its remote. The caller (PushDialog) has shown the user the
/// exact command and got an explicit confirmation.
#[tauri::command]
pub async fn push_worktree(state: State<'_, AppState>, branch: String) -> Result<String, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.push(&branch)).await
}

/// Commits on `branch` that its base does not have (newest first, capped).
#[tauri::command]
pub async fn branch_log(
    state: State<'_, AppState>,
    branch: String,
) -> Result<Vec<LogEntry>, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.branch_log(&branch, 100)).await
}

// ---------- checks ----------

/// The configured check command, if any — the frontend hides check UI without one.
#[tauri::command]
pub async fn get_check_command(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let checks = state.checks.clone();
    run_core(state.bus.clone(), move || checks.command()).await
}

/// Start the configured check in `branch`'s worktree; progress travels the bus.
#[tauri::command]
pub async fn run_check(state: State<'_, AppState>, branch: String) -> Result<(), String> {
    let checks = state.checks.clone();
    run_core(state.bus.clone(), move || checks.run(&branch)).await
}

/// Latest check result for `branch` (None until a check has run this app session).
#[tauri::command]
pub async fn get_check(
    state: State<'_, AppState>,
    branch: String,
) -> Result<Option<CheckResult>, String> {
    Ok(state.checks.get(&branch))
}

// ---------- GitHub daemon ----------

/// Full daemon status for the chip and the panel (talks to `gh`, hence blocking).
#[tauri::command]
pub async fn daemon_status(state: State<'_, AppState>) -> Result<DaemonStatus, String> {
    let daemon = state.daemon.clone();
    run_core(state.bus.clone(), move || Ok(daemon.status())).await
}

/// Every daemon task, newest first (the panel's queue/history list).
#[tauri::command]
pub async fn list_daemon_tasks(state: State<'_, AppState>) -> Result<Vec<DaemonTask>, String> {
    let daemon = state.daemon.clone();
    run_core(state.bus.clone(), move || daemon.list_tasks()).await
}

/// Master switch. Turning it on makes the next loop tick poll immediately.
#[tauri::command]
pub async fn set_daemon_enabled(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    let daemon = state.daemon.clone();
    run_core(state.bus.clone(), move || daemon.set_enabled(enabled)).await
}

/// Which gh account the daemon acts as (per-call token; the global active
/// account is never switched).
#[tauri::command]
pub async fn set_daemon_account(state: State<'_, AppState>, account: String) -> Result<(), String> {
    let daemon = state.daemon.clone();
    run_core(state.bus.clone(), move || daemon.set_account(&account)).await
}

/// Whether conversation telemetry (prompts + replies, per `core::telemetry`) is
/// being recorded. On by default.
#[tauri::command]
pub async fn get_telemetry_enabled(state: State<'_, AppState>) -> Result<bool, String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        Ok(store
            .get_setting(SETTING_TELEMETRY_ENABLED)?
            .map(|v| v != "false")
            .unwrap_or(true))
    })
    .await
}

#[tauri::command]
pub async fn set_telemetry_enabled(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        store.set_setting(
            SETTING_TELEMETRY_ENABLED,
            if enabled { "true" } else { "false" },
        )
    })
    .await
}

/// Whether OS notifications are enabled — config-gated per the original brief,
/// now also toggleable at runtime (previously only a frontend-local flag with
/// no backend counterpart; unified so `config.toml` and the UI agree).
#[tauri::command]
pub async fn get_os_notifications_enabled(state: State<'_, AppState>) -> Result<bool, String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        Ok(store
            .get_setting(SETTING_OS_NOTIFICATIONS)?
            .map(|v| v == "true")
            .unwrap_or(false))
    })
    .await
}

#[tauri::command]
pub async fn set_os_notifications_enabled(
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<(), String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        store.set_setting(
            SETTING_OS_NOTIFICATIONS,
            if enabled { "true" } else { "false" },
        )
    })
    .await
}

/// `"read_only"` (default) downgrades a second writer session on the same
/// branch instead of rejecting it; `"reject"` refuses it outright.
#[tauri::command]
pub async fn get_single_writer_policy(state: State<'_, AppState>) -> Result<String, String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        Ok(store
            .get_setting(SETTING_SINGLE_WRITER_POLICY)?
            .unwrap_or_else(|| DEFAULT_SINGLE_WRITER_POLICY.to_string()))
    })
    .await
}

#[tauri::command]
pub async fn set_single_writer_policy(
    state: State<'_, AppState>,
    policy: String,
) -> Result<(), String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        store.set_setting(SETTING_SINGLE_WRITER_POLICY, &policy)
    })
    .await
}

/// Seconds an implementation session gets, on close, to write its
/// `TASK_NOTES.md` before it is closed anyway. `0` disables the finalize step.
#[tauri::command]
pub async fn get_notes_finalize_timeout(state: State<'_, AppState>) -> Result<u64, String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        Ok(store
            .get_setting(SETTING_NOTES_FINALIZE_TIMEOUT)?
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_FINALIZE_TIMEOUT_SECS))
    })
    .await
}

#[tauri::command]
pub async fn set_notes_finalize_timeout(
    state: State<'_, AppState>,
    seconds: u64,
) -> Result<(), String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        store.set_setting(SETTING_NOTES_FINALIZE_TIMEOUT, &seconds.to_string())
    })
    .await
}

/// Take a task out of the queue (or hide a finished one) without touching GitHub.
#[tauri::command]
pub async fn dismiss_daemon_task(state: State<'_, AppState>, key: String) -> Result<(), String> {
    let daemon = state.daemon.clone();
    run_core(state.bus.clone(), move || daemon.dismiss_task(&key)).await
}

// ---------- PR workflow: render prompts, create, reply ----------
//
// Generation is not done here: these commands only render the editable
// templates (git context in, prompt text out). The frontend then asks a real
// session — with `resume_from` the branch's own implementation session when
// one exists — so the answer reflects that agent's actual context, not a
// stateless read of the diff. See `src/utils/agentAsk.ts`.

/// Render the "commit-message" prompt for the branch's uncommitted changes.
#[tauri::command]
pub async fn render_commit_prompt(
    state: State<'_, AppState>,
    branch: String,
    base: Option<String>,
) -> Result<String, String> {
    let compose = state.compose.clone();
    run_core(state.bus.clone(), move || {
        compose.commit_prompt(&branch, base.as_deref())
    })
    .await
}

#[derive(Debug, Serialize)]
pub struct PrPromptResult {
    /// The base actually used — echoes back an auto-detected one.
    pub base: String,
    pub prompt: String,
}

/// Render the "pr-description" prompt for the branch against `base` (or the
/// stored/default base when omitted).
#[tauri::command]
pub async fn render_pr_prompt(
    state: State<'_, AppState>,
    branch: String,
    base: Option<String>,
) -> Result<PrPromptResult, String> {
    let compose = state.compose.clone();
    run_core(state.bus.clone(), move || {
        let (base, prompt) = compose.pr_prompt(&branch, base.as_deref())?;
        Ok(PrPromptResult { base, prompt })
    })
    .await
}

/// Render the follow-up asking an open review session for its final reply
/// drafts. `extra` carries the user's clarifications on a regenerate.
#[tauri::command]
pub async fn render_pr_reply_followup(
    state: State<'_, AppState>,
    extra: Option<String>,
) -> Result<String, String> {
    let prompts = state.prompts.clone();
    run_core(state.bus.clone(), move || {
        let extra = match extra.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => {
                format!("\nAdditional instructions from the user:\n{text}\n")
            }
            _ => String::new(),
        };
        let mut vars = std::collections::HashMap::new();
        vars.insert("extra".to_string(), extra);
        prompts.render("pr-reply", &vars)
    })
    .await
}

/// Push the branch and open a pull request. Commit first via commit_worktree.
#[tauri::command]
pub async fn create_pr(
    state: State<'_, AppState>,
    branch: String,
    title: String,
    body: String,
    base: Option<String>,
) -> Result<CreatedPr, String> {
    let prs = state.prs.clone();
    run_core(state.bus.clone(), move || {
        prs.create(&branch, &title, &body, base.as_deref())
    })
    .await
}

/// Review comments of the branch's open PR (empty when there is none).
#[tauri::command]
pub async fn list_pr_comments(
    state: State<'_, AppState>,
    branch: String,
) -> Result<Vec<PrComment>, String> {
    let prs = state.prs.clone();
    run_core(state.bus.clone(), move || prs.comments(&branch)).await
}

#[derive(Debug, Deserialize)]
pub struct ReplyToPost {
    pub comment_id: u64,
    pub body: String,
}

/// Post the replies the user approved in the dialog — the explicit HITL step.
#[tauri::command]
pub async fn reply_pr_comments(
    state: State<'_, AppState>,
    pr: u64,
    replies: Vec<ReplyToPost>,
) -> Result<Vec<ReplyOutcome>, String> {
    let prs = state.prs.clone();
    run_core(state.bus.clone(), move || {
        let pairs: Vec<(u64, String)> = replies
            .into_iter()
            .map(|r| (r.comment_id, r.body))
            .collect();
        prs.reply(pr, &pairs)
    })
    .await
}

/// Open a URL in the default browser (used for freshly created PRs).
#[tauri::command]
pub async fn open_url(state: State<'_, AppState>, url: String) -> Result<(), String> {
    run_core(state.bus.clone(), move || {
        crate::core::launcher::open_url(&url)
    })
    .await
}

// ---------- worktree snapshots ----------

#[tauri::command]
pub async fn take_snapshot(
    state: State<'_, AppState>,
    branch: String,
    label: String,
) -> Result<(), String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || {
        mgr.snapshot_take(&branch, &label)
    })
    .await
}

#[tauri::command]
pub async fn list_snapshots(
    state: State<'_, AppState>,
    branch: String,
) -> Result<Vec<Snapshot>, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.snapshot_list(&branch)).await
}

#[tauri::command]
pub async fn restore_snapshot(
    state: State<'_, AppState>,
    branch: String,
    id: String,
    confirmed: bool,
) -> Result<RestoreOutcome, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || {
        mgr.snapshot_restore(&branch, &id, confirmed)
    })
    .await
}

#[tauri::command]
pub async fn drop_snapshot(
    state: State<'_, AppState>,
    branch: String,
    id: String,
) -> Result<(), String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || mgr.snapshot_drop(&branch, &id)).await
}

// ---------- sessions ----------

#[derive(Debug, Deserialize)]
pub struct SpawnSessionArgs {
    pub branch: String,
    pub prompt: String,
    pub session_type: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    /// `default`/absent, `off`, or a token budget as a decimal string.
    pub thinking: Option<String>,
    /// Maestro session id of a finished session whose SDK context to resume.
    pub resume_from: Option<String>,
}

/// Spawn an agent session bound to `branch`, executing in that branch's worktree.
#[tauri::command]
pub async fn spawn_session(
    state: State<'_, AppState>,
    args: SpawnSessionArgs,
) -> Result<Session, String> {
    let worktrees = state.worktrees.clone();
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        // Sessions execute inside the branch's worktree; resolve its path first.
        let worktree = worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(args.branch.as_str()))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {}", args.branch),
            })?;

        let session_type = args
            .session_type
            .as_deref()
            .and_then(SessionType::parse)
            .unwrap_or(SessionType::Manual);

        sessions.spawn(SpawnParams {
            branch: args.branch,
            cwd: worktree.path.to_string_lossy().into_owned(),
            session_type,
            model: args.model,
            effort: args.effort,
            permission_mode: args.permission_mode,
            thinking: args.thinking,
            tools_profile: None,
            disallowed_tools: Vec::new(),
            prompt: args.prompt,
            resume_from: args.resume_from,
        })
    })
    .await
}

#[tauri::command]
pub async fn send_prompt(
    state: State<'_, AppState>,
    session_id: String,
    prompt: String,
    attachments: Option<Vec<Attachment>>,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.send(&session_id, &prompt, &attachments.unwrap_or_default())
    })
    .await
}

#[tauri::command]
pub async fn interrupt_session(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || sessions.interrupt(&session_id)).await
}

#[tauri::command]
pub async fn close_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || sessions.close(&session_id)).await
}

#[tauri::command]
pub async fn respond_permission(
    state: State<'_, AppState>,
    request_id: String,
    allow: bool,
    updated_args: Option<Value>,
    message: Option<String>,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.respond_permission(&request_id, allow, updated_args, message)
    })
    .await
}

#[tauri::command]
pub async fn list_sessions(
    state: State<'_, AppState>,
    branch: String,
) -> Result<Vec<Session>, String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || sessions.list_for_branch(&branch)).await
}

/// Remove a finished session from the list (store row + UI).
#[tauri::command]
pub async fn delete_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || sessions.delete(&session_id)).await
}

/// Persist the frontend's own transcript for a session verbatim, so a restart
/// (or a `done`/`cancelled` close) does not lose it. Opaque to the backend —
/// it never parses `items`, only stores and returns it.
#[tauri::command]
pub async fn save_session_transcript(
    state: State<'_, AppState>,
    session_id: String,
    items: serde_json::Value,
) -> Result<(), String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        let json = serde_json::to_string(&items).map_err(|err| MaestroError::InvalidData {
            message: format!("could not serialize transcript: {err}"),
        })?;
        store.save_transcript(&session_id, &json)
    })
    .await
}

/// The transcript saved for a session, if any — `None` for a session that
/// never persisted one (e.g. still live in this process).
#[tauri::command]
pub async fn get_session_transcript(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Option<serde_json::Value>, String> {
    let store = state.store.clone();
    run_core(state.bus.clone(), move || {
        let Some(raw) = store.get_transcript(&session_id)? else {
            return Ok(None);
        };
        let value = serde_json::from_str(&raw).map_err(|err| MaestroError::InvalidData {
            message: format!("corrupt transcript for {session_id}: {err}"),
        })?;
        Ok(Some(value))
    })
    .await
}

// ---------- diffs ----------

/// Cached diff snapshot for a branch (computed on first access).
#[tauri::command]
pub async fn get_diff(
    state: State<'_, AppState>,
    branch: String,
    scope: Option<DiffScope>,
) -> Result<DiffSnapshot, String> {
    let diffs = state.diffs.clone();
    run_core(state.bus.clone(), move || {
        diffs
            .get(&branch, scope.unwrap_or_default())
            .map(|s| (*s).clone())
    })
    .await
}

/// Force recompute of a branch's diff; publishes `diff.updated`.
#[tauri::command]
pub async fn refresh_diff(
    state: State<'_, AppState>,
    branch: String,
    scope: Option<DiffScope>,
) -> Result<DiffSnapshot, String> {
    let diffs = state.diffs.clone();
    run_core(state.bus.clone(), move || {
        diffs
            .refresh(&branch, scope.unwrap_or_default())
            .map(|s| (*s).clone())
    })
    .await
}

/// Old/new contents of one changed file (for the unified editor view).
#[tauri::command]
pub async fn get_file_diff(
    state: State<'_, AppState>,
    branch: String,
    path: String,
    scope: Option<DiffScope>,
) -> Result<FileDiff, String> {
    let diffs = state.diffs.clone();
    run_core(state.bus.clone(), move || {
        diffs.file_diff(&branch, &path, scope.unwrap_or_default())
    })
    .await
}

/// Blame for a line range in the branch's worktree (line-question context in T6).
#[tauri::command]
pub async fn blame_range(
    state: State<'_, AppState>,
    branch: String,
    path: String,
    start: u32,
    end: u32,
) -> Result<Vec<BlameLine>, String> {
    let diffs = state.diffs.clone();
    run_core(state.bus.clone(), move || {
        diffs.blame(&branch, &path, start, end)
    })
    .await
}

// ---------- gates ----------

/// Pending gated tool calls, oldest first (dialog restore on UI reload).
#[tauri::command]
pub async fn list_pending_gates(state: State<'_, AppState>) -> Result<Vec<PendingGate>, String> {
    let gates = state.gates.clone();
    run_core(state.bus.clone(), move || gates.list()).await
}

/// Resolve a gate: allow executes with the edited params substituted into the
/// tool args; deny returns the optional feedback text to the agent.
#[tauri::command]
pub async fn respond_gate(
    state: State<'_, AppState>,
    gate_id: String,
    allow: bool,
    edited_params: Vec<GateParam>,
    feedback: Option<String>,
) -> Result<(), String> {
    let gates = state.gates.clone();
    run_core(state.bus.clone(), move || {
        gates.respond(&gate_id, allow, &edited_params, feedback)
    })
    .await
}

// ---------- line questions ----------

/// Ask a question about a range of lines in a diff (T6). Builds context (hunk text,
/// blame, branch), renders the `line-question` template, and sends it to the
/// branch's active session (or a fresh one), per the `line_question_target` setting.
#[tauri::command]
pub async fn ask_line_question(
    state: State<'_, AppState>,
    branch: String,
    path: String,
    start: u32,
    end: u32,
    question: String,
    scope: Option<DiffScope>,
) -> Result<LineQuestionInfo, String> {
    let questions = state.questions.clone();
    run_core(state.bus.clone(), move || {
        questions.ask(
            &branch,
            &path,
            start,
            end,
            &question,
            scope.unwrap_or_default(),
        )
    })
    .await
}

// ---------- prompt templates ----------

/// Every template in `~/.maestro/prompts`, with edited-vs-default state (T8).
#[tauri::command]
pub async fn list_prompts(state: State<'_, AppState>) -> Result<Vec<PromptFile>, String> {
    let prompts = state.prompts.clone();
    run_core(state.bus.clone(), move || prompts.list()).await
}

/// Overwrite a template; the next render uses it (no restart, nothing cached).
#[tauri::command]
pub async fn save_prompt(
    state: State<'_, AppState>,
    name: String,
    content: String,
) -> Result<PromptFile, String> {
    let prompts = state.prompts.clone();
    run_core(state.bus.clone(), move || prompts.save(&name, &content)).await
}

/// Restore a template to its built-in default.
#[tauri::command]
pub async fn reset_prompt(state: State<'_, AppState>, name: String) -> Result<PromptFile, String> {
    let prompts = state.prompts.clone();
    run_core(state.bus.clone(), move || prompts.reset(&name)).await
}

// ---------- attention queue ----------

/// Everything waiting on the user, most urgent first (T9).
#[tauri::command]
pub async fn list_attention(state: State<'_, AppState>) -> Result<Vec<AttentionItem>, String> {
    let attention = state.attention.clone();
    run_core(state.bus.clone(), move || attention.list()).await
}

/// Acknowledge one item (the user handled it or does not care).
#[tauri::command]
pub async fn dismiss_attention(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let attention = state.attention.clone();
    run_core(state.bus.clone(), move || attention.dismiss(&id)).await
}

/// Refresh the model list from the CLI (no session, no tokens). The answer arrives as a
/// `session.models` event, so the selector is never stale after switching sidecar modes.
#[tauri::command]
pub async fn refresh_models(state: State<'_, AppState>) -> Result<(), String> {
    let sessions = state.sessions.clone();
    let worktrees = state.worktrees.clone();
    run_core(state.bus.clone(), move || {
        // Any repo path works; the CLI reports the same models. Fall back to the
        // process cwd when no repository has been selected yet.
        let cwd = match worktrees.repo_info()? {
            Some(info) => info.path.to_string_lossy().into_owned(),
            None => ".".to_string(),
        };
        sessions.refresh_models(&cwd)
    })
    .await
}

// ---------- runtime session controls (S3) ----------

/// Change a live session's model. Empty string restores the default.
#[tauri::command]
pub async fn set_session_model(
    state: State<'_, AppState>,
    session_id: String,
    model: String,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.set_model(&session_id, &model)
    })
    .await
}

/// Change a live session's effort. Empty string restores the default.
#[tauri::command]
pub async fn set_session_effort(
    state: State<'_, AppState>,
    session_id: String,
    effort: String,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.set_effort(&session_id, &effort)
    })
    .await
}

/// Change a live session's permission mode (respects the single-writer rule).
#[tauri::command]
pub async fn set_session_permission_mode(
    state: State<'_, AppState>,
    session_id: String,
    mode: String,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.set_permission_mode(&session_id, &mode)
    })
    .await
}

/// Change how much a live session may think (`default`, `off`, or a token budget).
#[tauri::command]
pub async fn set_session_thinking(
    state: State<'_, AppState>,
    session_id: String,
    thinking: String,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.set_thinking(&session_id, &thinking)
    })
    .await
}

// ---------- task notes ----------

/// `TASK_NOTES.md` of a branch, read from its worktree. Missing notes are a state, not an
/// error: the panel renders an empty state instead of a toast.
#[tauri::command]
pub async fn get_notes(state: State<'_, AppState>, branch: String) -> Result<Notes, String> {
    let notes = state.notes.clone();
    run_core(state.bus.clone(), move || notes.read(&branch)).await
}

/// Same as [`get_notes`]; a separate command so the UI's Refresh button reads as an
/// explicit user action in logs, and so a future cache has a place to be invalidated.
#[tauri::command]
pub async fn refresh_notes(state: State<'_, AppState>, branch: String) -> Result<Notes, String> {
    let notes = state.notes.clone();
    run_core(state.bus.clone(), move || notes.read(&branch)).await
}

/// Reconnect or enable/disable one MCP server of a live session.
#[tauri::command]
pub async fn mcp_server_action(
    state: State<'_, AppState>,
    session_id: String,
    server: String,
    action: String,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.mcp_action(&session_id, &server, &action)
    })
    .await
}

/// Answer a blocking dialog the agent raised. `result` omitted means cancelled.
#[tauri::command]
pub async fn respond_user_dialog(
    state: State<'_, AppState>,
    request_id: String,
    result: Option<Value>,
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.respond_user_dialog(&request_id, result)
    })
    .await
}
