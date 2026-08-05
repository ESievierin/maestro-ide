//! Rust ↔ sidecar protocol, version 1. NDJSON over stdio: one JSON object per line.
//! Keep in sync with sidecar/src/protocol.ts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 5;

/// Requests sent to the sidecar. Every request carries an `id`; the sidecar
/// answers with an `ack` event carrying the same id.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarRequest {
    Spawn {
        id: u64,
        session_id: String,
        cwd: String,
        prompt: String,
        session_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        /// `""`/absent leaves the CLI default; `off` disables thinking; a decimal string
        /// is a token budget.
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        /// Extra tools this session gets (`review` → `ask_original_agent`).
        #[serde(skip_serializing_if = "Option::is_none")]
        tools_profile: Option<String>,
        /// Tools the session may not use at all.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        disallowed_tools: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_id: Option<String>,
    },
    Send {
        id: u64,
        session_id: String,
        prompt: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    Interrupt {
        id: u64,
        session_id: String,
    },
    Close {
        id: u64,
        session_id: String,
    },
    PermissionResponse {
        id: u64,
        request_id: String,
        allow: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_args: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    SetModel {
        id: u64,
        session_id: String,
        /// Empty string clears the override.
        model: String,
    },
    SetEffort {
        id: u64,
        session_id: String,
        /// Empty string clears the override.
        effort: String,
    },
    SetPermissionMode {
        id: u64,
        session_id: String,
        permission_mode: String,
    },
    /// Change how much the model may think mid-session.
    SetThinking {
        id: u64,
        session_id: String,
        thinking: String,
    },
    /// Answer a dialog the CLI asked the host to render.
    UserDialogResponse {
        id: u64,
        request_id: String,
        /// `completed` carries a dialog-specific result; `cancelled` applies the CLI default.
        behavior: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    /// The verdict on a paused tool call. `pass` hands it back to the CLI's own permission
    /// handling; `allow`/`deny` are final in every permission mode.
    GateDecision {
        id: u64,
        request_id: String,
        decision: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_args: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Answer an `ask_original_agent` call. Always a readable result, including failures.
    EscalationResponse {
        id: u64,
        request_id: String,
        result: String,
    },
    /// Reconnect or enable/disable one MCP server of a running session.
    McpAction {
        id: u64,
        session_id: String,
        server: String,
        action: String,
    },
    /// Ask the CLI for its model list without starting a session (no tokens spent).
    ListModels {
        id: u64,
        cwd: String,
    },
    Shutdown {
        id: u64,
    },
}

/// An image pasted into the chat, carried to the model as a content block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Attachment {
    /// MIME type, e.g. `image/png`.
    pub media_type: String,
    /// Base64 payload without a data-URI prefix.
    pub data: String,
}

/// A subagent profile the session can delegate to.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AgentInfo {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub model: String,
}

/// Connection state of one MCP server.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct McpServerInfo {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub tool_count: u32,
    #[serde(default)]
    pub detail: String,
}

/// One entry of the agent's todo list.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

/// A slash command supported by a running session (for input autocomplete).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CommandInfo {
    /// Command name without the leading slash.
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub argument_hint: String,
}

/// A model option reported by the running CLI (for the new-session selector).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ModelOption {
    pub id: String,
    pub display_name: String,
}

/// Events received from the sidecar.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarEvent {
    Ready {
        protocol_version: u32,
    },
    Ack {
        id: u64,
        ok: bool,
        #[serde(default)]
        error: Option<String>,
    },
    SessionInit {
        session_id: String,
        sdk_session_id: String,
        #[serde(default)]
        model: Option<String>,
    },
    Commands {
        session_id: String,
        commands: Vec<CommandInfo>,
    },
    Models {
        session_id: String,
        models: Vec<ModelOption>,
    },
    StreamDelta {
        session_id: String,
        text: String,
        /// Set when the text came from a subagent, so the UI can nest it.
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    /// The agent's reasoning, kept apart from its answer.
    ThinkingDelta {
        session_id: String,
        text: String,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    ToolUse {
        session_id: String,
        tool_use_id: String,
        name: String,
        summary: String,
        #[serde(default)]
        parent_tool_use_id: Option<String>,
    },
    /// What the tool returned, matched to its call by `tool_use_id`.
    ToolResult {
        session_id: String,
        tool_use_id: String,
        is_error: bool,
        text: String,
    },
    /// A tool is about to run and is paused until the core answers with a `GateDecision`.
    /// Fires in every permission mode, which is why the commit/push gate lives here.
    GateCheck {
        session_id: String,
        request_id: String,
        tool: String,
        args: Value,
    },
    /// The session asked the agent that implemented this branch about its reasoning.
    EscalationRequest {
        session_id: String,
        request_id: String,
        question: String,
    },
    /// Subagent profiles this session can delegate to.
    Agents {
        session_id: String,
        agents: Vec<AgentInfo>,
    },
    /// MCP servers of this session and their connection state.
    McpServers {
        session_id: String,
        servers: Vec<McpServerInfo>,
    },
    /// The agent's checklist, replaced wholesale on every TodoWrite.
    Todos {
        session_id: String,
        items: Vec<TodoItem>,
    },
    /// Cost and context pressure. Arrives in two flavours: turn totals after a result,
    /// and a context-window reading right after it.
    Usage {
        session_id: String,
        #[serde(default)]
        total_cost_usd: Option<f64>,
        #[serde(default)]
        num_turns: Option<u32>,
        #[serde(default)]
        input_tokens: Option<u64>,
        #[serde(default)]
        output_tokens: Option<u64>,
        #[serde(default)]
        context_tokens: Option<u64>,
        #[serde(default)]
        context_max_tokens: Option<u64>,
        #[serde(default)]
        context_percent: Option<f64>,
    },
    /// Subscription rate-limit state; the warning precedes the wall.
    RateLimit {
        session_id: String,
        status: String,
        #[serde(default)]
        limit_type: Option<String>,
        #[serde(default)]
        utilization: Option<f64>,
        #[serde(default)]
        resets_at: Option<String>,
    },
    /// A tool call denied without reaching `canUseTool` (auto-mode classifier, deny rule,
    /// `dontAsk`). Invisible otherwise: the agent just appears to skip work.
    PermissionDenied {
        session_id: String,
        tool: String,
        reason: String,
        message: String,
    },
    PermissionRequest {
        session_id: String,
        request_id: String,
        tool: String,
        args: Value,
        #[serde(default)]
        title: Option<String>,
    },
    /// The CLI wants a blocking dialog rendered (AskUserQuestion and friends). The kind is
    /// an open union: unknown kinds must be answered `cancelled` or the agent's turn parks.
    UserDialogRequest {
        session_id: String,
        request_id: String,
        dialog_kind: String,
        payload: Value,
        #[serde(default)]
        tool_use_id: Option<String>,
    },
    Status {
        session_id: String,
        status: String,
    },
    Result {
        session_id: String,
        subtype: String,
        is_error: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        total_cost_usd: Option<f64>,
        #[serde(default)]
        num_turns: Option<u32>,
    },
    SessionClosed {
        session_id: String,
        reason: String,
    },
    Error {
        #[serde(default)]
        session_id: Option<String>,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialize_with_snake_case_tags() {
        let req = SidecarRequest::Spawn {
            id: 1,
            session_id: "s1".into(),
            cwd: "C:/work".into(),
            prompt: "hello".into(),
            session_type: "manual".into(),
            model: None,
            effort: Some("high".into()),
            permission_mode: None,
            thinking: None,
            tools_profile: None,
            disallowed_tools: Vec::new(),
            resume_id: None,
        };
        let json = serde_json::to_value(&req).expect("serialize");
        assert_eq!(json["type"], "spawn");
        assert_eq!(json["effort"], "high");
        assert!(json.get("model").is_none(), "None fields are omitted");
    }

    #[test]
    fn events_deserialize() {
        let event: SidecarEvent =
            serde_json::from_str(r#"{"type":"stream_delta","session_id":"s1","text":"hi"}"#)
                .expect("parse");
        assert_eq!(
            event,
            SidecarEvent::StreamDelta {
                session_id: "s1".into(),
                text: "hi".into(),
                parent_tool_use_id: None,
            }
        );

        // Subagent output carries the parent tool call it belongs to.
        let event: SidecarEvent = serde_json::from_str(
            r#"{"type":"tool_use","session_id":"s1","tool_use_id":"t1","name":"Grep","summary":"{}","parent_tool_use_id":"task-1"}"#,
        )
        .expect("parse nested tool use");
        match event {
            SidecarEvent::ToolUse {
                tool_use_id,
                parent_tool_use_id,
                ..
            } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(parent_tool_use_id.as_deref(), Some("task-1"));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Usage arrives twice per turn (turn totals, then a context reading); every
        // field is optional so a partial reading still parses.
        let event: SidecarEvent =
            serde_json::from_str(r#"{"type":"usage","session_id":"s1","context_percent":12.5}"#)
                .expect("parse partial usage");
        match event {
            SidecarEvent::Usage {
                context_percent,
                total_cost_usd,
                ..
            } => {
                assert_eq!(context_percent, Some(12.5));
                assert_eq!(total_cost_usd, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let event: SidecarEvent = serde_json::from_str(
            r#"{"type":"result","session_id":"s1","subtype":"success","is_error":false}"#,
        )
        .expect("parse result without optional fields");
        match event {
            SidecarEvent::Result { subtype, .. } => assert_eq!(subtype, "success"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
