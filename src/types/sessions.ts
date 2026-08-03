// Mirrors the Rust types in src-tauri/src/core/session/.

import type { SessionStatus, SessionType } from "./events";

export interface Session {
  id: string;
  branch: string;
  session_type: SessionType;
  status: SessionStatus;
  model: string | null;
  effort: string | null;
  permission_mode: string | null;
  sdk_session_id: string | null;
  created_at: string;
  updated_at: string;
}

export interface CommandInfo {
  name: string;
  description: string;
  argument_hint: string;
}

export interface ModelOption {
  id: string;
  display_name: string;
}

/** Fallback until a live session reports the CLI's real model list. */
export const FALLBACK_MODELS: ModelOption[] = [
  { id: "claude-fable-5", display_name: "Claude Fable 5" },
  { id: "claude-opus-5", display_name: "Claude Opus 5" },
  { id: "claude-sonnet-5", display_name: "Claude Sonnet 5" },
  { id: "claude-haiku-4-5", display_name: "Claude Haiku 4.5" },
];

export type TranscriptItem =
  | { kind: "user"; text: string }
  | { kind: "text"; text: string }
  | { kind: "tool_use"; name: string; summary: string }
  | {
      kind: "permission_request";
      requestId: string;
      tool: string;
      args: unknown;
      title: string | null;
      resolved: "pending" | "allowed" | "denied";
    }
  | { kind: "status"; status: SessionStatus };

export const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;
// `bypassPermissions` is deliberately absent: with it the SDK never calls canUseTool,
// so pushes/PRs/commits would run without ever reaching the gate. The plumbing still
// accepts it (config file / settings), it just isn't offered in the UI.
export const PERMISSION_MODES = ["default", "acceptEdits", "plan"] as const;
export const READ_ONLY_MODE = "plan";
export const ACTIVE_STATUSES: SessionStatus[] = ["spawning", "streaming", "awaiting_input"];

export function isTerminalStatus(status: SessionStatus): boolean {
  return status === "done" || status === "failed" || status === "cancelled";
}
