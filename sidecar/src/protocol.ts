// Rust ↔ sidecar protocol, version 1. NDJSON over stdio: one JSON object per line.
// Keep in sync with src-tauri/src/core/agent/protocol.rs.

export const PROTOCOL_VERSION = 2;

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
  resume_id?: string;
}

export interface SendRequest {
  type: "send";
  id: number;
  session_id: string;
  prompt: string;
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

/** Ask the CLI which models it offers. No session, no turn, no tokens. */
export interface ListModelsRequest {
  type: "list_models";
  id: number;
  cwd: string;
}

export interface ShutdownRequest {
  type: "shutdown";
  id: number;
}

export type SidecarRequest =
  | ListModelsRequest
  | SetModelRequest
  | SetEffortRequest
  | SetPermissionModeRequest
  | UserDialogResponseRequest
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

export type SidecarEvent =
  | { type: "ready"; protocol_version: number }
  | { type: "ack"; id: number; ok: boolean; error?: string }
  | { type: "session_init"; session_id: string; sdk_session_id: string; model?: string }
  | { type: "commands"; session_id: string; commands: CommandInfo[] }
  /** `session_id` is empty for the global list from `list_models`. */
  | { type: "models"; session_id: string; models: ModelOption[] }
  | { type: "stream_delta"; session_id: string; text: string }
  | { type: "tool_use"; session_id: string; name: string; summary: string }
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
  send(prompt: string): void;
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
  setPermissionMode(mode: string): Promise<void>;
}
