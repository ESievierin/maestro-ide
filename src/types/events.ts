// Mirrors the Rust `Event` enum in src-tauri/src/core/bus/mod.rs.
// Keep the two in sync when adding events.

export type Severity = "info" | "warning" | "error" | "critical";

export type SessionStatus =
  "spawning" | "streaming" | "awaiting_input" | "done" | "failed" | "cancelled";

export type SessionType = "research" | "implementation" | "review_fix" | "manual";

export type BusEvent =
  | { type: "worktree.created"; data: { branch: string; path: string } }
  | { type: "worktree.removed"; data: { branch: string } }
  | {
      type: "session.status_changed";
      data: { session_id: string; branch: string; status: SessionStatus };
    }
  | { type: "session.stream_delta"; data: { session_id: string; text: string } }
  | { type: "session.tool_use"; data: { session_id: string; name: string; summary: string } }
  | {
      type: "session.commands";
      data: {
        session_id: string;
        commands: { name: string; description: string; argument_hint: string }[];
      };
    }
  | {
      type: "session.models";
      data: { session_id: string; models: { id: string; display_name: string }[] };
    }
  | {
      type: "session.permission_request";
      data: {
        session_id: string;
        request_id: string;
        tool: string;
        args: unknown;
        title: string | null;
      };
    }
  | { type: "diff.updated"; data: { branch: string } }
  | {
      type: "gate.pending";
      data: {
        gate_id: string;
        session_id: string;
        tool: string;
        kind: string;
        branch: string;
        params: { key: string; label: string; value: string; multiline: boolean }[];
        raw_args: unknown;
      };
    }
  | {
      type: "attention.required";
      data: {
        source: string;
        branch: string | null;
        session_id: string | null;
        message: string;
      };
    }
  | { type: "error.raised"; data: { severity: Severity; code: string; message: string } }
  | { type: "system.test"; data: { message: string } };

export interface Branch {
  name: string;
  task_id: string | null;
  created_at: string;
}
