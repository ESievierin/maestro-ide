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
  | {
      type: "session.stream_delta";
      data: { session_id: string; text: string; parent_tool_use_id: string | null };
    }
  | {
      type: "session.thinking_delta";
      data: { session_id: string; text: string; parent_tool_use_id: string | null };
    }
  | {
      type: "session.tool_use";
      data: {
        session_id: string;
        tool_use_id: string;
        name: string;
        summary: string;
        parent_tool_use_id: string | null;
      };
    }
  | {
      type: "session.tool_result";
      data: { session_id: string; tool_use_id: string; is_error: boolean; text: string };
    }
  | {
      type: "session.agents";
      data: {
        session_id: string;
        agents: { name: string; description: string; model: string }[];
      };
    }
  | {
      type: "session.mcp_servers";
      data: {
        session_id: string;
        servers: { name: string; status: string; tool_count: number; detail: string }[];
      };
    }
  | {
      type: "session.todos";
      data: { session_id: string; items: { content: string; status: string }[] };
    }
  | {
      type: "session.usage";
      data: {
        session_id: string;
        total_cost_usd: number | null;
        num_turns: number | null;
        input_tokens: number | null;
        output_tokens: number | null;
        context_tokens: number | null;
        context_max_tokens: number | null;
        context_percent: number | null;
      };
    }
  | {
      type: "session.rate_limit";
      data: {
        session_id: string;
        status: string;
        limit_type: string | null;
        utilization: number | null;
        resets_at: string | null;
      };
    }
  | {
      type: "session.permission_denied";
      data: { session_id: string; tool: string; reason: string; message: string };
    }
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
  | {
      type: "session.user_dialog";
      data: {
        session_id: string;
        request_id: string;
        dialog_kind: string;
        payload: unknown;
      };
    }
  | {
      type: "session.user_dialog_resolved";
      data: { session_id: string; request_id: string };
    }
  | {
      type: "session.settings_changed";
      data: {
        session_id: string;
        model: string | null;
        effort: string | null;
        permission_mode: string | null;
        thinking: string | null;
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
        note: string | null;
        raw_args: unknown;
      };
    }
  | { type: "question.answering"; data: { question_id: string; session_id: string } }
  | {
      type: "question.answered";
      data: { question_id: string; session_id: string; ok: boolean };
    }
  | { type: "gate.resolved"; data: { gate_id: string; reason: string } }
  | { type: "attention.updated"; data: { count: number } }
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
