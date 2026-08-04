//! Rust ↔ sidecar protocol, version 1. NDJSON over stdio: one JSON object per line.
//! Keep in sync with sidecar/src/protocol.ts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 2;

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
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_id: Option<String>,
    },
    Send {
        id: u64,
        session_id: String,
        prompt: String,
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
    /// Answer a dialog the CLI asked the host to render.
    UserDialogResponse {
        id: u64,
        request_id: String,
        /// `completed` carries a dialog-specific result; `cancelled` applies the CLI default.
        behavior: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
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
    },
    ToolUse {
        session_id: String,
        name: String,
        summary: String,
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
                text: "hi".into()
            }
        );

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
