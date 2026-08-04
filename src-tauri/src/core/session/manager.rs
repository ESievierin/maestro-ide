//! Session lifecycle manager.
//!
//! Owns the mapping session ↔ branch ↔ runtime status, validates every state-machine
//! transition, persists rows, and publishes `session.*` events on the bus. Consumes
//! [`EngineSignal`]s produced by the agent engine's supervisor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::core::agent::protocol::{Attachment, SidecarEvent};
use crate::core::agent::{AgentEngine, EngineSignal, SpawnSessionRequest};
use crate::core::bus::{Event, EventBus};
use crate::core::gate::GateManager;
use crate::core::notes::NotesManager;
use crate::core::prompts::PromptManager;
use crate::core::session::{
    is_known_effort, is_known_permission_mode, is_known_thinking, is_writer_mode, Session,
    SessionStatus, SessionType, EFFORT_LEVELS, READ_ONLY_MODE, THINKING_OPTIONS,
};
use crate::core::store::Store;
use crate::error::{MaestroError, Result, Severity};

/// Setting key for what happens when a second writer is requested on a branch.
pub const SETTING_SINGLE_WRITER_POLICY: &str = "single_writer_policy";

/// How long an implementation session gets, on close, to write its `TASK_NOTES.md`.
pub const SETTING_NOTES_FINALIZE_TIMEOUT: &str = "notes_finalize_timeout_secs";

/// Default for [`SETTING_NOTES_FINALIZE_TIMEOUT`]. `0` disables the finalize step.
const DEFAULT_FINALIZE_TIMEOUT_SECS: u64 = 120;

/// Template rendered as the finalize prompt.
const NOTES_TEMPLATE: &str = "task-notes";

/// Dialog kind for the plan review. Approving it lets the agent start writing, so the
/// answer goes through the single-writer rule before it reaches the CLI.
const DIALOG_PLAN_APPROVAL: &str = "plan_approval";

/// Permission mode a session lands in when its plan is approved.
const APPROVED_PLAN_MODE: &str = "acceptEdits";

/// Everything the IPC layer passes in to start a session.
#[derive(Clone, Debug)]
pub struct SpawnParams {
    pub branch: String,
    pub cwd: String,
    pub session_type: SessionType,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    /// Thinking budget: `default`/`None`, `off`, or a token count.
    pub thinking: Option<String>,
    pub prompt: String,
    /// Maestro session id of a finished session to resume (continues its SDK context).
    pub resume_from: Option<String>,
}

#[derive(Clone, Debug)]
struct RuntimeSession {
    branch: String,
    status: SessionStatus,
    /// Writer sessions hold the branch's single write slot.
    is_writer: bool,
    /// Set when the user requested close; decides the terminal status on
    /// `session_closed` (graceful close → done/cancelled instead of failed).
    close_requested: bool,
}

pub struct SessionManager {
    store: Arc<dyn Store>,
    bus: EventBus,
    engine: Arc<dyn AgentEngine>,
    /// Gate for dangerous tool calls (T7); `None` skips gating entirely.
    gates: Option<Arc<GateManager>>,
    /// Notes + templates for the finalize step; `None` skips it entirely.
    notes: Option<Arc<NotesManager>>,
    prompts: Option<Arc<PromptManager>>,
    runtime: Mutex<HashMap<String, RuntimeSession>>,
    /// Dialogs the agents are blocked on: request id → (session id, dialog kind). The
    /// engine keys dialogs by request id alone, and everything downstream needs both.
    dialogs: Mutex<HashMap<String, (String, String)>>,
    /// Sessions writing their notes before closing: session id → deadline. The close
    /// happens when their turn ends, or when the deadline passes.
    finalizing: Mutex<HashMap<String, DateTime<Utc>>>,
}

impl SessionManager {
    pub fn new(store: Arc<dyn Store>, bus: EventBus, engine: Arc<dyn AgentEngine>) -> Self {
        Self::with_gates(store, bus, engine, None)
    }

    pub fn with_gates(
        store: Arc<dyn Store>,
        bus: EventBus,
        engine: Arc<dyn AgentEngine>,
        gates: Option<Arc<GateManager>>,
    ) -> Self {
        Self {
            store,
            bus,
            engine,
            gates,
            notes: None,
            prompts: None,
            runtime: Mutex::new(HashMap::new()),
            dialogs: Mutex::new(HashMap::new()),
            finalizing: Mutex::new(HashMap::new()),
        }
    }

    /// Give the manager what it needs to ask a closing implementation session for its
    /// notes. Additive: without it, `close()` behaves exactly as it did before.
    pub fn with_notes(mut self, notes: Arc<NotesManager>, prompts: Arc<PromptManager>) -> Self {
        self.notes = Some(notes);
        self.prompts = Some(prompts);
        self
    }

    /// Consume engine signals until the channel closes. Run as a background task.
    pub async fn run_loop(self: Arc<Self>, mut rx: UnboundedReceiver<EngineSignal>) {
        while let Some(signal) = rx.recv().await {
            match signal {
                EngineSignal::Event(event) => self.handle_event(event),
                EngineSignal::Crashed { code } => self.handle_crash(code),
            }
            self.sweep_finalize_deadlines();
        }
        tracing::info!("engine signal channel closed; session manager loop ending");
    }

    /// Spawn a new session bound to `branch`. The branch row is upserted so sessions
    /// on branches created outside Maestro (e.g. the primary worktree) don't hit FK
    /// violations.
    ///
    /// **Single-writer rule** (enforced here, not in the UI): at most one live session
    /// with write permissions per branch. A second writer request is downgraded to
    /// read-only or rejected, depending on the `single_writer_policy` setting.
    pub fn spawn(&self, mut params: SpawnParams) -> Result<Session> {
        self.store.upsert_branch(&params.branch, None, None)?;

        // Resolve resume before anything else: inherit context from the old session.
        let resume_id = match &params.resume_from {
            Some(source_id) => {
                let source = self.store.get_session(source_id)?.ok_or_else(|| {
                    MaestroError::InvalidData {
                        message: format!("cannot resume: unknown session {source_id}"),
                    }
                })?;
                let sdk_id =
                    source
                        .sdk_session_id
                        .clone()
                        .ok_or_else(|| MaestroError::InvalidData {
                            message: "cannot resume: session has no SDK session id".into(),
                        })?;
                if params.model.is_none() {
                    params.model = source.model.clone();
                }
                if params.effort.is_none() {
                    params.effort = source.effort.clone();
                }
                if params.thinking.is_none() {
                    params.thinking = source.thinking.clone();
                }
                Some(sdk_id)
            }
            None => None,
        };

        // Single-writer enforcement.
        let mut permission_mode = params.permission_mode.clone();
        if is_writer_mode(permission_mode.as_deref()) && self.branch_has_writer(&params.branch)? {
            let policy = self
                .store
                .get_setting(SETTING_SINGLE_WRITER_POLICY)?
                .unwrap_or_else(|| "read_only".to_string());
            match policy.as_str() {
                "reject" => {
                    return Err(MaestroError::InvalidData {
                        message: format!(
                            "a writer session is already active on {} — close it first",
                            params.branch
                        ),
                    });
                }
                _ => {
                    permission_mode = Some(READ_ONLY_MODE.to_string());
                    tracing::info!(
                        branch = params.branch,
                        "writer slot taken; session downgraded to read-only"
                    );
                    self.bus.publish(Event::ErrorRaised {
                        severity: Severity::Info,
                        code: "session".into(),
                        message: format!(
                            "a writer is already active on {} — new session started read-only",
                            params.branch
                        ),
                    });
                }
            }
        }

        let mut session = Session::new(
            params.branch.clone(),
            params.session_type,
            params.model.clone(),
            params.effort.clone(),
            permission_mode.clone(),
        );
        session.thinking = params.thinking.clone();
        self.store.insert_session(&session)?;
        self.lock_runtime()?.insert(
            session.id.clone(),
            RuntimeSession {
                branch: params.branch.clone(),
                status: SessionStatus::Spawning,
                is_writer: session.is_writer(),
                close_requested: false,
            },
        );
        self.publish_status(&session.id, &params.branch, SessionStatus::Spawning);

        let spawn_result = self.engine.spawn_session(SpawnSessionRequest {
            session_id: session.id.clone(),
            cwd: params.cwd,
            prompt: params.prompt,
            session_type: session.session_type.as_str().to_string(),
            model: params.model,
            effort: params.effort,
            permission_mode,
            thinking: params.thinking,
            resume_id,
        });
        if let Err(err) = spawn_result {
            self.transition(&session.id, SessionStatus::Failed);
            return Err(err);
        }

        tracing::info!(
            session_id = session.id,
            branch = params.branch,
            session_type = session.session_type.as_str(),
            writer = session.is_writer(),
            "session spawned"
        );
        Ok(session)
    }

    /// Is a live writer session currently bound to `branch`?
    fn branch_has_writer(&self, branch: &str) -> Result<bool> {
        let runtime = self.lock_runtime()?;
        Ok(runtime
            .values()
            .any(|s| s.branch == branch && s.is_writer && !s.status.is_terminal()))
    }

    pub fn send(&self, session_id: &str, prompt: &str, attachments: &[Attachment]) -> Result<()> {
        self.ensure_live(session_id)?;
        self.engine.send_prompt(session_id, prompt, attachments)
    }

    pub fn interrupt(&self, session_id: &str) -> Result<()> {
        self.ensure_live(session_id)?;
        self.engine.interrupt(session_id)
    }

    /// Close the session. If it is idle this is a graceful completion (`done`);
    /// mid-work it is a cancellation (`cancelled`). Closing an already-finished
    /// session is a no-op.
    pub fn close(&self, session_id: &str) -> Result<()> {
        {
            let mut runtime = self.lock_runtime()?;
            match runtime.get_mut(session_id) {
                Some(entry) => entry.close_requested = true,
                None => {
                    // Not live. Terminal in the store → idempotent no-op; a stale
                    // non-terminal row (shouldn't happen) is swept to failed.
                    return match self.store.get_session(session_id)? {
                        Some(session) if session.status.is_terminal() => Ok(()),
                        Some(_) => {
                            self.transition(session_id, SessionStatus::Failed);
                            Ok(())
                        }
                        None => Err(MaestroError::InvalidData {
                            message: format!("unknown session: {session_id}"),
                        }),
                    };
                }
            }
        }
        // An implementation session gets one last turn to write down what it decided.
        if self.start_finalize(session_id) {
            return Ok(());
        }
        self.engine.close_session(session_id)
    }

    /// Ask a closing implementation session to write its `TASK_NOTES.md`, and defer the
    /// actual close until that turn ends. Returns false when there is nothing to ask —
    /// wrong session type, not idle, notes disabled, or the send failed — in which case the
    /// caller closes as usual. Notes are best-effort: they never block or fail a close.
    fn start_finalize(&self, session_id: &str) -> bool {
        let (notes, prompts) = match (&self.notes, &self.prompts) {
            (Some(notes), Some(prompts)) => (notes, prompts),
            _ => return false,
        };
        let timeout = self.finalize_timeout();
        if timeout.is_zero() {
            return false;
        }

        // Only a session that is idle can take another turn; a streaming one would queue
        // the prompt behind work the user just asked to stop.
        let branch = match self.lock_runtime() {
            Ok(runtime) => match runtime.get(session_id) {
                Some(entry) if entry.status == SessionStatus::AwaitingInput => entry.branch.clone(),
                _ => return false,
            },
            Err(_) => return false,
        };
        match self.store.get_session(session_id) {
            Ok(Some(session)) if session.session_type == SessionType::Implementation => {}
            _ => return false,
        }
        if self
            .finalizing
            .lock()
            .map(|f| f.contains_key(session_id))
            .unwrap_or(true)
        {
            return false;
        }

        let branch_row = self.store.get_branch(&branch).ok().flatten();
        let mut vars = HashMap::new();
        vars.insert("branch".to_string(), branch.clone());
        vars.insert(
            "task_id".to_string(),
            branch_row
                .as_ref()
                .and_then(|b| b.task_id.clone())
                .unwrap_or_else(|| "(none)".to_string()),
        );
        vars.insert(
            "base".to_string(),
            branch_row
                .and_then(|b| b.base_branch)
                .unwrap_or_else(|| "(unknown)".to_string()),
        );
        vars.insert(
            "notes".to_string(),
            notes
                .current_text(&branch)
                .unwrap_or_else(|| "none yet".to_string()),
        );

        let prompt = match prompts.render(NOTES_TEMPLATE, &vars) {
            Ok(prompt) => prompt,
            Err(err) => {
                tracing::warn!(session_id, error = %err, "notes template unavailable; closing without notes");
                return false;
            }
        };
        if let Err(err) = self.engine.send_prompt(session_id, &prompt, &[]) {
            tracing::warn!(session_id, error = %err, "finalize prompt failed; closing without notes");
            return false;
        }

        let deadline = Utc::now() + chrono::Duration::from_std(timeout).unwrap_or_default();
        if let Ok(mut finalizing) = self.finalizing.lock() {
            finalizing.insert(session_id.to_string(), deadline);
        }
        tracing::info!(
            session_id,
            branch,
            "asked session to write TASK_NOTES.md before closing"
        );
        self.transition(session_id, SessionStatus::Streaming);
        true
    }

    /// The finalize turn ended (or ran out of time): close for real, and let the notes
    /// panel know there may be something new to read.
    fn finish_finalize(&self, session_id: &str, branch: &str) {
        let was_finalizing = self
            .finalizing
            .lock()
            .map(|mut f| f.remove(session_id).is_some())
            .unwrap_or(false);
        if !was_finalizing {
            return;
        }
        tracing::info!(session_id, "finalize turn done; closing session");
        self.bus.publish(Event::NotesUpdated {
            branch: branch.to_string(),
        });
        if let Err(err) = self.engine.close_session(session_id) {
            crate::error::report(&self.bus, &err);
        }
    }

    /// Deadline sweep for finalize turns that never end. Called on every engine signal, so
    /// no timer task is needed: a stuck session is closed on the next event, and at the
    /// latest when the user acts again.
    fn sweep_finalize_deadlines(&self) {
        let expired: Vec<String> = match self.finalizing.lock() {
            Ok(finalizing) => {
                let now = Utc::now();
                finalizing
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(id, _)| id.clone())
                    .collect()
            }
            Err(_) => return,
        };
        for session_id in expired {
            tracing::warn!(session_id, "notes finalize timed out; closing anyway");
            let branch = self
                .lock_runtime()
                .ok()
                .and_then(|r| r.get(&session_id).map(|s| s.branch.clone()))
                .unwrap_or_default();
            self.finish_finalize(&session_id, &branch);
        }
    }

    fn finalize_timeout(&self) -> std::time::Duration {
        let secs = self
            .store
            .get_setting(SETTING_NOTES_FINALIZE_TIMEOUT)
            .ok()
            .flatten()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_FINALIZE_TIMEOUT_SECS);
        std::time::Duration::from_secs(secs)
    }

    /// Delete a **finished** session from the store (list cleanup). Deleting an
    /// active session is rejected — close it first.
    pub fn delete(&self, session_id: &str) -> Result<()> {
        match self.store.get_session(session_id)? {
            None => Ok(()), // already gone — idempotent
            Some(session) if session.status.is_terminal() => {
                self.store.delete_session(session_id)?;
                tracing::info!(session_id, "session deleted");
                Ok(())
            }
            Some(_) => Err(MaestroError::InvalidData {
                message: "session is still active — close it before removing".into(),
            }),
        }
    }

    pub fn respond_permission(
        &self,
        request_id: &str,
        allow: bool,
        updated_args: Option<Value>,
        message: Option<String>,
    ) -> Result<()> {
        self.engine
            .respond_permission(request_id, allow, updated_args, message)
    }

    /// Change the model of a live session and persist it, so the transcript, the
    /// selector and the history agree afterwards. An empty string clears the override.
    pub fn set_model(&self, session_id: &str, model: &str) -> Result<()> {
        self.ensure_live(session_id)?;
        self.engine.set_model(session_id, model)?;
        self.store
            .set_session_runtime(session_id, Some(model), None, None, None)?;
        tracing::info!(session_id, model, "session model changed");
        self.bus.publish(Event::SessionSettingsChanged {
            session_id: session_id.to_string(),
            model: Some(model.to_string()),
            effort: None,
            permission_mode: None,
            thinking: None,
        });
        Ok(())
    }

    /// Change the effort of a live session; an empty string clears the override.
    pub fn set_effort(&self, session_id: &str, effort: &str) -> Result<()> {
        if !is_known_effort(effort) {
            return Err(MaestroError::InvalidData {
                message: format!(
                    "unknown effort \"{effort}\" — expected one of: {}",
                    EFFORT_LEVELS.join(", ")
                ),
            });
        }
        self.ensure_live(session_id)?;
        self.engine.set_effort(session_id, effort)?;
        self.store
            .set_session_runtime(session_id, None, Some(effort), None, None)?;
        tracing::info!(session_id, effort, "session effort changed");
        self.bus.publish(Event::SessionSettingsChanged {
            session_id: session_id.to_string(),
            model: None,
            effort: Some(effort.to_string()),
            permission_mode: None,
            thinking: None,
        });
        Ok(())
    }

    /// Change the permission mode of a live session. This can flip the session between
    /// writer and read-only, so the single-writer bookkeeping is updated too.
    pub fn set_permission_mode(&self, session_id: &str, mode: &str) -> Result<()> {
        if !is_known_permission_mode(mode) {
            return Err(MaestroError::InvalidData {
                message: format!(
                    "unknown permission mode \"{mode}\" — expected one of: {}",
                    crate::core::session::PERMISSION_MODES.join(", ")
                ),
            });
        }
        self.ensure_live(session_id)?;

        // Switching to a writer mode must respect the branch's single writer.
        let becomes_writer = is_writer_mode(Some(mode));
        let branch = {
            let runtime = self.lock_runtime()?;
            runtime.get(session_id).map(|s| s.branch.clone())
        };
        if becomes_writer {
            if let Some(branch) = &branch {
                let taken = {
                    let runtime = self.lock_runtime()?;
                    runtime.iter().any(|(id, s)| {
                        id != session_id
                            && s.branch == *branch
                            && s.is_writer
                            && !s.status.is_terminal()
                    })
                };
                if taken {
                    return Err(MaestroError::InvalidData {
                        message: format!(
                            "another writer session is active on {branch} — close it first"
                        ),
                    });
                }
            }
        }

        self.engine.set_permission_mode(session_id, mode)?;
        self.store
            .set_session_runtime(session_id, None, None, Some(mode), None)?;
        if let Ok(mut runtime) = self.lock_runtime() {
            if let Some(entry) = runtime.get_mut(session_id) {
                entry.is_writer = becomes_writer;
            }
        }
        tracing::info!(
            session_id,
            mode,
            writer = becomes_writer,
            "session permission mode changed"
        );
        self.bus.publish(Event::SessionSettingsChanged {
            session_id: session_id.to_string(),
            model: None,
            effort: None,
            permission_mode: Some(mode.to_string()),
            thinking: None,
        });
        Ok(())
    }

    /// Change how much the model may think. Worth its own knob: with the CLI default the
    /// models tested here often produce no thinking at all, so there is nothing to show.
    pub fn set_thinking(&self, session_id: &str, thinking: &str) -> Result<()> {
        if !is_known_thinking(thinking) {
            return Err(MaestroError::InvalidData {
                message: format!(
                    "unknown thinking setting \"{thinking}\" — expected one of: {}",
                    THINKING_OPTIONS.join(", ")
                ),
            });
        }
        self.ensure_live(session_id)?;
        self.engine.set_thinking(session_id, thinking)?;
        self.store
            .set_session_runtime(session_id, None, None, None, Some(thinking))?;
        tracing::info!(session_id, thinking, "session thinking changed");
        self.bus.publish(Event::SessionSettingsChanged {
            session_id: session_id.to_string(),
            model: None,
            effort: None,
            permission_mode: None,
            thinking: Some(thinking.to_string()),
        });
        Ok(())
    }

    /// Answer a dialog the agent is blocked on. `result` is dialog-specific; `None`
    /// cancels, which makes the CLI apply the dialog's own default.
    pub fn respond_user_dialog(&self, request_id: &str, result: Option<Value>) -> Result<()> {
        let behavior = if result.is_some() {
            "completed"
        } else {
            "cancelled"
        };
        let pending = self
            .dialogs
            .lock()
            .ok()
            .and_then(|d| d.get(request_id).cloned());

        // An approved plan turns a read-only session into a writer. The branch allows one,
        // so claim the slot *before* the CLI is told, and refuse the approval if it is
        // taken — otherwise two agents would be writing to the same worktree.
        if let Some((session_id, kind)) = &pending {
            if kind == DIALOG_PLAN_APPROVAL && plan_approved(result.as_ref()) {
                self.set_permission_mode(session_id, APPROVED_PLAN_MODE)
                    .map_err(|err| MaestroError::InvalidData {
                        message: format!("cannot start on this plan: {err}"),
                    })?;
            }
        }

        tracing::info!(request_id, behavior, "user dialog answered");
        self.engine
            .respond_user_dialog(request_id, behavior, result)?;
        // Announce the resolution so the attention queue and any other view stand down.
        if let Ok(mut dialogs) = self.dialogs.lock() {
            dialogs.remove(request_id);
        }
        let session_id = pending
            .map(|(session_id, _)| session_id)
            .unwrap_or_default();
        self.bus.publish(Event::SessionUserDialogResolved {
            session_id,
            request_id: request_id.to_string(),
        });
        Ok(())
    }

    /// Reconnect or enable/disable one of a session's MCP servers. The new state arrives
    /// as a `session.mcp_servers` event.
    pub fn mcp_action(&self, session_id: &str, server: &str, action: &str) -> Result<()> {
        if !matches!(action, "reconnect" | "enable" | "disable") {
            return Err(MaestroError::InvalidData {
                message: format!("unknown mcp action: {action}"),
            });
        }
        self.ensure_live(session_id)?;
        tracing::info!(session_id, server, action, "mcp server action");
        self.engine.mcp_action(session_id, server, action)
    }

    /// Ask the CLI for its model list; the answer arrives as a `session.models` event
    /// with an empty session id. Costs nothing — no session, no turn.
    pub fn refresh_models(&self, cwd: &str) -> Result<()> {
        self.engine.list_models(cwd)
    }

    pub fn list_for_branch(&self, branch: &str) -> Result<Vec<Session>> {
        self.store.list_sessions(branch)
    }

    /// Mark every non-terminal session in the store as failed. Called at startup
    /// (sessions from a previous app run are gone) and after a sidecar crash.
    pub fn fail_stale_sessions(&self, reason: &str) {
        let active = match self.store.list_active_sessions() {
            Ok(sessions) => sessions,
            Err(err) => {
                crate::error::report(&self.bus, &err);
                return;
            }
        };
        for session in active {
            tracing::warn!(session_id = session.id, %reason, "failing stale session");
            if let Some(gates) = &self.gates {
                gates.cancel_for_session(&session.id, reason);
            }
            self.transition(&session.id, SessionStatus::Failed);
        }
    }

    // ---------- event handling ----------

    fn handle_event(&self, event: SidecarEvent) {
        match event {
            SidecarEvent::SessionInit {
                session_id,
                sdk_session_id,
                ..
            } => {
                if let Err(err) = self.store.set_session_sdk_id(&session_id, &sdk_session_id) {
                    crate::error::report(&self.bus, &err);
                }
            }
            SidecarEvent::Status { session_id, status } => {
                let Some(status) = SessionStatus::parse(match status.as_str() {
                    // Sidecar runtime statuses map onto the session state machine.
                    "streaming" => "streaming",
                    "awaiting_input" => "awaiting_input",
                    other => other,
                }) else {
                    tracing::warn!(session_id, %status, "unknown runtime status");
                    return;
                };
                self.transition(&session_id, status);
                // A session that was writing its notes has finished that turn: close it.
                if status == SessionStatus::AwaitingInput {
                    let branch = self
                        .lock_runtime()
                        .ok()
                        .and_then(|r| r.get(&session_id).map(|s| s.branch.clone()))
                        .unwrap_or_default();
                    self.finish_finalize(&session_id, &branch);
                }
            }
            SidecarEvent::StreamDelta {
                session_id,
                text,
                parent_tool_use_id,
            } => {
                self.bus.publish(Event::SessionStreamDelta {
                    session_id,
                    text,
                    parent_tool_use_id,
                });
            }
            SidecarEvent::ThinkingDelta {
                session_id,
                text,
                parent_tool_use_id,
            } => {
                self.bus.publish(Event::SessionThinkingDelta {
                    session_id,
                    text,
                    parent_tool_use_id,
                });
            }
            SidecarEvent::ToolUse {
                session_id,
                tool_use_id,
                name,
                summary,
                parent_tool_use_id,
            } => {
                self.bus.publish(Event::SessionToolUse {
                    session_id,
                    tool_use_id,
                    name,
                    summary,
                    parent_tool_use_id,
                });
            }
            SidecarEvent::ToolResult {
                session_id,
                tool_use_id,
                is_error,
                text,
            } => {
                self.bus.publish(Event::SessionToolResult {
                    session_id,
                    tool_use_id,
                    is_error,
                    text,
                });
            }
            SidecarEvent::Agents { session_id, agents } => {
                self.bus
                    .publish(Event::SessionAgents { session_id, agents });
            }
            SidecarEvent::McpServers {
                session_id,
                servers,
            } => {
                self.bus.publish(Event::SessionMcpServers {
                    session_id,
                    servers,
                });
            }
            SidecarEvent::Todos { session_id, items } => {
                self.bus.publish(Event::SessionTodos { session_id, items });
            }
            SidecarEvent::Usage {
                session_id,
                total_cost_usd,
                num_turns,
                input_tokens,
                output_tokens,
                context_tokens,
                context_max_tokens,
                context_percent,
            } => {
                self.bus.publish(Event::SessionUsage {
                    session_id,
                    total_cost_usd,
                    num_turns,
                    input_tokens,
                    output_tokens,
                    context_tokens,
                    context_max_tokens,
                    context_percent,
                });
            }
            SidecarEvent::RateLimit {
                session_id,
                status,
                limit_type,
                utilization,
                resets_at,
            } => {
                // Worth knowing before it becomes a wall, so it is also an error event.
                if status != "allowed" {
                    let window = limit_type.clone().unwrap_or_else(|| "quota".to_string());
                    let used = utilization
                        .map(|u| format!(" at {u:.0}%"))
                        .unwrap_or_default();
                    self.bus.publish(Event::ErrorRaised {
                        severity: if status == "rejected" {
                            Severity::Error
                        } else {
                            Severity::Warning
                        },
                        code: "rate_limit".into(),
                        message: format!("Rate limit ({window}){used}"),
                    });
                }
                self.bus.publish(Event::SessionRateLimit {
                    session_id,
                    status,
                    limit_type,
                    utilization,
                    resets_at,
                });
            }
            SidecarEvent::PermissionDenied {
                session_id,
                tool,
                reason,
                message,
            } => {
                tracing::info!(session_id, tool, reason, "tool call denied without asking");
                self.bus.publish(Event::SessionPermissionDenied {
                    session_id,
                    tool,
                    reason,
                    message,
                });
            }
            SidecarEvent::PermissionRequest {
                session_id,
                request_id,
                tool,
                args,
                title,
            } => {
                // Gated operations pause here (gate.pending) instead of the
                // plain permission prompt; everything else is unchanged.
                if let Some(gates) = &self.gates {
                    let branch = self
                        .lock_runtime()
                        .ok()
                        .and_then(|r| r.get(&session_id).map(|s| s.branch.clone()))
                        .unwrap_or_default();
                    if gates.intercept(&session_id, &branch, &request_id, &tool, &args) {
                        return;
                    }
                }
                self.bus.publish(Event::SessionPermissionRequest {
                    session_id,
                    request_id,
                    tool,
                    args,
                    title,
                });
            }
            SidecarEvent::UserDialogRequest {
                session_id,
                request_id,
                dialog_kind,
                payload,
                ..
            } => {
                if let Ok(mut dialogs) = self.dialogs.lock() {
                    dialogs.insert(
                        request_id.clone(),
                        (session_id.clone(), dialog_kind.clone()),
                    );
                }
                self.bus.publish(Event::SessionUserDialog {
                    session_id,
                    request_id,
                    dialog_kind,
                    payload,
                });
            }
            SidecarEvent::Commands {
                session_id,
                commands,
            } => {
                self.bus.publish(Event::SessionCommands {
                    session_id,
                    commands,
                });
            }
            SidecarEvent::Models { session_id, models } => {
                self.bus
                    .publish(Event::SessionModels { session_id, models });
            }
            SidecarEvent::Result {
                session_id,
                subtype,
                is_error,
                ..
            } => {
                tracing::info!(session_id, subtype, is_error, "session turn finished");
            }
            SidecarEvent::SessionClosed { session_id, reason } => {
                if let Ok(mut finalizing) = self.finalizing.lock() {
                    finalizing.remove(&session_id);
                }
                // Any gate still waiting on this session can no longer execute.
                if let Some(gates) = &self.gates {
                    gates.cancel_for_session(&session_id, "session closed");
                }
                let close_requested = self
                    .lock_runtime()
                    .ok()
                    .and_then(|r| r.get(&session_id).map(|s| s.close_requested))
                    .unwrap_or(false);
                let current = self.current_status(&session_id);
                let terminal = match reason.as_str() {
                    "error" => SessionStatus::Failed,
                    _ if close_requested => match current {
                        Some(SessionStatus::AwaitingInput) => SessionStatus::Done,
                        _ => SessionStatus::Cancelled,
                    },
                    _ => SessionStatus::Done,
                };
                self.transition(&session_id, terminal);
                if let Ok(mut runtime) = self.lock_runtime() {
                    runtime.remove(&session_id);
                }
            }
            SidecarEvent::Error {
                session_id,
                message,
            } => {
                tracing::error!(?session_id, %message, "sidecar error");
                self.bus.publish(Event::ErrorRaised {
                    severity: Severity::Error,
                    code: "agent".into(),
                    message,
                });
            }
            SidecarEvent::Ready { .. } | SidecarEvent::Ack { .. } => {}
        }
    }

    fn handle_crash(&self, code: Option<i32>) {
        tracing::error!(?code, "sidecar crashed; failing affected sessions");
        // Pending gates target permission requests that died with the process.
        if let Some(gates) = &self.gates {
            let live: Vec<String> = self
                .lock_runtime()
                .map(|r| r.keys().cloned().collect())
                .unwrap_or_default();
            for session_id in live {
                gates.cancel_for_session(&session_id, "sidecar crashed");
            }
        }
        self.bus.publish(Event::ErrorRaised {
            severity: Severity::Error,
            code: "agent".into(),
            message: format!("agent sidecar crashed (exit code {code:?}); sessions failed"),
        });

        // Fail everything we know to be live, then sweep the store for the rest.
        let live: Vec<String> = self
            .lock_runtime()
            .map(|r| r.keys().cloned().collect())
            .unwrap_or_default();
        for session_id in live {
            self.transition(&session_id, SessionStatus::Failed);
        }
        self.fail_stale_sessions("sidecar crash");
    }

    // ---------- helpers ----------

    /// Apply a validated state-machine transition, persist it, publish it.
    fn transition(&self, session_id: &str, next: SessionStatus) {
        let branch = {
            let Ok(mut runtime) = self.lock_runtime() else {
                return;
            };
            match runtime.get_mut(session_id) {
                Some(entry) => {
                    if !entry.status.can_transition_to(next) {
                        tracing::debug!(
                            session_id,
                            from = entry.status.as_str(),
                            to = next.as_str(),
                            "ignoring invalid transition"
                        );
                        return;
                    }
                    entry.status = next;
                    entry.branch.clone()
                }
                None => {
                    // Not in runtime (e.g. stale row from a previous run) — look it up.
                    match self.store.get_session(session_id) {
                        Ok(Some(session)) => {
                            if !session.status.can_transition_to(next) {
                                return;
                            }
                            session.branch
                        }
                        _ => return,
                    }
                }
            }
        };

        if let Err(err) = self.store.update_session_status(session_id, next) {
            crate::error::report(&self.bus, &err);
        }
        self.publish_status(session_id, &branch, next);

        if next.is_terminal() {
            if let Ok(mut runtime) = self.lock_runtime() {
                runtime.remove(session_id);
            }
        }
    }

    fn publish_status(&self, session_id: &str, branch: &str, status: SessionStatus) {
        self.bus.publish(Event::SessionStatusChanged {
            session_id: session_id.to_string(),
            branch: branch.to_string(),
            status,
        });
    }

    fn current_status(&self, session_id: &str) -> Option<SessionStatus> {
        self.lock_runtime()
            .ok()
            .and_then(|r| r.get(session_id).map(|s| s.status))
    }

    fn ensure_live(&self, session_id: &str) -> Result<()> {
        let runtime = self.lock_runtime()?;
        match runtime.get(session_id) {
            Some(entry) if !entry.status.is_terminal() => Ok(()),
            Some(_) => Err(MaestroError::InvalidData {
                message: format!("session is finished: {session_id}"),
            }),
            None => Err(MaestroError::InvalidData {
                message: format!("unknown session: {session_id}"),
            }),
        }
    }

    fn lock_runtime(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, RuntimeSession>>> {
        self.runtime.lock().map_err(|_| MaestroError::InvalidData {
            message: "session manager lock poisoned".into(),
        })
    }
}

/// Did the user approve the plan? The answer is dialog-specific JSON everywhere else in
/// this layer, and this is the one field the core has to understand.
fn plan_approved(result: Option<&Value>) -> bool {
    result
        .and_then(|value| value.get("approved"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::SqliteStore;
    use std::sync::Mutex as StdMutex;

    /// `(request_id, allow, updated_args, message)` as passed to the engine.
    type PermissionCall = (String, bool, Option<Value>, Option<String>);

    /// Engine double that records calls.
    #[derive(Default)]
    struct MockEngine {
        calls: StdMutex<Vec<String>>,
        spawns: StdMutex<Vec<SpawnSessionRequest>>,
        perms: StdMutex<Vec<PermissionCall>>,
    }

    impl AgentEngine for MockEngine {
        fn spawn_session(&self, req: SpawnSessionRequest) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("spawn:{}", req.session_id));
            self.spawns.lock().unwrap().push(req);
            Ok(())
        }
        fn send_prompt(
            &self,
            session_id: &str,
            _prompt: &str,
            _attachments: &[Attachment],
        ) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send:{session_id}"));
            Ok(())
        }
        fn interrupt(&self, session_id: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("interrupt:{session_id}"));
            Ok(())
        }
        fn close_session(&self, session_id: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("close:{session_id}"));
            Ok(())
        }
        fn respond_permission(
            &self,
            request_id: &str,
            allow: bool,
            updated_args: Option<Value>,
            message: Option<String>,
        ) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("perm:{request_id}"));
            self.perms
                .lock()
                .unwrap()
                .push((request_id.to_string(), allow, updated_args, message));
            Ok(())
        }

        fn list_models(&self, _cwd: &str) -> Result<()> {
            Ok(())
        }

        fn set_model(&self, session_id: &str, model: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_model:{session_id}:{model}"));
            Ok(())
        }
        fn set_effort(&self, session_id: &str, effort: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_effort:{session_id}:{effort}"));
            Ok(())
        }
        fn mcp_action(&self, session_id: &str, server: &str, action: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("mcp:{session_id}:{server}:{action}"));
            Ok(())
        }

        fn set_thinking(&self, session_id: &str, thinking: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_thinking:{session_id}:{thinking}"));
            Ok(())
        }

        fn set_permission_mode(&self, session_id: &str, mode: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set_permission_mode:{session_id}:{mode}"));
            Ok(())
        }
        fn respond_user_dialog(
            &self,
            request_id: &str,
            behavior: &str,
            _result: Option<Value>,
        ) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("dialog:{request_id}:{behavior}"));
            Ok(())
        }
    }

    fn setup() -> (Arc<SessionManager>, EventBus, Arc<MockEngine>) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = Arc::new(MockEngine::default());
        let manager = Arc::new(SessionManager::new(store, bus.clone(), engine.clone()));
        (manager, bus, engine)
    }

    /// Manager with the notes finalize step armed, plus a temp repo so a worktree exists.
    fn setup_with_notes(
        timeout_secs: &str,
    ) -> (
        Arc<SessionManager>,
        EventBus,
        Arc<MockEngine>,
        tempfile::TempDir,
    ) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .set_setting(SETTING_NOTES_FINALIZE_TIMEOUT, timeout_secs)
            .unwrap();
        let engine = Arc::new(MockEngine::default());
        let dir = tempfile::tempdir().unwrap();
        let worktrees = Arc::new(crate::core::worktree::WorktreeManager::new(
            Arc::new(crate::core::worktree::GitCli),
            store.clone(),
            bus.clone(),
        ));
        let notes = Arc::new(crate::core::notes::NotesManager::new(
            worktrees,
            bus.clone(),
        ));
        let prompts =
            Arc::new(crate::core::prompts::PromptManager::new(dir.path().join("prompts")).unwrap());
        let manager = Arc::new(
            SessionManager::new(store, bus.clone(), engine.clone()).with_notes(notes, prompts),
        );
        (manager, bus, engine, dir)
    }

    /// Drive a spawned session to `awaiting_input`, which is where a close can finalize.
    fn make_idle(manager: &SessionManager, session_id: &str) {
        manager.handle_event(SidecarEvent::Status {
            session_id: session_id.to_string(),
            status: "awaiting_input".into(),
        });
    }

    fn spawn_params(branch: &str) -> SpawnParams {
        SpawnParams {
            branch: branch.into(),
            cwd: ".".into(),
            session_type: SessionType::Manual,
            model: None,
            effort: None,
            permission_mode: None,
            thinking: None,
            prompt: "hi".into(),
            resume_from: None,
        }
    }

    #[tokio::test]
    async fn single_writer_downgrades_second_session() {
        let (manager, _bus, engine) = setup();

        let first = manager.spawn(spawn_params("impl/T-8-x")).unwrap();
        assert!(first.is_writer());

        let second = manager.spawn(spawn_params("impl/T-8-x")).unwrap();
        assert!(!second.is_writer(), "second session must be read-only");
        assert_eq!(second.permission_mode.as_deref(), Some(READ_ONLY_MODE));

        // The engine received the downgraded mode too.
        let spawns = engine.spawns.lock().unwrap();
        assert_eq!(spawns[1].permission_mode.as_deref(), Some(READ_ONLY_MODE));

        // A different branch still gets a writer.
        drop(spawns);
        let other = manager.spawn(spawn_params("impl/T-8-y")).unwrap();
        assert!(other.is_writer());
    }

    #[tokio::test]
    async fn single_writer_reject_policy() {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store
            .set_setting(SETTING_SINGLE_WRITER_POLICY, "reject")
            .unwrap();
        let engine = Arc::new(MockEngine::default());
        let manager = SessionManager::new(store, bus, engine);

        manager.spawn(spawn_params("impl/T-9-x")).unwrap();
        let err = manager
            .spawn(spawn_params("impl/T-9-x"))
            .expect_err("second writer must be rejected");
        assert!(err.to_string().contains("already active"));

        // An explicitly read-only session is still allowed.
        let mut ro = spawn_params("impl/T-9-x");
        ro.permission_mode = Some(READ_ONLY_MODE.into());
        assert!(manager.spawn(ro).is_ok());
    }

    #[tokio::test]
    async fn writer_slot_frees_when_writer_finishes() {
        let (manager, _bus, _engine) = setup();

        let first = manager.spawn(spawn_params("impl/T-10-x")).unwrap();
        manager.handle_event(SidecarEvent::Status {
            session_id: first.id.clone(),
            status: "streaming".into(),
        });
        manager.close(&first.id).unwrap();
        manager.handle_event(SidecarEvent::SessionClosed {
            session_id: first.id.clone(),
            reason: "closed".into(),
        });

        let second = manager.spawn(spawn_params("impl/T-10-x")).unwrap();
        assert!(second.is_writer(), "slot freed after the writer finished");
    }

    #[tokio::test]
    async fn resume_uses_stored_sdk_id_and_inherits_model() {
        let (manager, _bus, engine) = setup();

        let mut params = spawn_params("impl/T-11-x");
        params.model = Some("claude-opus-5".into());
        let source = manager.spawn(params).unwrap();
        manager.handle_event(SidecarEvent::SessionInit {
            session_id: source.id.clone(),
            sdk_session_id: "sdk-resume-me".into(),
            model: None,
        });
        // Finish the source session.
        manager.close(&source.id).unwrap();
        manager.handle_event(SidecarEvent::SessionClosed {
            session_id: source.id.clone(),
            reason: "closed".into(),
        });

        let mut resume = spawn_params("impl/T-11-x");
        resume.prompt = String::new();
        resume.resume_from = Some(source.id.clone());
        let resumed = manager.spawn(resume).unwrap();
        assert_eq!(
            resumed.model.as_deref(),
            Some("claude-opus-5"),
            "model inherited"
        );

        let spawns = engine.spawns.lock().unwrap();
        let last = spawns.last().unwrap();
        assert_eq!(last.resume_id.as_deref(), Some("sdk-resume-me"));
    }

    #[tokio::test]
    async fn spawn_persists_and_publishes() {
        let (manager, bus, engine) = setup();
        let mut rx = bus.subscribe();

        let session = manager.spawn(spawn_params("impl/T-1-x")).unwrap();
        assert_eq!(session.status, SessionStatus::Spawning);
        assert!(engine.calls.lock().unwrap()[0].starts_with("spawn:"));

        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "session.status_changed");

        let stored = manager.list_for_branch("impl/T-1-x").unwrap();
        assert_eq!(stored.len(), 1);
    }

    #[tokio::test]
    async fn status_events_drive_the_state_machine() {
        let (manager, _bus, _engine) = setup();
        let session = manager.spawn(spawn_params("impl/T-2-x")).unwrap();

        manager.handle_event(SidecarEvent::Status {
            session_id: session.id.clone(),
            status: "streaming".into(),
        });
        manager.handle_event(SidecarEvent::Status {
            session_id: session.id.clone(),
            status: "awaiting_input".into(),
        });

        let stored = manager.list_for_branch("impl/T-2-x").unwrap();
        assert_eq!(stored[0].status, SessionStatus::AwaitingInput);

        // An invalid transition (awaiting_input → awaiting_input) is ignored.
        manager.handle_event(SidecarEvent::Status {
            session_id: session.id.clone(),
            status: "awaiting_input".into(),
        });
        let stored = manager.list_for_branch("impl/T-2-x").unwrap();
        assert_eq!(stored[0].status, SessionStatus::AwaitingInput);
    }

    #[tokio::test]
    async fn sdk_session_id_is_persisted() {
        let (manager, _bus, _engine) = setup();
        let session = manager.spawn(spawn_params("impl/T-3-x")).unwrap();

        manager.handle_event(SidecarEvent::SessionInit {
            session_id: session.id.clone(),
            sdk_session_id: "sdk-abc".into(),
            model: Some("claude-opus-5".into()),
        });

        let stored = manager.list_for_branch("impl/T-3-x").unwrap();
        assert_eq!(stored[0].sdk_session_id.as_deref(), Some("sdk-abc"));
    }

    #[tokio::test]
    async fn close_while_idle_is_done_close_while_streaming_is_cancelled() {
        let (manager, _bus, _engine) = setup();

        // Idle close → done.
        let s1 = manager.spawn(spawn_params("impl/T-4-x")).unwrap();
        manager.handle_event(SidecarEvent::Status {
            session_id: s1.id.clone(),
            status: "streaming".into(),
        });
        manager.handle_event(SidecarEvent::Status {
            session_id: s1.id.clone(),
            status: "awaiting_input".into(),
        });
        manager.close(&s1.id).unwrap();
        manager.handle_event(SidecarEvent::SessionClosed {
            session_id: s1.id.clone(),
            reason: "closed".into(),
        });

        // Mid-stream close → cancelled.
        let s2 = manager.spawn(spawn_params("impl/T-4-x")).unwrap();
        manager.handle_event(SidecarEvent::Status {
            session_id: s2.id.clone(),
            status: "streaming".into(),
        });
        manager.close(&s2.id).unwrap();
        manager.handle_event(SidecarEvent::SessionClosed {
            session_id: s2.id.clone(),
            reason: "closed".into(),
        });

        let stored = manager.list_for_branch("impl/T-4-x").unwrap();
        let s1_row = stored.iter().find(|s| s.id == s1.id).unwrap();
        let s2_row = stored.iter().find(|s| s.id == s2.id).unwrap();
        assert_eq!(s1_row.status, SessionStatus::Done);
        assert_eq!(s2_row.status, SessionStatus::Cancelled);
    }

    #[tokio::test]
    async fn close_and_delete_of_finished_sessions() {
        let (manager, _bus, _engine) = setup();
        let session = manager.spawn(spawn_params("impl/T-7-x")).unwrap();

        // Deleting an active session is rejected.
        assert!(manager.delete(&session.id).is_err());

        // Fail it (e.g. crash), then close must be a no-op, delete must work.
        manager.handle_crash(Some(1));
        assert!(
            manager.close(&session.id).is_ok(),
            "close on failed is idempotent"
        );
        manager.delete(&session.id).unwrap();
        assert!(manager.list_for_branch("impl/T-7-x").unwrap().is_empty());
        // Double-delete is fine.
        manager.delete(&session.id).unwrap();
    }

    #[tokio::test]
    async fn crash_fails_all_live_sessions() {
        let (manager, _bus, _engine) = setup();
        let s1 = manager.spawn(spawn_params("impl/T-5-x")).unwrap();
        let s2 = manager.spawn(spawn_params("impl/T-5-x")).unwrap();
        manager.handle_event(SidecarEvent::Status {
            session_id: s1.id.clone(),
            status: "streaming".into(),
        });

        manager.handle_crash(Some(13));

        let stored = manager.list_for_branch("impl/T-5-x").unwrap();
        assert!(stored.iter().all(|s| s.status == SessionStatus::Failed));
        assert!(
            manager.send(&s2.id, "hi", &[]).is_err(),
            "dead sessions reject sends"
        );
    }

    fn setup_with_gates(
        rules: Vec<Box<dyn crate::core::gate::GateRule>>,
    ) -> (
        Arc<SessionManager>,
        Arc<crate::core::gate::GateManager>,
        EventBus,
        Arc<MockEngine>,
    ) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = Arc::new(MockEngine::default());
        let mut registry = crate::core::gate::GateRegistry::new();
        for rule in rules {
            registry.register(rule);
        }
        let gates = Arc::new(crate::core::gate::GateManager::new(
            registry,
            engine.clone(),
            bus.clone(),
        ));
        let manager = Arc::new(SessionManager::with_gates(
            store,
            bus.clone(),
            engine.clone(),
            Some(gates.clone()),
        ));
        (manager, gates, bus, engine)
    }

    #[tokio::test]
    async fn gated_permission_pauses_as_gate_pending() {
        let (manager, gates, bus, engine) =
            setup_with_gates(vec![Box::new(crate::core::gate::GitPushRule)]);
        let session = manager.spawn(spawn_params("impl/T-7-gate")).unwrap();

        let mut rx = bus.subscribe();
        manager.handle_event(SidecarEvent::PermissionRequest {
            session_id: session.id.clone(),
            request_id: "req-push".into(),
            tool: "Bash".into(),
            args: serde_json::json!({ "command": "git push origin main" }),
            title: None,
        });

        // The gate event replaces the plain permission request entirely.
        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "gate.pending");

        let pending = gates.list().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].branch, "impl/T-7-gate",
            "branch from runtime map"
        );
        assert_eq!(pending[0].session_id, session.id);

        // Approving executes with the (unedited) args passed back to the engine.
        gates.respond(&pending[0].gate_id, true, &[], None).unwrap();
        let perms = engine.perms.lock().unwrap();
        let (request_id, allow, updated_args, _) = &perms[0];
        assert_eq!(request_id, "req-push");
        assert!(*allow);
        assert_eq!(
            updated_args.as_ref().unwrap()["command"],
            "git push origin main"
        );
        drop(perms);
        assert!(gates.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn non_gated_permission_falls_through_unchanged() {
        let (manager, gates, bus, _engine) =
            setup_with_gates(vec![Box::new(crate::core::gate::GitPushRule)]);
        let session = manager.spawn(spawn_params("impl/T-7-plain")).unwrap();

        let mut rx = bus.subscribe();
        manager.handle_event(SidecarEvent::PermissionRequest {
            session_id: session.id.clone(),
            request_id: "req-ls".into(),
            tool: "Bash".into(),
            args: serde_json::json!({ "command": "ls -la" }),
            title: Some("List files".into()),
        });

        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "session.permission_request");
        assert!(gates.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn closing_an_implementation_session_asks_for_notes_first() {
        let (manager, _bus, engine, _dir) = setup_with_notes("120");
        let mut params = spawn_params("impl/S2-1");
        params.session_type = SessionType::Implementation;
        let session = manager.spawn(params).unwrap();
        make_idle(&manager, &session.id);

        manager.close(&session.id).unwrap();

        // The finalize prompt went out and the session is *not* closed yet.
        let calls = engine.calls.lock().unwrap().clone();
        assert!(
            calls.contains(&format!("send:{}", session.id)),
            "expected a finalize prompt, got {calls:?}"
        );
        assert!(!calls.contains(&format!("close:{}", session.id)));

        // When that turn ends, the close happens for real.
        make_idle(&manager, &session.id);
        let calls = engine.calls.lock().unwrap().clone();
        assert!(calls.contains(&format!("close:{}", session.id)));
    }

    #[tokio::test]
    async fn other_session_types_close_untouched() {
        let (manager, _bus, engine, _dir) = setup_with_notes("120");
        for session_type in [SessionType::Manual, SessionType::Research] {
            let mut params = spawn_params("impl/S2-2");
            params.session_type = session_type;
            params.permission_mode = Some(READ_ONLY_MODE.to_string());
            let session = manager.spawn(params).unwrap();
            make_idle(&manager, &session.id);
            manager.close(&session.id).unwrap();

            let calls = engine.calls.lock().unwrap().clone();
            assert!(
                calls.contains(&format!("close:{}", session.id)),
                "{session_type:?} must close immediately"
            );
            assert!(
                !calls.contains(&format!("send:{}", session.id)),
                "{session_type:?} must not be asked for notes"
            );
        }
    }

    #[tokio::test]
    async fn a_streaming_or_terminal_session_closes_immediately() {
        let (manager, _bus, engine, _dir) = setup_with_notes("120");

        // Streaming: the user asked it to stop, so do not queue another turn behind that.
        let mut params = spawn_params("impl/S2-3");
        params.session_type = SessionType::Implementation;
        let streaming = manager.spawn(params).unwrap();
        manager.handle_event(SidecarEvent::Status {
            session_id: streaming.id.clone(),
            status: "streaming".into(),
        });
        manager.close(&streaming.id).unwrap();
        assert!(engine
            .calls
            .lock()
            .unwrap()
            .contains(&format!("close:{}", streaming.id)));

        // Terminal: close is an idempotent no-op, notes or not.
        let mut params = spawn_params("impl/S2-3b");
        params.session_type = SessionType::Implementation;
        let done = manager.spawn(params).unwrap();
        make_idle(&manager, &done.id);
        manager.handle_event(SidecarEvent::SessionClosed {
            session_id: done.id.clone(),
            reason: "closed".into(),
        });
        let before = engine.calls.lock().unwrap().len();
        manager.close(&done.id).unwrap();
        assert_eq!(engine.calls.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn a_finalize_turn_that_never_ends_still_closes() {
        // Zero-second timeout: the deadline is already past when the sweep runs, which is
        // the same path a stuck agent takes 120 seconds later.
        let (manager, _bus, engine, _dir) = setup_with_notes("1");
        let mut params = spawn_params("impl/S2-4");
        params.session_type = SessionType::Implementation;
        let session = manager.spawn(params).unwrap();
        make_idle(&manager, &session.id);
        manager.close(&session.id).unwrap();
        assert!(!engine
            .calls
            .lock()
            .unwrap()
            .contains(&format!("close:{}", session.id)));

        // Move the deadline into the past and sweep, as the signal loop does.
        manager.finalizing.lock().unwrap().insert(
            session.id.clone(),
            Utc::now() - chrono::Duration::seconds(1),
        );
        manager.sweep_finalize_deadlines();
        assert!(engine
            .calls
            .lock()
            .unwrap()
            .contains(&format!("close:{}", session.id)));
    }

    #[tokio::test]
    async fn the_finalize_step_can_be_switched_off() {
        let (manager, _bus, engine, _dir) = setup_with_notes("0");
        let mut params = spawn_params("impl/S2-5");
        params.session_type = SessionType::Implementation;
        let session = manager.spawn(params).unwrap();
        make_idle(&manager, &session.id);
        manager.close(&session.id).unwrap();

        let calls = engine.calls.lock().unwrap().clone();
        assert!(calls.contains(&format!("close:{}", session.id)));
        assert!(!calls.contains(&format!("send:{}", session.id)));
    }

    #[tokio::test]
    async fn stale_sessions_failed_at_startup() {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.upsert_branch("impl/T-6-x", None, None).unwrap();
        let orphan = Session::new("impl/T-6-x", SessionType::Manual, None, None, None);
        store.insert_session(&orphan).unwrap();

        let manager = SessionManager::new(store.clone(), bus, Arc::new(MockEngine::default()));
        manager.fail_stale_sessions("startup");

        let stored = store.list_sessions("impl/T-6-x").unwrap();
        assert_eq!(stored[0].status, SessionStatus::Failed);
    }
    #[tokio::test]
    async fn runtime_model_and_effort_reach_engine_store_and_bus() {
        let (manager, bus, engine) = setup();
        let session = manager.spawn(spawn_params("impl/S3-a")).unwrap();
        let mut rx = bus.subscribe();

        manager.set_model(&session.id, "claude-opus-5").unwrap();
        manager.set_effort(&session.id, "high").unwrap();

        let calls = engine.calls.lock().unwrap().clone();
        assert!(calls.contains(&format!("set_model:{}:claude-opus-5", session.id)));
        assert!(calls.contains(&format!("set_effort:{}:high", session.id)));

        // Persisted, so history and a later resume agree with the UI.
        let stored = manager.store.get_session(&session.id).unwrap().unwrap();
        assert_eq!(stored.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(stored.effort.as_deref(), Some("high"));

        let first = rx.recv().await.unwrap();
        assert_eq!(first.name(), "session.settings_changed");
    }

    #[tokio::test]
    async fn thinking_is_validated_persisted_and_announced() {
        let (manager, bus, engine) = setup();
        let session = manager.spawn(spawn_params("impl/S3-e")).unwrap();
        let mut rx = bus.subscribe();

        // Only the offered budgets are accepted; a typo must not reach the CLI.
        let err = manager
            .set_thinking(&session.id, "4k")
            .expect_err("unknown budget");
        assert_eq!(err.code(), "invalid_data");

        manager.set_thinking(&session.id, "16000").unwrap();
        assert!(engine
            .calls
            .lock()
            .unwrap()
            .contains(&format!("set_thinking:{}:16000", session.id)));
        let stored = manager.store.get_session(&session.id).unwrap().unwrap();
        assert_eq!(stored.thinking.as_deref(), Some("16000"));

        match rx.recv().await.unwrap() {
            Event::SessionSettingsChanged { thinking, .. } => {
                assert_eq!(thinking.as_deref(), Some("16000"));
            }
            other => panic!("expected settings_changed, got {}", other.name()),
        }
    }

    #[tokio::test]
    async fn resume_inherits_thinking_budget() {
        let (manager, _bus, engine) = setup();
        let mut params = spawn_params("impl/S3-f");
        params.thinking = Some("32000".into());
        let first = manager.spawn(params).unwrap();
        manager.handle_event(SidecarEvent::SessionInit {
            session_id: first.id.clone(),
            sdk_session_id: "sdk-1".into(),
            model: None,
        });
        manager.close(&first.id).unwrap();
        manager.handle_event(SidecarEvent::SessionClosed {
            session_id: first.id.clone(),
            reason: "closed".into(),
        });

        let mut resume = spawn_params("impl/S3-f");
        resume.resume_from = Some(first.id.clone());
        let second = manager.spawn(resume).unwrap();
        assert_eq!(second.thinking.as_deref(), Some("32000"));
        let spawns = engine.spawns.lock().unwrap();
        assert_eq!(spawns.last().unwrap().thinking.as_deref(), Some("32000"));
    }

    #[tokio::test]
    async fn runtime_settings_rejected_for_unknown_session() {
        let (manager, _bus, _engine) = setup();
        assert!(manager.set_model("nope", "claude-opus-5").is_err());
        assert!(manager.set_effort("nope", "high").is_err());
        assert!(manager.set_permission_mode("nope", "acceptEdits").is_err());
    }

    #[tokio::test]
    async fn permission_mode_switch_respects_single_writer() {
        let (manager, _bus, engine) = setup();
        let writer = manager.spawn(spawn_params("impl/S3-b")).unwrap();
        let reader = manager.spawn(spawn_params("impl/S3-b")).unwrap();
        assert!(writer.is_writer());
        assert!(!reader.is_writer());

        // The reader cannot grab the write slot while the writer is alive.
        let err = manager
            .set_permission_mode(&reader.id, "acceptEdits")
            .expect_err("second writer must be refused");
        assert_eq!(err.code(), "invalid_data");
        assert!(!engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("set_permission_mode:")));

        // Going read-only is always allowed, and frees the slot when the writer does it.
        manager
            .set_permission_mode(&writer.id, READ_ONLY_MODE)
            .unwrap();
        manager
            .set_permission_mode(&reader.id, "acceptEdits")
            .unwrap();
        let stored = manager.store.get_session(&reader.id).unwrap().unwrap();
        assert_eq!(stored.permission_mode.as_deref(), Some("acceptEdits"));
    }

    #[tokio::test]
    async fn approving_a_plan_claims_the_writer_slot() {
        let (manager, _bus, engine) = setup();
        // A writer already holds the branch, so the planner starts read-only.
        let writer = manager.spawn(spawn_params("impl/S3-g")).unwrap();
        let planner = manager.spawn(spawn_params("impl/S3-g")).unwrap();
        assert!(!planner.is_writer());

        manager.handle_event(SidecarEvent::UserDialogRequest {
            session_id: planner.id.clone(),
            request_id: "plan-1".into(),
            dialog_kind: "plan_approval".into(),
            payload: serde_json::json!({ "plan": "do the thing" }),
            tool_use_id: None,
        });

        // Approving would make a second writer on the branch: refused, and the CLI is
        // never told, so the agent stays in plan mode.
        let err = manager
            .respond_user_dialog("plan-1", Some(serde_json::json!({ "approved": true })))
            .expect_err("second writer must be refused");
        assert_eq!(err.code(), "invalid_data");
        assert!(!engine
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("dialog:plan-1")));

        // With the slot free, the same approval promotes the session and reaches the CLI.
        manager.close(&writer.id).unwrap();
        manager.handle_event(SidecarEvent::SessionClosed {
            session_id: writer.id.clone(),
            reason: "closed".into(),
        });
        manager
            .respond_user_dialog("plan-1", Some(serde_json::json!({ "approved": true })))
            .unwrap();
        let stored = manager.store.get_session(&planner.id).unwrap().unwrap();
        assert_eq!(stored.permission_mode.as_deref(), Some("acceptEdits"));
        assert!(engine
            .calls
            .lock()
            .unwrap()
            .contains(&"dialog:plan-1:completed".to_string()));
    }

    #[tokio::test]
    async fn rejecting_a_plan_leaves_the_session_read_only() {
        let (manager, _bus, _engine) = setup();
        let mut params = spawn_params("impl/S3-h");
        params.permission_mode = Some(READ_ONLY_MODE.to_string());
        let planner = manager.spawn(params).unwrap();

        manager.handle_event(SidecarEvent::UserDialogRequest {
            session_id: planner.id.clone(),
            request_id: "plan-2".into(),
            dialog_kind: "plan_approval".into(),
            payload: serde_json::json!({ "plan": "do the thing" }),
            tool_use_id: None,
        });
        manager
            .respond_user_dialog(
                "plan-2",
                Some(serde_json::json!({ "approved": false, "feedback": "narrow it down" })),
            )
            .unwrap();

        let stored = manager.store.get_session(&planner.id).unwrap().unwrap();
        assert_eq!(stored.permission_mode.as_deref(), Some(READ_ONLY_MODE));
    }

    #[tokio::test]
    async fn mcp_actions_are_validated_and_forwarded() {
        let (manager, _bus, engine) = setup();
        let session = manager.spawn(spawn_params("impl/S3-i")).unwrap();

        assert_eq!(
            manager
                .mcp_action(&session.id, "srv", "restart")
                .expect_err("unknown action")
                .code(),
            "invalid_data"
        );
        assert!(manager.mcp_action("nope", "srv", "reconnect").is_err());

        manager.mcp_action(&session.id, "srv", "reconnect").unwrap();
        manager.mcp_action(&session.id, "srv", "disable").unwrap();
        let calls = engine.calls.lock().unwrap().clone();
        assert!(calls.contains(&format!("mcp:{}:srv:reconnect", session.id)));
        assert!(calls.contains(&format!("mcp:{}:srv:disable", session.id)));
    }

    #[tokio::test]
    async fn user_dialog_request_is_published_and_answered() {
        let (manager, bus, engine) = setup();
        let session = manager.spawn(spawn_params("impl/S3-c")).unwrap();
        let mut rx = bus.subscribe();

        manager.handle_event(SidecarEvent::UserDialogRequest {
            session_id: session.id.clone(),
            request_id: "dlg-1".into(),
            dialog_kind: "ask_user_question".into(),
            payload: serde_json::json!({ "questions": [] }),
            tool_use_id: None,
        });
        let event = rx.recv().await.unwrap();
        assert_eq!(event.name(), "session.user_dialog");

        manager
            .respond_user_dialog("dlg-1", Some(serde_json::json!({ "answers": ["a"] })))
            .unwrap();
        // The resolution names the session, so the attention queue can clear its entry.
        match rx.recv().await.unwrap() {
            Event::SessionUserDialogResolved {
                session_id,
                request_id,
            } => {
                assert_eq!(session_id, session.id);
                assert_eq!(request_id, "dlg-1");
            }
            other => panic!("expected a resolved event, got {}", other.name()),
        }
        manager.respond_user_dialog("dlg-2", None).unwrap();
        let calls = engine.calls.lock().unwrap().clone();
        assert!(calls.contains(&"dialog:dlg-1:completed".to_string()));
        assert!(calls.contains(&"dialog:dlg-2:cancelled".to_string()));
    }
}
