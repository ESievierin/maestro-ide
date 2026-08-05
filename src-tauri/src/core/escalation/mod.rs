//! `ask_original_agent` — the live fallback when `TASK_NOTES.md` does not answer it (S2-T2).
//!
//! A review session can ask the agent that implemented the branch why it did something. The
//! answer comes from resuming that implementation session's own context, read-only, on a
//! cheap model, for exactly one question.
//!
//! Two rules shape everything here:
//!
//! 1. **Every outcome is text the asking agent can read.** No implementation session, no
//!    resumable context, a spawn failure, a crash mid-flight, a timeout, over budget — all
//!    of them answer the tool call with a sentence explaining what to do instead. A thrown
//!    error would kill the asking turn, which is a far worse failure than a missing answer.
//! 2. **The budget is enforced here, not in the prompt.** Two escalations per asking turn,
//!    counted per session and reset when that session starts a new turn. Prompt wording is
//!    not a limit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast::error::RecvError;

use crate::core::agent::AgentEngine;
use crate::core::bus::{Event, EventBus};
use crate::core::session::{SessionManager, SessionStatus, SessionType, SpawnParams};
use crate::core::store::Store;
use crate::core::worktree::WorktreeManager;
use crate::error::Result;

/// Setting key for how long one escalation may take.
pub const SETTING_ESCALATION_TIMEOUT: &str = "escalation_timeout_secs";

/// Default for [`SETTING_ESCALATION_TIMEOUT`].
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Escalations one asking turn may make. Deliberately small: the notes are the primary
/// channel, and an agent that needs a conversation is a sign the notes were inadequate.
const BUDGET_PER_TURN: u32 = 2;

/// Model every escalation runs on, whatever the original used — the answer is a lookup in
/// context that already exists, not new reasoning.
const ESCALATION_MODEL: &str = "sonnet";

/// Tools an escalated session may never use. Belt and braces: it also runs in plan mode.
const DISALLOWED: [&str; 3] = ["Edit", "Write", "Bash"];

/// What the asking agent is told when there is nothing to ask.
const NO_CONTEXT: &str =
    "Context unavailable: no implementation session for this branch can be resumed. \
     Answer from TASK_NOTES.md and the code.";

pub struct EscalationManager {
    engine: Arc<dyn AgentEngine>,
    store: Arc<dyn Store>,
    sessions: Arc<SessionManager>,
    worktrees: Arc<WorktreeManager>,
    bus: EventBus,
    /// Escalations spent by each asking session in its current turn.
    spent: Mutex<HashMap<String, u32>>,
    /// Weak self-reference, so the synchronous trait call can spawn async work without a
    /// second `Arc` keeping the manager alive forever.
    me: std::sync::OnceLock<std::sync::Weak<EscalationManager>>,
}

impl EscalationManager {
    pub fn new(
        engine: Arc<dyn AgentEngine>,
        store: Arc<dyn Store>,
        sessions: Arc<SessionManager>,
        worktrees: Arc<WorktreeManager>,
        bus: EventBus,
    ) -> Self {
        Self {
            engine,
            store,
            sessions,
            worktrees,
            bus,
            spent: Mutex::new(HashMap::new()),
            me: std::sync::OnceLock::new(),
        }
    }

    /// Hand the manager its own `Arc` once it exists. Call immediately after construction.
    pub fn attach(self: &Arc<Self>) {
        let _ = self.me.set(Arc::downgrade(self));
    }

    /// Watch the bus so a new turn on an asking session refills its budget.
    pub async fn run_loop(self: Arc<Self>, bus: EventBus) {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(Event::SessionStatusChanged {
                    session_id, status, ..
                }) => {
                    // A new turn starts (streaming) or the session is gone: either way its
                    // spent count no longer applies.
                    if status == SessionStatus::Streaming || status.is_terminal() {
                        if let Ok(mut spent) = self.spent.lock() {
                            spent.remove(&session_id);
                        }
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(skipped)) => {
                    // Worst case a budget resets late; the ceiling still holds.
                    tracing::warn!(skipped, "escalation loop lagged behind the bus");
                }
                Err(RecvError::Closed) => break,
            }
        }
    }

    /// Answer one `ask_original_agent` call. Spawned as a task: it waits for another
    /// session's turn, which must not block the engine's signal loop.
    pub async fn handle_request(
        self: Arc<Self>,
        session_id: String,
        request_id: String,
        question: String,
    ) {
        let result = self.answer(&session_id, &question).await;
        if let Err(err) = self.engine.respond_escalation(&request_id, &result.text) {
            // The asking session is gone; nothing to answer, and its tool call died with it.
            tracing::warn!(session_id, error = %err, "escalation answer could not be delivered");
        }
    }

    async fn answer(&self, asking_session_id: &str, question: &str) -> Answer {
        let asking = match self.store.get_session(asking_session_id) {
            Ok(Some(session)) => session,
            _ => return Answer::failed("unknown asking session", NO_CONTEXT),
        };

        if !self.claim_budget(asking_session_id) {
            return Answer::failed(
                "over budget",
                "Context unavailable: this turn already used its escalation budget \
                 (2 questions). Answer from TASK_NOTES.md and the code.",
            );
        }

        // The target is the branch's most recent implementation session with a resumable
        // context. Never research/manual (they did not write the code), never a fan-out.
        let target = match self.resolve_target(&asking.branch) {
            Some(target) => target,
            None => return Answer::failed("no implementation session", NO_CONTEXT),
        };

        let cwd = match self.worktree_path(&asking.branch) {
            Some(path) => path,
            None => {
                return Answer::failed(
                    "no worktree",
                    "Context unavailable: this branch has no worktree to run in. \
                     Answer from TASK_NOTES.md and the code.",
                )
            }
        };

        let spawned = self.sessions.spawn(SpawnParams {
            branch: asking.branch.clone(),
            cwd,
            session_type: SessionType::Escalation,
            model: Some(ESCALATION_MODEL.to_string()),
            effort: None,
            permission_mode: Some(crate::core::session::READ_ONLY_MODE.to_string()),
            thinking: None,
            tools_profile: None,
            disallowed_tools: DISALLOWED.iter().map(|t| t.to_string()).collect(),
            prompt: escalation_prompt(question),
            resume_from: Some(target.clone()),
        });
        let escalated = match spawned {
            Ok(session) => session,
            Err(err) => {
                tracing::warn!(error = %err, "escalation session could not start");
                self.publish_failed(asking_session_id, &target, "spawn failed");
                return Answer::plain(NO_CONTEXT);
            }
        };

        // Announced after the spawn so the event names the session that will answer —
        // which is what the attention queue needs to leave it alone.
        self.bus.publish(Event::EscalationStarted {
            asking_session_id: asking_session_id.to_string(),
            target_session_id: target.clone(),
            escalated_session_id: escalated.id.clone(),
            question: question.to_string(),
        });

        let answer = self.collect_answer(&escalated.id).await;
        // The escalated session exists only for this answer.
        if let Err(err) = self.sessions.close(&escalated.id) {
            tracing::warn!(session_id = escalated.id, error = %err, "closing escalated session failed");
        }

        match answer {
            Some(text) if !text.trim().is_empty() => {
                self.bus.publish(Event::EscalationFinished {
                    asking_session_id: asking_session_id.to_string(),
                    target_session_id: target,
                    chars: text.len() as u32,
                });
                Answer::plain(format!(
                    "The agent that implemented this branch says:\n\n{}",
                    text.trim()
                ))
            }
            _ => {
                self.publish_failed(asking_session_id, &target, "no answer before timeout");
                Answer::plain(
                    "Context unavailable: the original agent did not answer in time. \
                     Answer from TASK_NOTES.md and the code.",
                )
            }
        }
    }

    /// Collect the escalated session's reply: its stream deltas until its turn ends. Same
    /// shape as a line question, and the same reason for being event-driven — the text
    /// arrives in pieces on the bus, and the turn's end is a status transition.
    async fn collect_answer(&self, escalated_id: &str) -> Option<String> {
        let mut rx = self.bus.subscribe();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + self.timeout();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // Stop the turn; its partial text is still worth returning.
                let _ = self.engine.interrupt(escalated_id);
                return if text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                };
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(Event::SessionStreamDelta {
                    session_id,
                    text: delta,
                    parent_tool_use_id,
                })) => {
                    // Subagent chatter is not the answer.
                    if session_id == escalated_id && parent_tool_use_id.is_none() {
                        text.push_str(&delta);
                    }
                }
                Ok(Ok(Event::SessionStatusChanged {
                    session_id, status, ..
                })) if session_id == escalated_id => {
                    // The turn ended (idle again) or the session died.
                    if status == SessionStatus::AwaitingInput || status.is_terminal() {
                        return Some(text);
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(RecvError::Lagged(skipped))) => {
                    tracing::warn!(skipped, "escalation answer collector lagged");
                }
                Ok(Err(RecvError::Closed)) => return None,
                Err(_) => {
                    let _ = self.engine.interrupt(escalated_id);
                    return if text.trim().is_empty() {
                        None
                    } else {
                        Some(text)
                    };
                }
            }
        }
    }

    /// Latest implementation session of the branch that has a resumable SDK context.
    fn resolve_target(&self, branch: &str) -> Option<String> {
        let mut candidates: Vec<_> = self
            .store
            .list_sessions(branch)
            .ok()?
            .into_iter()
            .filter(|s| s.session_type == SessionType::Implementation && s.sdk_session_id.is_some())
            .collect();
        candidates.sort_by_key(|s| s.created_at);
        candidates.pop().map(|s| s.id)
    }

    fn worktree_path(&self, branch: &str) -> Option<String> {
        self.worktrees
            .list()
            .ok()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .map(|w| w.path.to_string_lossy().into_owned())
    }

    /// Take one escalation from the asking turn's budget. False when it is used up.
    fn claim_budget(&self, asking_session_id: &str) -> bool {
        let Ok(mut spent) = self.spent.lock() else {
            return false;
        };
        let used = spent.entry(asking_session_id.to_string()).or_insert(0);
        if *used >= BUDGET_PER_TURN {
            return false;
        }
        *used += 1;
        true
    }

    fn publish_failed(&self, asking_session_id: &str, target_session_id: &str, reason: &str) {
        tracing::info!(
            asking_session_id,
            target_session_id,
            reason,
            "escalation failed"
        );
        self.bus.publish(Event::EscalationFailed {
            asking_session_id: asking_session_id.to_string(),
            target_session_id: target_session_id.to_string(),
            reason: reason.to_string(),
        });
    }

    fn timeout(&self) -> Duration {
        let secs = self
            .store
            .get_setting(SETTING_ESCALATION_TIMEOUT)
            .ok()
            .flatten()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Duration::from_secs(secs.max(1))
    }
}

/// What goes back to the asking agent, plus whether it counts as a failure for the record.
struct Answer {
    text: String,
}

impl Answer {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    fn failed(reason: &str, text: &str) -> Self {
        tracing::info!(reason, "escalation refused");
        Self { text: text.into() }
    }
}

/// The escalated session's only turn. It resumes the implementation context, so it already
/// knows the work; all it needs is the question and a hard stop on doing anything else.
fn escalation_prompt(question: &str) -> String {
    format!(
        "Another agent is reviewing this branch and asks you, the agent that implemented \
it, one question. You are read-only now: do not edit files, run commands, or start new \
work — just answer from what you did and why.\n\n\
Question: {question}\n\n\
Answer in at most a short paragraph. If you genuinely do not know, say so plainly."
    )
}

impl crate::core::session::manager::EscalationHandler for EscalationManager {
    /// Answering means waiting for another session's whole turn, so it runs as its own
    /// task: the engine's signal loop must never block on an agent.
    fn handle(&self, session_id: String, request_id: String, question: String) {
        let this = self.me.get().and_then(|weak| weak.upgrade());
        match this {
            Some(this) => {
                tauri::async_runtime::spawn(this.handle_request(session_id, request_id, question));
            }
            None => tracing::error!("escalation manager is not initialised"),
        }
    }
}

/// Public for the session manager's routing: it needs the type name, not the internals.
pub fn is_escalation(session_type: SessionType) -> bool {
    session_type == SessionType::Escalation
}

/// Convenience for the IPC layer and tests.
pub fn escalation_budget() -> u32 {
    BUDGET_PER_TURN
}

#[allow(dead_code)]
fn _assert_result_type(_: Result<()>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent::protocol::Attachment;
    use crate::core::agent::SpawnSessionRequest;
    use crate::core::session::Session;
    use crate::core::store::SqliteStore;
    use std::sync::Mutex as StdMutex;

    /// Engine double that records what the escalation layer asked of it.
    #[derive(Default)]
    struct MockEngine {
        spawns: StdMutex<Vec<SpawnSessionRequest>>,
        answers: StdMutex<Vec<(String, String)>>,
        interrupts: StdMutex<Vec<String>>,
    }

    impl AgentEngine for MockEngine {
        fn spawn_session(&self, req: SpawnSessionRequest) -> Result<()> {
            self.spawns.lock().unwrap().push(req);
            Ok(())
        }
        fn send_prompt(&self, _s: &str, _p: &str, _a: &[Attachment]) -> Result<()> {
            Ok(())
        }
        fn interrupt(&self, session_id: &str) -> Result<()> {
            self.interrupts.lock().unwrap().push(session_id.to_string());
            Ok(())
        }
        fn close_session(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }
        fn respond_permission(
            &self,
            _r: &str,
            _a: bool,
            _u: Option<serde_json::Value>,
            _m: Option<String>,
        ) -> Result<()> {
            Ok(())
        }
        fn list_models(&self, _cwd: &str) -> Result<()> {
            Ok(())
        }
        fn set_model(&self, _s: &str, _m: &str) -> Result<()> {
            Ok(())
        }
        fn set_effort(&self, _s: &str, _e: &str) -> Result<()> {
            Ok(())
        }
        fn set_thinking(&self, _s: &str, _t: &str) -> Result<()> {
            Ok(())
        }
        fn respond_gate_check(
            &self,
            _request_id: &str,
            _decision: &str,
            _updated_args: Option<serde_json::Value>,
            _message: Option<String>,
        ) -> Result<()> {
            Ok(())
        }

        fn respond_escalation(&self, request_id: &str, result: &str) -> Result<()> {
            self.answers
                .lock()
                .unwrap()
                .push((request_id.to_string(), result.to_string()));
            Ok(())
        }
        fn mcp_action(&self, _s: &str, _srv: &str, _a: &str) -> Result<()> {
            Ok(())
        }
        fn set_permission_mode(&self, _s: &str, _m: &str) -> Result<()> {
            Ok(())
        }
        fn respond_user_dialog(
            &self,
            _r: &str,
            _b: &str,
            _res: Option<serde_json::Value>,
        ) -> Result<()> {
            Ok(())
        }
    }

    struct Harness {
        escalations: Arc<EscalationManager>,
        sessions: Arc<SessionManager>,
        store: Arc<SqliteStore>,
        engine: Arc<MockEngine>,
        bus: EventBus,
    }

    /// A store with a branch, an asking session, and (optionally) a resumable
    /// implementation session to escalate to. No worktree: `worktree_path` returns None
    /// unless a repo is configured, which is exactly the "no worktree" fallback — tests
    /// that need a target patch the setting themselves.
    fn harness(with_target: bool) -> (Harness, Session) {
        let bus = EventBus::new();
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        store.upsert_branch("impl/S2-T2", None, None).unwrap();
        let engine = Arc::new(MockEngine::default());
        let sessions = Arc::new(SessionManager::new(
            store.clone(),
            bus.clone(),
            engine.clone(),
        ));
        let worktrees = Arc::new(crate::core::worktree::WorktreeManager::new(
            Arc::new(crate::core::worktree::GitCli),
            store.clone(),
            bus.clone(),
        ));
        let escalations = Arc::new(EscalationManager::new(
            engine.clone(),
            store.clone(),
            sessions.clone(),
            worktrees,
            bus.clone(),
        ));
        escalations.attach();

        if with_target {
            let mut target =
                Session::new("impl/S2-T2", SessionType::Implementation, None, None, None);
            target.sdk_session_id = Some("sdk-original".into());
            store.insert_session(&target).unwrap();
        }
        let asking = Session::new("impl/S2-T2", SessionType::ReviewFix, None, None, None);
        store.insert_session(&asking).unwrap();

        (
            Harness {
                escalations,
                sessions,
                store,
                engine,
                bus,
            },
            asking,
        )
    }

    #[test]
    fn the_prompt_carries_the_question_and_forbids_acting() {
        let prompt = escalation_prompt("Why three retries?");
        assert!(prompt.contains("Why three retries?"));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("do not edit files"));
    }

    #[test]
    fn escalation_sessions_are_recognised() {
        assert!(is_escalation(SessionType::Escalation));
        assert!(!is_escalation(SessionType::Implementation));
    }

    #[test]
    fn disallowed_tools_cover_the_ways_to_change_a_worktree() {
        for tool in ["Edit", "Write", "Bash"] {
            assert!(DISALLOWED.contains(&tool));
        }
    }

    #[tokio::test]
    async fn a_branch_without_an_implementation_session_gets_a_readable_refusal() {
        let (h, asking) = harness(false);
        let answer = h.escalations.answer(&asking.id, "why three retries?").await;
        assert!(answer.text.contains("Context unavailable"));
        assert!(answer.text.contains("TASK_NOTES.md"));
        // Nothing was spawned: there was nothing to resume.
        assert!(h.engine.spawns.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_asking_session_is_refused_without_spawning() {
        let (h, _asking) = harness(true);
        let answer = h.escalations.answer("no-such-session", "why?").await;
        assert!(answer.text.contains("Context unavailable"));
        assert!(h.engine.spawns.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_budget_is_two_per_turn_and_resets_on_a_new_turn() {
        let (h, asking) = harness(true);
        assert!(h.escalations.claim_budget(&asking.id));
        assert!(h.escalations.claim_budget(&asking.id));
        assert!(
            !h.escalations.claim_budget(&asking.id),
            "the third call in one turn must be refused"
        );

        // The refusal is an answer, not an error, and it spawns nothing.
        let answer = h.escalations.answer(&asking.id, "third question").await;
        assert!(answer.text.contains("escalation budget"));
        assert!(h.engine.spawns.lock().unwrap().is_empty());

        // A new turn on the asking session refills it — the loop watches for `streaming`.
        h.escalations.spent.lock().unwrap().remove(&asking.id);
        assert!(h.escalations.claim_budget(&asking.id));
    }

    #[tokio::test]
    async fn the_budget_loop_refills_on_a_streaming_transition() {
        let (h, asking) = harness(true);
        assert!(h.escalations.claim_budget(&asking.id));
        assert!(h.escalations.claim_budget(&asking.id));

        let escalations = h.escalations.clone();
        let bus = h.bus.clone();
        let loop_handle = tokio::spawn(escalations.run_loop(bus.clone()));
        // Give the loop a moment to subscribe, then announce a new turn.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        bus.publish(Event::SessionStatusChanged {
            session_id: asking.id.clone(),
            branch: "impl/S2-T2".into(),
            status: SessionStatus::Streaming,
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            h.escalations.claim_budget(&asking.id),
            "a new turn must refill the budget"
        );
        loop_handle.abort();
    }

    #[tokio::test]
    async fn the_target_is_the_latest_implementation_session_with_a_context() {
        let (h, _asking) = harness(true);
        // A newer implementation session without an SDK id is not resumable.
        let mut newer = Session::new("impl/S2-T2", SessionType::Implementation, None, None, None);
        newer.sdk_session_id = None;
        h.store.insert_session(&newer).unwrap();
        // A research session is never the target, even with a context.
        let mut research = Session::new("impl/S2-T2", SessionType::Research, None, None, None);
        research.sdk_session_id = Some("sdk-research".into());
        h.store.insert_session(&research).unwrap();

        let target = h
            .escalations
            .resolve_target("impl/S2-T2")
            .expect("a target");
        let resolved = h.store.get_session(&target).unwrap().unwrap();
        assert_eq!(resolved.session_type, SessionType::Implementation);
        assert_eq!(resolved.sdk_session_id.as_deref(), Some("sdk-original"));
    }

    #[tokio::test]
    async fn an_escalation_session_is_read_only_and_keeps_the_writer_slot_free() {
        let (h, _asking) = harness(true);
        // Spawn an escalation session the way the manager does.
        let escalated = h
            .sessions
            .spawn(SpawnParams {
                branch: "impl/S2-T2".into(),
                cwd: ".".into(),
                session_type: SessionType::Escalation,
                model: Some(ESCALATION_MODEL.into()),
                effort: None,
                permission_mode: Some(crate::core::session::READ_ONLY_MODE.to_string()),
                thinking: None,
                tools_profile: None,
                disallowed_tools: DISALLOWED.iter().map(|t| t.to_string()).collect(),
                prompt: escalation_prompt("why?"),
                resume_from: None,
            })
            .unwrap();
        assert!(!escalated.is_writer());

        // The branch's write slot is still free afterwards.
        let writer = h
            .sessions
            .spawn(SpawnParams {
                branch: "impl/S2-T2".into(),
                cwd: ".".into(),
                session_type: SessionType::Implementation,
                model: None,
                effort: None,
                permission_mode: None,
                thinking: None,
                tools_profile: None,
                disallowed_tools: Vec::new(),
                prompt: "carry on".into(),
                resume_from: None,
            })
            .unwrap();
        assert!(writer.is_writer(), "an escalation must not take the slot");

        // And the engine was told to withhold the writing tools.
        let spawns = h.engine.spawns.lock().unwrap();
        let escalation_spawn = spawns
            .iter()
            .find(|s| s.session_id == escalated.id)
            .expect("the escalation spawn");
        assert_eq!(escalation_spawn.model.as_deref(), Some("sonnet"));
        for tool in DISALLOWED {
            assert!(escalation_spawn
                .disallowed_tools
                .contains(&tool.to_string()));
        }
    }
}
