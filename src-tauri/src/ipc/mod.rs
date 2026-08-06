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

use crate::core::agent::protocol::Attachment;
use crate::core::attention::{AttentionItem, AttentionManager};
use crate::core::bus::{Event, EventBus};
use crate::core::diff::{DiffManager, DiffScope, DiffSnapshot, FileDiff};
use crate::core::gate::{GateManager, GateParam, PendingGate};
use crate::core::notes::{Notes, NotesManager};
use crate::core::prompts::{PromptFile, PromptManager};
use crate::core::questions::{LineQuestionInfo, LineQuestionManager};
use crate::core::session::{Session, SessionManager, SessionType, SpawnParams};
use crate::core::store::{Branch, Store};
use crate::core::worktree::{
    BlameLine, CreateWorktreeRequest, MergeOutcome, RemoveOutcome, RepoInfo, WorktreeInfo,
    WorktreeManager,
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

/// Merge `source_branch` into whatever branch `target_branch`'s worktree has
/// checked out.
#[tauri::command]
pub async fn merge_worktree(
    state: State<'_, AppState>,
    source_branch: String,
    target_branch: String,
) -> Result<MergeOutcome, String> {
    let mgr = state.worktrees.clone();
    run_core(state.bus.clone(), move || {
        mgr.merge_into(&source_branch, &target_branch)
    })
    .await
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
