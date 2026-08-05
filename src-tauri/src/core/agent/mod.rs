//! Agent engine boundary and the sidecar supervisor.
//!
//! [`AgentEngine`] is the trait the session layer depends on; [`SidecarEngine`] is the
//! concrete impl that launches and supervises the Node sidecar (Claude Agent SDK).
//! The supervisor restarts the process lazily after a crash: the crash is signalled to
//! the session layer (which fails affected sessions), and the next request spawns a
//! fresh process.

pub mod protocol;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::{MaestroError, Result};
use protocol::{Attachment, SidecarEvent, SidecarRequest};

/// Everything the session layer needs to know to start a session.
#[derive(Clone, Debug)]
pub struct SpawnSessionRequest {
    pub session_id: String,
    pub cwd: String,
    pub prompt: String,
    pub session_type: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    /// Thinking budget: see [`AgentEngine::set_thinking`].
    pub thinking: Option<String>,
    /// Extra tools this session gets (`review` → `ask_original_agent`).
    pub tools_profile: Option<String>,
    /// Tools this session may not use at all.
    pub disallowed_tools: Vec<String>,
    pub resume_id: Option<String>,
}

/// Signals delivered to the session layer: parsed sidecar events plus process death.
#[derive(Clone, Debug)]
pub enum EngineSignal {
    Event(SidecarEvent),
    Crashed { code: Option<i32> },
}

/// Agent execution boundary. Concrete impl: the Node sidecar. Test doubles implement
/// this trait. Methods are cheap (a line written to the child's stdin); results and
/// progress arrive asynchronously as [`EngineSignal`]s.
pub trait AgentEngine: Send + Sync {
    fn spawn_session(&self, req: SpawnSessionRequest) -> Result<()>;
    fn send_prompt(&self, session_id: &str, prompt: &str, attachments: &[Attachment])
        -> Result<()>;
    fn interrupt(&self, session_id: &str) -> Result<()>;
    fn close_session(&self, session_id: &str) -> Result<()>;
    fn respond_permission(
        &self,
        request_id: &str,
        allow: bool,
        updated_args: Option<Value>,
        message: Option<String>,
    ) -> Result<()>;

    /// Ask the CLI which models it offers; the answer arrives as a `models` event.
    fn list_models(&self, cwd: &str) -> Result<()>;

    /// Runtime knobs of a live session. Empty strings clear model/effort overrides.
    fn set_model(&self, session_id: &str, model: &str) -> Result<()>;
    fn set_effort(&self, session_id: &str, effort: &str) -> Result<()>;
    fn set_permission_mode(&self, session_id: &str, mode: &str) -> Result<()>;
    /// `""`/`default` restores the CLI default, `off` disables thinking, a decimal string
    /// sets a token budget.
    fn set_thinking(&self, session_id: &str, thinking: &str) -> Result<()>;
    /// `reconnect` | `enable` | `disable` on one of the session's MCP servers.
    fn mcp_action(&self, session_id: &str, server: &str, action: &str) -> Result<()>;
    /// Answer an `ask_original_agent` call with text the asking agent can read.
    fn respond_escalation(&self, request_id: &str, result: &str) -> Result<()>;
    /// Answer a paused tool call: `pass` (not gated), `allow` (optionally with edited
    /// arguments) or `deny` with a message for the agent.
    fn respond_gate_check(
        &self,
        request_id: &str,
        decision: &str,
        updated_args: Option<Value>,
        message: Option<String>,
    ) -> Result<()>;

    /// Answer a dialog the CLI asked the host to render.
    fn respond_user_dialog(
        &self,
        request_id: &str,
        behavior: &str,
        result: Option<Value>,
    ) -> Result<()>;
}

/// How to launch the sidecar process.
#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl SidecarConfig {
    /// Resolution order: `MAESTRO_SIDECAR_SCRIPT` env var, then the dev-tree path
    /// (next to the crate), then `sidecar/main.js` beside the executable.
    pub fn resolve() -> Self {
        let script = std::env::var("MAESTRO_SIDECAR_SCRIPT")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sidecar/dist/main.js");
                dev.exists().then_some(dev)
            })
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(|d| d.join("sidecar/main.js")))
            })
            .unwrap_or_else(|| PathBuf::from("sidecar/dist/main.js"));

        Self {
            program: "node".into(),
            args: vec![script.to_string_lossy().into_owned()],
            env: Vec::new(),
        }
    }
}

struct RunningSidecar {
    stdin: ChildStdin,
    generation: u64,
}

struct Shared {
    config: SidecarConfig,
    signal_tx: UnboundedSender<EngineSignal>,
    running: Mutex<Option<RunningSidecar>>,
    /// request id → session id, so failed acks can be attributed to a session.
    pending: Mutex<HashMap<u64, String>>,
    generation: AtomicU64,
}

pub struct SidecarEngine {
    shared: Arc<Shared>,
    next_id: AtomicU64,
}

impl SidecarEngine {
    pub fn new(config: SidecarConfig, signal_tx: UnboundedSender<EngineSignal>) -> Self {
        Self {
            shared: Arc::new(Shared {
                config,
                signal_tx,
                running: Mutex::new(None),
                pending: Mutex::new(HashMap::new()),
                generation: AtomicU64::new(0),
            }),
            next_id: AtomicU64::new(1),
        }
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Serialize and write a request; starts the sidecar if it is not running.
    fn write(&self, request: &SidecarRequest, session_id: Option<&str>) -> Result<()> {
        let mut running = self
            .shared
            .running
            .lock()
            .map_err(|_| internal("sidecar lock poisoned"))?;

        if running.is_none() {
            *running = Some(start_sidecar(&self.shared)?);
        }

        if let (Some(id), Some(session)) = (request_id(request), session_id) {
            if let Ok(mut pending) = self.shared.pending.lock() {
                pending.insert(id, session.to_string());
            }
        }

        let line = serde_json::to_string(request).map_err(|err| internal(&err.to_string()))?;
        let sidecar = running.as_mut().expect("just ensured running");
        let write_result = sidecar
            .stdin
            .write_all(line.as_bytes())
            .and_then(|_| sidecar.stdin.write_all(b"\n"))
            .and_then(|_| sidecar.stdin.flush());

        if let Err(err) = write_result {
            // Pipe broke: the process is dead or dying; the waiter thread reports the
            // crash. Drop our handle so the next request restarts.
            *running = None;
            return Err(MaestroError::InvalidData {
                message: format!("sidecar write failed: {err}"),
            });
        }
        Ok(())
    }
}

fn request_id(request: &SidecarRequest) -> Option<u64> {
    match request {
        SidecarRequest::Spawn { id, .. }
        | SidecarRequest::Send { id, .. }
        | SidecarRequest::Interrupt { id, .. }
        | SidecarRequest::Close { id, .. }
        | SidecarRequest::PermissionResponse { id, .. }
        | SidecarRequest::ListModels { id, .. }
        | SidecarRequest::SetModel { id, .. }
        | SidecarRequest::SetEffort { id, .. }
        | SidecarRequest::SetPermissionMode { id, .. }
        | SidecarRequest::SetThinking { id, .. }
        | SidecarRequest::McpAction { id, .. }
        | SidecarRequest::EscalationResponse { id, .. }
        | SidecarRequest::GateDecision { id, .. }
        | SidecarRequest::UserDialogResponse { id, .. }
        | SidecarRequest::Shutdown { id } => Some(*id),
    }
}

fn internal(message: &str) -> MaestroError {
    MaestroError::InvalidData {
        message: message.to_string(),
    }
}

fn start_sidecar(shared: &Arc<Shared>) -> Result<RunningSidecar> {
    let config = &shared.config;
    let mut cmd = Command::new(&config.program);
    cmd.args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &config.env {
        cmd.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut child = cmd.spawn().map_err(|err| MaestroError::Config {
        message: format!(
            "failed to launch sidecar ({} {}): {err}",
            config.program,
            config.args.join(" ")
        ),
    })?;

    let generation = shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| internal("no sidecar stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| internal("no sidecar stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| internal("no sidecar stderr"))?;

    tracing::info!(generation, program = %config.program, "sidecar started");

    spawn_stdout_reader(shared.clone(), stdout);
    spawn_stderr_reader(stderr);
    spawn_waiter(shared.clone(), child, generation);

    Ok(RunningSidecar { stdin, generation })
}

fn spawn_stdout_reader(shared: Arc<Shared>, stdout: std::process::ChildStdout) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<SidecarEvent>(trimmed) {
                Ok(SidecarEvent::Ready { protocol_version }) => {
                    if protocol_version != protocol::PROTOCOL_VERSION {
                        tracing::error!(
                            sidecar = protocol_version,
                            core = protocol::PROTOCOL_VERSION,
                            "sidecar protocol version mismatch"
                        );
                        // A stale `sidecar/dist` used to fail silently: features simply
                        // did not work. Surface it as an error event so the UI says so —
                        // the engine has no bus, so it travels the signal channel like any
                        // other sidecar problem.
                        let _ = shared.signal_tx.send(EngineSignal::Event(SidecarEvent::Error {
                            session_id: None,
                            message: format!(
                                "the sidecar speaks protocol v{protocol_version}, this build expects v{}; rebuild it with: cd sidecar && npm run build",
                                protocol::PROTOCOL_VERSION
                            ),
                        }));
                    } else {
                        tracing::info!(protocol_version, "sidecar ready");
                    }
                }
                Ok(SidecarEvent::Ack { id, ok, error }) => {
                    let session = shared.pending.lock().ok().and_then(|mut p| p.remove(&id));
                    if !ok {
                        let message = error.unwrap_or_else(|| "request rejected".into());
                        tracing::error!(id, session = ?session, %message, "sidecar nack");
                        if let Some(session_id) = session {
                            // Attribute the failure to the session so it can be failed.
                            let _ =
                                shared
                                    .signal_tx
                                    .send(EngineSignal::Event(SidecarEvent::Error {
                                        session_id: Some(session_id.clone()),
                                        message,
                                    }));
                            let _ = shared.signal_tx.send(EngineSignal::Event(
                                SidecarEvent::SessionClosed {
                                    session_id,
                                    reason: "error".into(),
                                },
                            ));
                        }
                    }
                }
                Ok(event) => {
                    let _ = shared.signal_tx.send(EngineSignal::Event(event));
                }
                Err(err) => {
                    tracing::warn!(error = %err, line = trimmed, "unparseable sidecar event");
                }
            }
        }
        tracing::debug!("sidecar stdout closed");
    });
}

fn spawn_stderr_reader(stderr: std::process::ChildStderr) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if !line.trim().is_empty() {
                tracing::warn!(target: "sidecar", "{line}");
            }
        }
    });
}

fn spawn_waiter(shared: Arc<Shared>, mut child: Child, generation: u64) {
    std::thread::spawn(move || {
        let code = child.wait().ok().and_then(|status| status.code());
        let is_current = shared
            .running
            .lock()
            .map(|mut running| {
                if running.as_ref().is_some_and(|r| r.generation == generation) {
                    *running = None;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);

        // Exit code 0 with a deliberate shutdown is fine; anything else while we
        // still considered the process current is a crash.
        if is_current {
            tracing::error!(?code, generation, "sidecar exited unexpectedly");
            let _ = shared.signal_tx.send(EngineSignal::Crashed { code });
        } else {
            tracing::info!(?code, generation, "old sidecar process exited");
        }
    });
}

impl AgentEngine for SidecarEngine {
    fn spawn_session(&self, req: SpawnSessionRequest) -> Result<()> {
        let request = SidecarRequest::Spawn {
            id: self.next_request_id(),
            session_id: req.session_id.clone(),
            cwd: req.cwd,
            prompt: req.prompt,
            session_type: req.session_type,
            model: req.model,
            effort: req.effort,
            permission_mode: req.permission_mode,
            thinking: req.thinking,
            tools_profile: req.tools_profile,
            disallowed_tools: req.disallowed_tools,
            resume_id: req.resume_id,
        };
        self.write(&request, Some(&req.session_id))
    }

    fn send_prompt(
        &self,
        session_id: &str,
        prompt: &str,
        attachments: &[Attachment],
    ) -> Result<()> {
        let request = SidecarRequest::Send {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
            prompt: prompt.to_string(),
            attachments: attachments.to_vec(),
        };
        self.write(&request, Some(session_id))
    }

    fn interrupt(&self, session_id: &str) -> Result<()> {
        let request = SidecarRequest::Interrupt {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
        };
        self.write(&request, Some(session_id))
    }

    fn close_session(&self, session_id: &str) -> Result<()> {
        let request = SidecarRequest::Close {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
        };
        self.write(&request, Some(session_id))
    }

    fn respond_permission(
        &self,
        request_id: &str,
        allow: bool,
        updated_args: Option<Value>,
        message: Option<String>,
    ) -> Result<()> {
        let request = SidecarRequest::PermissionResponse {
            id: self.next_request_id(),
            request_id: request_id.to_string(),
            allow,
            updated_args,
            message,
        };
        self.write(&request, None)
    }

    fn list_models(&self, cwd: &str) -> Result<()> {
        let request = SidecarRequest::ListModels {
            id: self.next_request_id(),
            cwd: cwd.to_string(),
        };
        self.write(&request, None)
    }

    fn set_model(&self, session_id: &str, model: &str) -> Result<()> {
        let request = SidecarRequest::SetModel {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
            model: model.to_string(),
        };
        self.write(&request, Some(session_id))
    }

    fn set_effort(&self, session_id: &str, effort: &str) -> Result<()> {
        let request = SidecarRequest::SetEffort {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
            effort: effort.to_string(),
        };
        self.write(&request, Some(session_id))
    }

    fn respond_gate_check(
        &self,
        request_id: &str,
        decision: &str,
        updated_args: Option<Value>,
        message: Option<String>,
    ) -> Result<()> {
        let request = SidecarRequest::GateDecision {
            id: self.next_request_id(),
            request_id: request_id.to_string(),
            decision: decision.to_string(),
            updated_args,
            message,
        };
        self.write(&request, None)
    }

    fn respond_escalation(&self, request_id: &str, result: &str) -> Result<()> {
        let request = SidecarRequest::EscalationResponse {
            id: self.next_request_id(),
            request_id: request_id.to_string(),
            result: result.to_string(),
        };
        self.write(&request, None)
    }

    fn mcp_action(&self, session_id: &str, server: &str, action: &str) -> Result<()> {
        let request = SidecarRequest::McpAction {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
            server: server.to_string(),
            action: action.to_string(),
        };
        self.write(&request, Some(session_id))
    }

    fn set_thinking(&self, session_id: &str, thinking: &str) -> Result<()> {
        let request = SidecarRequest::SetThinking {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
            thinking: thinking.to_string(),
        };
        self.write(&request, Some(session_id))
    }

    fn set_permission_mode(&self, session_id: &str, mode: &str) -> Result<()> {
        let request = SidecarRequest::SetPermissionMode {
            id: self.next_request_id(),
            session_id: session_id.to_string(),
            permission_mode: mode.to_string(),
        };
        self.write(&request, Some(session_id))
    }

    fn respond_user_dialog(
        &self,
        request_id: &str,
        behavior: &str,
        result: Option<Value>,
    ) -> Result<()> {
        let request = SidecarRequest::UserDialogResponse {
            id: self.next_request_id(),
            request_id: request_id.to_string(),
            behavior: behavior.to_string(),
            result,
        };
        self.write(&request, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end against the real Node sidecar in mock mode. Requires the sidecar to
    /// be built (`npm run build` in sidecar/); set MAESTRO_SIDECAR_E2E=1 to enable.
    #[test]
    fn sidecar_mock_end_to_end() {
        if std::env::var("MAESTRO_SIDECAR_E2E").is_err() {
            eprintln!("MAESTRO_SIDECAR_E2E not set; skipping sidecar e2e test");
            return;
        }

        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../sidecar/dist/main.js");
        assert!(script.exists(), "sidecar must be built first: {script:?}");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = SidecarEngine::new(
            SidecarConfig {
                program: "node".into(),
                args: vec![script.to_string_lossy().into_owned()],
                env: vec![("MAESTRO_SIDECAR_MOCK".into(), "1".into())],
            },
            tx,
        );

        // Two parallel sessions streaming simultaneously.
        for (sid, prompt) in [("e2e-1", "first prompt"), ("e2e-2", "second prompt")] {
            engine
                .spawn_session(SpawnSessionRequest {
                    session_id: sid.into(),
                    cwd: ".".into(),
                    prompt: prompt.into(),
                    session_type: "manual".into(),
                    model: None,
                    effort: None,
                    permission_mode: None,
                    thinking: None,
                    tools_profile: None,
                    disallowed_tools: Vec::new(),
                    resume_id: None,
                })
                .expect("spawn");
        }

        let mut inits = 0;
        let mut deltas_1 = 0;
        let mut deltas_2 = 0;
        let mut awaiting = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);

        while awaiting.len() < 2 && std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(EngineSignal::Event(event)) => match event {
                    SidecarEvent::SessionInit { .. } => inits += 1,
                    SidecarEvent::StreamDelta { session_id, .. } => {
                        if session_id == "e2e-1" {
                            deltas_1 += 1;
                        } else {
                            deltas_2 += 1;
                        }
                    }
                    SidecarEvent::Status { session_id, status } if status == "awaiting_input" => {
                        awaiting.insert(session_id);
                    }
                    _ => {}
                },
                Ok(EngineSignal::Crashed { .. }) => panic!("sidecar crashed unexpectedly"),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }

        assert_eq!(inits, 2, "both sessions initialized");
        assert!(deltas_1 > 0 && deltas_2 > 0, "both sessions streamed");
        assert_eq!(awaiting.len(), 2, "both sessions reached awaiting_input");

        // Crash recovery: a CRASH prompt kills the process; we must observe Crashed.
        engine
            .send_prompt("e2e-1", "please CRASH now", &[])
            .expect("send");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut crashed = false;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(EngineSignal::Crashed { .. }) => {
                    crashed = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
        assert!(crashed, "supervisor must report the crash");

        // And the engine restarts on the next request.
        engine
            .spawn_session(SpawnSessionRequest {
                session_id: "e2e-3".into(),
                cwd: ".".into(),
                prompt: "after restart".into(),
                session_type: "manual".into(),
                model: None,
                effort: None,
                permission_mode: None,
                thinking: None,
                tools_profile: None,
                disallowed_tools: Vec::new(),
                resume_id: None,
            })
            .expect("spawn after crash");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut recovered = false;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(EngineSignal::Event(SidecarEvent::SessionInit { session_id, .. }))
                    if session_id == "e2e-3" =>
                {
                    recovered = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
        assert!(recovered, "sidecar must restart after a crash");
    }
}
