//! Session lifecycle manager.
//!
//! Owns the mapping session ↔ branch ↔ runtime status, validates every state-machine
//! transition, persists rows, and publishes `session.*` events on the bus. Consumes
//! [`EngineSignal`]s produced by the agent engine's supervisor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::core::agent::protocol::SidecarEvent;
use crate::core::agent::{AgentEngine, EngineSignal, SpawnSessionRequest};
use crate::core::bus::{Event, EventBus};
use crate::core::gate::GateManager;
use crate::core::session::{is_writer_mode, Session, SessionStatus, SessionType, READ_ONLY_MODE};
use crate::core::store::Store;
use crate::error::{MaestroError, Result, Severity};

/// Setting key for what happens when a second writer is requested on a branch.
pub const SETTING_SINGLE_WRITER_POLICY: &str = "single_writer_policy";

/// Everything the IPC layer passes in to start a session.
#[derive(Clone, Debug)]
pub struct SpawnParams {
    pub branch: String,
    pub cwd: String,
    pub session_type: SessionType,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
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
    runtime: Mutex<HashMap<String, RuntimeSession>>,
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
            runtime: Mutex::new(HashMap::new()),
        }
    }

    /// Consume engine signals until the channel closes. Run as a background task.
    pub async fn run_loop(self: Arc<Self>, mut rx: UnboundedReceiver<EngineSignal>) {
        while let Some(signal) = rx.recv().await {
            match signal {
                EngineSignal::Event(event) => self.handle_event(event),
                EngineSignal::Crashed { code } => self.handle_crash(code),
            }
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

        let session = Session::new(
            params.branch.clone(),
            params.session_type,
            params.model.clone(),
            params.effort.clone(),
            permission_mode.clone(),
        );
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

    pub fn send(&self, session_id: &str, prompt: &str) -> Result<()> {
        self.ensure_live(session_id)?;
        self.engine.send_prompt(session_id, prompt)
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
        self.engine.close_session(session_id)
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
            }
            SidecarEvent::StreamDelta { session_id, text } => {
                self.bus
                    .publish(Event::SessionStreamDelta { session_id, text });
            }
            SidecarEvent::ToolUse {
                session_id,
                name,
                summary,
            } => {
                self.bus.publish(Event::SessionToolUse {
                    session_id,
                    name,
                    summary,
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
        fn send_prompt(&self, session_id: &str, _prompt: &str) -> Result<()> {
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
    }

    fn setup() -> (Arc<SessionManager>, EventBus, Arc<MockEngine>) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let engine = Arc::new(MockEngine::default());
        let manager = Arc::new(SessionManager::new(store, bus.clone(), engine.clone()));
        (manager, bus, engine)
    }

    fn spawn_params(branch: &str) -> SpawnParams {
        SpawnParams {
            branch: branch.into(),
            cwd: ".".into(),
            session_type: SessionType::Manual,
            model: None,
            effort: None,
            permission_mode: None,
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
            manager.send(&s2.id, "hi").is_err(),
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
}
