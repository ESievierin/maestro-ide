// Rust ↔ sidecar protocol. NDJSON over stdio: one JSON object per line.
// Keep in sync with src-tauri/src/core/agent/protocol.rs.

export const PROTOCOL_VERSION = 5;

// ---------- Requests (core → sidecar) ----------

export interface SpawnRequest {
  type: "spawn";
  id: number;
  session_id: string;
  cwd: string;
  prompt: string;
  session_type: string;
  model?: string;
  effort?: string;
  permission_mode?: string;
  /** See {@link SetThinkingRequest.thinking}. Absent leaves the CLI default. */
  thinking?: string;
  /**
   * Which extra tools this session gets. `"review"` registers `ask_original_agent`;
   * absent or unknown registers nothing. A string rather than a boolean so more profiles
   * can be added without another protocol change.
   */
  tools_profile?: string;
  /** Tool names the session may not use at all (SDK `disallowedTools`). */
  disallowed_tools?: string[];
  resume_id?: string;
}

/** An image the user pasted into the chat, carried as base64 next to the prompt. */
export interface Attachment {
  media_type: string;
  data: string;
}

export interface SendRequest {
  type: "send";
  id: number;
  session_id: string;
  prompt: string;
  attachments?: Attachment[];
}

export interface InterruptRequest {
  type: "interrupt";
  id: number;
  session_id: string;
}

export interface CloseRequest {
  type: "close";
  id: number;
  session_id: string;
}

export interface PermissionResponseRequest {
  type: "permission_response";
  id: number;
  request_id: string;
  allow: boolean;
  updated_args?: Record<string, unknown>;
  message?: string;
}

/** Change the model of a running session (Query.setModel). */
export interface SetModelRequest {
  type: "set_model";
  id: number;
  session_id: string;
  /** Empty string clears the override and returns to the default. */
  model: string;
}

/** Change the effort of a running session (Query.applyFlagSettings). */
export interface SetEffortRequest {
  type: "set_effort";
  id: number;
  session_id: string;
  /** Empty string clears the override. */
  effort: string;
}

/**
 * Change how much the model may think mid-session (Query.setMaxThinkingTokens).
 *
 * `thinking` is `""`/`"default"` for the CLI's own behaviour (adaptive on models that
 * support it), `"off"` to disable it, or a token budget as a decimal string (`"4000"`).
 * The budget matters in practice: with the default, the models tested here often produce
 * no thinking at all, so nothing can be shown.
 */
export interface SetThinkingRequest {
  type: "set_thinking";
  id: number;
  session_id: string;
  thinking: string;
}

/** Change the permission mode of a running session (Query.setPermissionMode). */
export interface SetPermissionModeRequest {
  type: "set_permission_mode";
  id: number;
  session_id: string;
  permission_mode: string;
}

/**
 * A dialog answer in Maestro's own terms. The engine translates it into the
 * CLI-specific result shape of the dialog it belongs to, so neither the core nor the
 * UI has to know that (say) `permission_ask_user_question` is answered with a
 * permission decision carrying an updated tool input.
 */
export interface DialogAnswer {
  /** Question text → chosen option label. Multi-select answers are comma-separated. */
  answers?: Record<string, string>;
  /** Per-question extras the CLI hands back to the model (option preview, user notes). */
  annotations?: Record<string, { preview?: string; notes?: string }>;
  /**
   * Free text instead of picking options. The agent is told the user wants to clarify
   * the questions rather than answer them as asked.
   */
  feedback?: string;
  /**
   * Yes/no answer for approval dialogs (the plan review). `false` with `feedback` is
   * "not yet, here is what to change".
   */
  approved?: boolean;
}

/** The host's answer to a `user_dialog_request`. */
export interface UserDialogResponseRequest {
  type: "user_dialog_response";
  id: number;
  request_id: string;
  /** `completed` carries the answer; `cancelled` means the user dismissed the dialog. */
  behavior: "completed" | "cancelled";
  result?: DialogAnswer;
}

/**
 * The core's verdict on a tool call, before it runs.
 *
 * `pass` means "not my business" — the CLI carries on with whatever its permission mode
 * would have done. `allow` and `deny` are final and apply in **every** permission mode,
 * which is the whole reason gating moved to a hook: a classifier-approved call in `auto`
 * never reaches `canUseTool`, but it does reach `PreToolUse`.
 */
export interface GateDecisionRequest {
  type: "gate_decision";
  id: number;
  request_id: string;
  decision: "pass" | "allow" | "deny";
  /** Substituted into the tool call on `allow` (the gate's editable params). */
  updated_args?: Record<string, unknown>;
  /** Shown to the agent on `deny`. */
  message?: string;
}

/**
 * The core's answer to an `escalation_request`. Always a result the asking agent can read —
 * including the failures, which arrive as plain text rather than as a thrown tool error.
 */
export interface EscalationResponseRequest {
  type: "escalation_response";
  id: number;
  request_id: string;
  result: string;
}

/** Ask the CLI which models it offers. No session, no turn, no tokens. */
export interface ListModelsRequest {
  type: "list_models";
  id: number;
  cwd: string;
}

/** Reconnect or enable/disable an MCP server of a running session. */
export interface McpActionRequest {
  type: "mcp_action";
  id: number;
  session_id: string;
  server: string;
  /** `reconnect`, `enable` or `disable`. */
  action: string;
}

export interface ShutdownRequest {
  type: "shutdown";
  id: number;
}

export type SidecarRequest =
  | ListModelsRequest
  | SetModelRequest
  | SetEffortRequest
  | SetThinkingRequest
  | SetPermissionModeRequest
  | UserDialogResponseRequest
  | EscalationResponseRequest
  | GateDecisionRequest
  | McpActionRequest
  | SpawnRequest
  | SendRequest
  | InterruptRequest
  | CloseRequest
  | PermissionResponseRequest
  | ShutdownRequest;

// ---------- Events (sidecar → core) ----------

export type SessionRuntimeStatus = "streaming" | "awaiting_input";

export interface CommandInfo {
  /** Command name without the leading slash. */
  name: string;
  description: string;
  argument_hint: string;
}

export interface ModelOption {
  /** Model identifier to use when spawning sessions. */
  id: string;
  display_name: string;
}

/** A subagent profile the session can delegate to (`Task` with this `subagent_type`). */
export interface AgentInfo {
  name: string;
  description: string;
  model: string;
}

/** Connection state of one MCP server, as the CLI reports it. */
export interface McpServerInfo {
  name: string;
  status: string;
  tool_count: number;
  detail: string;
}

/** One entry of the agent's todo list (TodoWrite). */
export interface TodoItem {
  content: string;
  status: string;
}

export type SidecarEvent =
  | { type: "ready"; protocol_version: number }
  | { type: "ack"; id: number; ok: boolean; error?: string }
  | { type: "session_init"; session_id: string; sdk_session_id: string; model?: string }
  | { type: "commands"; session_id: string; commands: CommandInfo[] }
  /** `session_id` is empty for the global list from `list_models`. */
  | { type: "models"; session_id: string; models: ModelOption[] }
  /** `parent_tool_use_id` is set for subagent output, so the UI can nest it. */
  | {
      type: "stream_delta";
      session_id: string;
      text: string;
      parent_tool_use_id?: string;
    }
  /** The agent's reasoning, kept separate from its answer. */
  | {
      type: "thinking_delta";
      session_id: string;
      text: string;
      parent_tool_use_id?: string;
    }
  | {
      type: "tool_use";
      session_id: string;
      /** Matches the `tool_result` that follows, and the subagent output nested under it. */
      tool_use_id: string;
      name: string;
      summary: string;
      parent_tool_use_id?: string;
    }
  /** What a tool actually returned; without it a session looks like it did nothing. */
  | {
      type: "tool_result";
      session_id: string;
      tool_use_id: string;
      is_error: boolean;
      text: string;
    }
  /**
   * A tool is about to run. The core answers with a `gate_decision`; until then the call
   * is paused. Unlike a permission request this fires in every permission mode.
   */
  | {
      type: "gate_check";
      session_id: string;
      request_id: string;
      tool: string;
      args: Record<string, unknown>;
    }
  /**
   * The session called `ask_original_agent`: it wants the reasoning of the agent that
   * implemented this branch. The core answers with `escalation_response`.
   */
  | {
      type: "escalation_request";
      session_id: string;
      request_id: string;
      question: string;
    }
  /** Subagent profiles this session can delegate to. */
  | { type: "agents"; session_id: string; agents: AgentInfo[] }
  /** MCP servers of this session and their connection state. */
  | { type: "mcp_servers"; session_id: string; servers: McpServerInfo[] }
  /** The agent's plan/checklist, replaced wholesale on every TodoWrite. */
  | { type: "todos"; session_id: string; items: TodoItem[] }
  /** Cost and context pressure after a turn. */
  | {
      type: "usage";
      session_id: string;
      total_cost_usd?: number;
      num_turns?: number;
      input_tokens?: number;
      output_tokens?: number;
      context_tokens?: number;
      context_max_tokens?: number;
      context_percent?: number;
    }
  /** Subscription rate-limit state; a warning here precedes a wall. */
  | {
      type: "rate_limit";
      session_id: string;
      status: string;
      limit_type?: string;
      utilization?: number;
      resets_at?: string;
    }
  /**
   * A tool call denied without ever reaching `canUseTool` (auto mode's classifier, a deny
   * rule, `dontAsk`). Invisible otherwise — the agent just seems to skip work.
   */
  | {
      type: "permission_denied";
      session_id: string;
      tool: string;
      reason: string;
      message: string;
    }
  | {
      type: "permission_request";
      session_id: string;
      request_id: string;
      tool: string;
      args: Record<string, unknown>;
      title?: string;
    }
  /**
   * The CLI wants a blocking dialog rendered (AskUserQuestion and friends). `dialog_kind`
   * is an open union: a host that does not recognise one must answer `cancelled`, or the
   * agent's turn hangs waiting for an answer that never comes.
   */
  | {
      type: "user_dialog_request";
      session_id: string;
      request_id: string;
      dialog_kind: string;
      payload: Record<string, unknown>;
      tool_use_id?: string;
    }
  | { type: "status"; session_id: string; status: SessionRuntimeStatus }
  | {
      type: "result";
      session_id: string;
      subtype: string;
      is_error: boolean;
      duration_ms?: number;
      total_cost_usd?: number;
      num_turns?: number;
    }
  | { type: "session_closed"; session_id: string; reason: "ended" | "closed" | "error" }
  | { type: "error"; session_id?: string; message: string };

export function writeEvent(event: SidecarEvent): void {
  process.stdout.write(JSON.stringify(event) + "\n");
}

/** Common surface of a session runner (real SDK-backed or mock). */
export interface SessionHandle {
  spawn(req: SpawnRequest): Promise<void>;
  send(prompt: string, attachments?: Attachment[]): void;
  interrupt(): Promise<void>;
  close(): void;
  /** Returns false when the request id is unknown to this session. */
  respondPermission(
    requestId: string,
    allow: boolean,
    updatedArgs?: Record<string, unknown>,
    message?: string,
  ): boolean;
  /** Returns false when the dialog id is unknown to this session. */
  respondUserDialog(
    requestId: string,
    behavior: "completed" | "cancelled",
    result?: DialogAnswer,
  ): boolean;
  setModel(model: string): Promise<void>;
  setEffort(effort: string): Promise<void>;
  setThinking(thinking: string): Promise<void>;
  /** `reconnect` | `enable` | `disable` on one MCP server. */
  mcpAction(server: string, action: string): Promise<void>;
  /** Returns false when the escalation id is unknown to this session. */
  respondEscalation(requestId: string, result: string): boolean;
  /** Returns false when the gate check id is unknown to this session. */
  respondGate(
    requestId: string,
    decision: string,
    updatedArgs?: Record<string, unknown>,
    message?: string,
  ): boolean;
  setPermissionMode(mode: string): Promise<void>;
}
