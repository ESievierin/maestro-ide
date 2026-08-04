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
  thinking: string | null;
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

/** The one dialog kind Maestro renders today (see the sidecar's `ASK_USER_QUESTION`). */
export const DIALOG_ASK_USER_QUESTION = "ask_user_question";

export interface QuestionOption {
  label: string;
  description: string;
  preview?: string;
}

export interface DialogQuestion {
  question: string;
  header: string;
  options: QuestionOption[];
  multiSelect: boolean;
}

/** Payload of an `ask_user_question` dialog: the `AskUserQuestion` tool's own input. */
export interface AskUserQuestionPayload {
  questions: DialogQuestion[];
}

/** A dialog waiting on the user, as tracked in the frontend store. */
export interface UserDialog {
  sessionId: string;
  requestId: string;
  dialogKind: string;
  payload: unknown;
}

/** Maestro's answer to a dialog; the sidecar maps it to the CLI's result shape. */
export interface DialogAnswer {
  answers?: Record<string, string>;
  annotations?: Record<string, { preview?: string; notes?: string }>;
  feedback?: string;
}

/** Narrow an unknown dialog payload to the ask-user-question shape, or null. */
export function asQuestionPayload(payload: unknown): AskUserQuestionPayload | null {
  if (typeof payload !== "object" || payload === null) return null;
  const questions = (payload as { questions?: unknown }).questions;
  if (!Array.isArray(questions) || questions.length === 0) return null;
  const valid = questions.every(
    (q) =>
      typeof q === "object" &&
      q !== null &&
      typeof (q as DialogQuestion).question === "string" &&
      Array.isArray((q as DialogQuestion).options),
  );
  return valid ? { questions: questions as DialogQuestion[] } : null;
}

/** One entry of the agent's checklist (CLI tasks, or TodoWrite on older CLIs). */
export interface TodoItem {
  content: string;
  status: string;
}

/** Cost and context pressure of a session, as reported after each turn. */
export interface SessionUsage {
  costUsd?: number;
  turns?: number;
  inputTokens?: number;
  outputTokens?: number;
  contextTokens?: number;
  contextMaxTokens?: number;
  contextPercent?: number;
}

/** Account-wide rate-limit state; a warning arrives before the wall. */
export interface RateLimitInfo {
  status: string;
  limitType?: string;
  utilization?: number;
  resetsAt?: string;
}

/** Subagent activity, nested under the tool call that spawned it. */
export type ToolChild =
  | { kind: "text"; text: string }
  | { kind: "thinking"; text: string }
  | { kind: "tool_use"; id: string; name: string; summary: string };

export type TranscriptItem =
  | { kind: "user"; text: string }
  | { kind: "text"; text: string }
  /** The agent's reasoning; rendered folded, never mixed into the answer. */
  | { kind: "thinking"; text: string }
  | {
      kind: "tool_use";
      id: string;
      name: string;
      summary: string;
      /** Filled in when the tool returns; absent while it is still running. */
      result?: { isError: boolean; text: string };
      /** Subagent output for a Task call. */
      children: ToolChild[];
    }
  /** Denied without asking: auto-mode classifier, a deny rule, or `dontAsk`. */
  | { kind: "denied"; tool: string; reason: string; message: string }
  | {
      kind: "permission_request";
      requestId: string;
      tool: string;
      args: unknown;
      title: string | null;
      resolved: "pending" | "allowed" | "denied";
    }
  | { kind: "status"; status: SessionStatus }
  /** What the user answered in a dialog, kept so the transcript tells the whole story. */
  | { kind: "dialog"; title: string; lines: string[] }
  /** A runtime switch (model/effort/permissions) applied mid-session. */
  | { kind: "settings"; text: string };

export const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;

/**
 * Thinking budgets. `default` leaves the CLI alone — which in testing meant the models
 * produced no thinking at all, so nothing was shown; a budget is what makes reasoning
 * visible. Mirrors `THINKING_OPTIONS` in the Rust core.
 */
export const THINKING_OPTIONS = ["default", "off", "4000", "16000", "32000"] as const;

export const THINKING_LABELS: Record<string, string> = {
  default: "thinking: CLI default",
  off: "thinking: off",
  "4000": "thinking: 4k budget",
  "16000": "thinking: 16k budget",
  "32000": "thinking: 32k budget",
};
// `bypassPermissions` is deliberately absent: with it the SDK never calls canUseTool,
// so pushes/PRs/commits would run without ever reaching the gate. The plumbing still
// accepts it (config file / settings), it just isn't offered in the UI.
export const PERMISSION_MODES = ["default", "acceptEdits", "auto", "plan"] as const;
export const READ_ONLY_MODE = "plan";

/**
 * `auto` lets a model classifier answer permission prompts. What it approves never
 * reaches `canUseTool`, and that callback is what the commit/push/PR gate hangs on — so
 * in this mode a gated command can execute without the approval dialog. Offered because
 * it is genuinely useful for low-stakes work, labelled so the trade-off is visible, and
 * to be revisited once the gate moves to a PreToolUse hook (which fires regardless of
 * permission mode).
 */
export const GATE_UNSAFE_MODES: readonly string[] = ["auto"];

export const PERMISSION_MODE_LABELS: Record<string, string> = {
  default: "default (asks each time)",
  acceptEdits: "acceptEdits (edits auto, commands ask)",
  auto: "auto — gate not guaranteed",
  plan: "plan (read-only)",
};
export const ACTIVE_STATUSES: SessionStatus[] = ["spawning", "streaming", "awaiting_input"];

/** Order the checklist renders in: what is happening now, then what is left. */
export const TODO_STATUS_ORDER: Record<string, number> = {
  failed: 0,
  in_progress: 1,
  pending: 2,
  completed: 3,
};

export function isTerminalStatus(status: SessionStatus): boolean {
  return status === "done" || status === "failed" || status === "cancelled";
}
