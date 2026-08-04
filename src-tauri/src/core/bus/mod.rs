//! Central event bus.
//!
//! Every state change in the core is published here as a typed [`Event`]. UI panels,
//! notifications, and the future background daemon are subscribers — core modules never
//! call UI update logic directly.

use serde::Serialize;
use tokio::sync::broadcast;

use crate::core::session::SessionStatus;
use crate::error::Severity;

/// Buffered events per subscriber before a slow subscriber starts lagging.
const CHANNEL_CAPACITY: usize = 256;

/// All events that can flow through the system. Serialized with `type` + `data`, so the
/// frontend receives e.g. `{ "type": "worktree.created", "data": { ... } }`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    #[serde(rename = "worktree.created")]
    WorktreeCreated { branch: String, path: String },

    #[serde(rename = "worktree.removed")]
    WorktreeRemoved { branch: String },

    #[serde(rename = "session.status_changed")]
    SessionStatusChanged {
        session_id: String,
        branch: String,
        status: SessionStatus,
    },

    #[serde(rename = "session.stream_delta")]
    SessionStreamDelta {
        session_id: String,
        text: String,
        /// Set when a subagent produced the text.
        parent_tool_use_id: Option<String>,
    },

    #[serde(rename = "session.tool_use")]
    SessionToolUse {
        session_id: String,
        tool_use_id: String,
        name: String,
        summary: String,
        /// Set when a subagent made the call, so the UI nests it under the Task entry.
        parent_tool_use_id: Option<String>,
    },

    #[serde(rename = "session.permission_request")]
    SessionPermissionRequest {
        session_id: String,
        request_id: String,
        tool: String,
        args: serde_json::Value,
        title: Option<String>,
    },

    /// The agent asked a blocking question (AskUserQuestion and friends). The UI renders
    /// `payload` for the known kinds and cancels the rest.
    #[serde(rename = "session.user_dialog")]
    SessionUserDialog {
        session_id: String,
        request_id: String,
        dialog_kind: String,
        payload: serde_json::Value,
    },

    /// A slice of the agent's reasoning. Separate from `session.stream_delta` so the UI
    /// can fold it away; `parent_tool_use_id` marks subagent thinking.
    #[serde(rename = "session.thinking_delta")]
    SessionThinkingDelta {
        session_id: String,
        text: String,
        parent_tool_use_id: Option<String>,
    },

    /// What a tool returned, matched to its call by `tool_use_id`.
    #[serde(rename = "session.tool_result")]
    SessionToolResult {
        session_id: String,
        tool_use_id: String,
        is_error: bool,
        text: String,
    },

    /// Subagent profiles this session can delegate to.
    #[serde(rename = "session.agents")]
    SessionAgents {
        session_id: String,
        agents: Vec<crate::core::agent::protocol::AgentInfo>,
    },

    /// MCP servers of this session and their connection state.
    #[serde(rename = "session.mcp_servers")]
    SessionMcpServers {
        session_id: String,
        servers: Vec<crate::core::agent::protocol::McpServerInfo>,
    },

    /// The agent's checklist as of the latest TodoWrite (replaces the previous one).
    #[serde(rename = "session.todos")]
    SessionTodos {
        session_id: String,
        items: Vec<crate::core::agent::protocol::TodoItem>,
    },

    /// Cost and context pressure for a session.
    #[serde(rename = "session.usage")]
    SessionUsage {
        session_id: String,
        total_cost_usd: Option<f64>,
        num_turns: Option<u32>,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        context_tokens: Option<u64>,
        context_max_tokens: Option<u64>,
        context_percent: Option<f64>,
    },

    /// Subscription rate-limit state changed (a warning arrives before the wall).
    #[serde(rename = "session.rate_limit")]
    SessionRateLimit {
        session_id: String,
        status: String,
        limit_type: Option<String>,
        utilization: Option<f64>,
        resets_at: Option<String>,
    },

    /// A tool call denied without asking the user (classifier, deny rule, `dontAsk`).
    #[serde(rename = "session.permission_denied")]
    SessionPermissionDenied {
        session_id: String,
        tool: String,
        reason: String,
        message: String,
    },

    /// The dialog was answered or dismissed; whatever was waiting on it can stand down.
    #[serde(rename = "session.user_dialog_resolved")]
    SessionUserDialogResolved {
        session_id: String,
        request_id: String,
    },

    /// A runtime knob of a live session changed (model / effort / permission mode).
    #[serde(rename = "session.settings_changed")]
    SessionSettingsChanged {
        session_id: String,
        model: Option<String>,
        effort: Option<String>,
        permission_mode: Option<String>,
        thinking: Option<String>,
    },

    #[serde(rename = "session.commands")]
    SessionCommands {
        session_id: String,
        commands: Vec<crate::core::agent::protocol::CommandInfo>,
    },

    #[serde(rename = "session.models")]
    SessionModels {
        session_id: String,
        models: Vec<crate::core::agent::protocol::ModelOption>,
    },

    #[serde(rename = "diff.updated")]
    DiffUpdated { branch: String },

    #[serde(rename = "gate.pending")]
    GatePending {
        gate_id: String,
        session_id: String,
        tool: String,
        kind: String,
        branch: String,
        params: Vec<crate::core::gate::GateParam>,
        /// Explanation shown when the gate has no editable params.
        note: Option<String>,
        raw_args: serde_json::Value,
    },

    /// The target session started the turn that answers `question_id` — the UI
    /// routes `session.stream_delta` for that session into this question until
    /// `question.answered` arrives.
    #[serde(rename = "question.answering")]
    QuestionAnswering {
        question_id: String,
        session_id: String,
    },

    #[serde(rename = "question.answered")]
    QuestionAnswered {
        question_id: String,
        session_id: String,
        ok: bool,
    },

    /// A gate left the pending set: answered by the user, cancelled, or its
    /// session died. The UI drops it from the queue on this event.
    #[serde(rename = "gate.resolved")]
    GateResolved { gate_id: String, reason: String },

    #[serde(rename = "attention.required")]
    AttentionRequired {
        source: String,
        branch: Option<String>,
        session_id: Option<String>,
        message: String,
    },

    /// The attention queue changed; panels refetch it. `count` lets a badge update
    /// without a round trip.
    #[serde(rename = "attention.updated")]
    AttentionUpdated { count: usize },

    #[serde(rename = "error.raised")]
    ErrorRaised {
        severity: Severity,
        code: String,
        message: String,
    },

    /// Diagnostic event used to verify the core → frontend pipeline end to end.
    #[serde(rename = "system.test")]
    Test { message: String },
}

impl Event {
    /// Event name as it appears on the wire; used for structured logging.
    pub fn name(&self) -> &'static str {
        match self {
            Event::WorktreeCreated { .. } => "worktree.created",
            Event::WorktreeRemoved { .. } => "worktree.removed",
            Event::SessionStatusChanged { .. } => "session.status_changed",
            Event::SessionStreamDelta { .. } => "session.stream_delta",
            Event::SessionToolUse { .. } => "session.tool_use",
            Event::SessionPermissionRequest { .. } => "session.permission_request",
            Event::SessionUserDialog { .. } => "session.user_dialog",
            Event::SessionThinkingDelta { .. } => "session.thinking_delta",
            Event::SessionToolResult { .. } => "session.tool_result",
            Event::SessionAgents { .. } => "session.agents",
            Event::SessionMcpServers { .. } => "session.mcp_servers",
            Event::SessionTodos { .. } => "session.todos",
            Event::SessionUsage { .. } => "session.usage",
            Event::SessionRateLimit { .. } => "session.rate_limit",
            Event::SessionPermissionDenied { .. } => "session.permission_denied",
            Event::SessionUserDialogResolved { .. } => "session.user_dialog_resolved",
            Event::SessionSettingsChanged { .. } => "session.settings_changed",
            Event::SessionCommands { .. } => "session.commands",
            Event::SessionModels { .. } => "session.models",
            Event::DiffUpdated { .. } => "diff.updated",
            Event::GatePending { .. } => "gate.pending",
            Event::QuestionAnswering { .. } => "question.answering",
            Event::QuestionAnswered { .. } => "question.answered",
            Event::GateResolved { .. } => "gate.resolved",
            Event::AttentionRequired { .. } => "attention.required",
            Event::AttentionUpdated { .. } => "attention.updated",
            Event::ErrorRaised { .. } => "error.raised",
            Event::Test { .. } => "system.test",
        }
    }
}

/// Cheap-to-clone handle to the bus. Publishing never blocks; subscribers that fall
/// behind by more than the channel capacity observe a `Lagged` error and can resync.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    /// Publish an event to all current subscribers. An event with no subscribers is
    /// dropped silently — that is normal during startup.
    pub fn publish(&self, event: Event) {
        tracing::debug!(event = event.name(), "bus.publish");
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_reaches_subscriber() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(Event::Test {
            message: "hello".into(),
        });
        let received = rx.recv().await.expect("event");
        assert_eq!(received.name(), "system.test");
    }

    #[tokio::test]
    async fn publish_reaches_all_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.publish(Event::DiffUpdated {
            branch: "feature/x".into(),
        });
        assert_eq!(rx1.recv().await.expect("rx1").name(), "diff.updated");
        assert_eq!(rx2.recv().await.expect("rx2").name(), "diff.updated");
    }

    #[test]
    fn events_serialize_with_dotted_names() {
        let event = Event::WorktreeCreated {
            branch: "impl/T-1-demo".into(),
            path: "C:/work/maestro-impl-T-1-demo".into(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], "worktree.created");
        assert_eq!(json["data"]["branch"], "impl/T-1-demo");
    }
}
