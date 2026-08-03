//! IPC bridge: Tauri commands for frontend → core, and event forwarding core → frontend.
//!
//! Commands are thin: they translate the invoke into a core call and map errors.
//! No business logic lives here. Git/store work is blocking, so commands hop onto
//! the blocking pool via [`run_core`].

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, State};
use tokio::sync::broadcast::error::RecvError;

use crate::core::bus::{Event, EventBus};
use crate::core::diff::{DiffManager, DiffScope, DiffSnapshot, FileDiff};
use crate::core::session::{Session, SessionManager, SessionType, SpawnParams};
use crate::core::store::{Branch, Store};
use crate::core::worktree::{
    BlameLine, CreateWorktreeRequest, RemoveOutcome, RepoInfo, WorktreeInfo, WorktreeManager,
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

// ---------- sessions ----------

#[derive(Debug, Deserialize)]
pub struct SpawnSessionArgs {
    pub branch: String,
    pub prompt: String,
    pub session_type: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
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
) -> Result<(), String> {
    let sessions = state.sessions.clone();
    run_core(state.bus.clone(), move || {
        sessions.send(&session_id, &prompt)
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
