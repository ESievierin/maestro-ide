// Rust ↔ sidecar protocol, version 1. NDJSON over stdio: one JSON object per line.
// Keep in sync with src-tauri/src/core/agent/protocol.rs.

export const PROTOCOL_VERSION = 1;

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

export interface ShutdownRequest {
  type: "shutdown";
  id: number;
}

export type SidecarRequest =
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
}
