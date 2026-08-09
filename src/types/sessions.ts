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
  /** Extra tool profile the session ran with (`review` gets `ask_original_agent`). */
  tools_profile: string | null;
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

/** Dialog kinds Maestro renders (see the sidecar's constants of the same names). */
export const DIALOG_ASK_USER_QUESTION = "ask_user_question";
export const DIALOG_PLAN_APPROVAL = "plan_approval";
export const DIALOG_ELICITATION = "elicitation";
export const DIALOG_REVIEW_COMMENTS = "review_comments";

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

/**
 * One PR review comment, either the agent's draft (from a `review_comments` payload)
 * or the human's edited version sent back as the answer — same shape both directions.
 * A brand-new comment is anchored to `path`+`line`; a reply to a comment that already
 * exists sets `in_reply_to` instead (its `path`/`line` are for display only).
 */
export interface ReviewCommentDraft {
  path: string;
  line: number;
  side?: "LEFT" | "RIGHT";
  body: string;
  in_reply_to?: number;
}

/** Payload of a `review_comments` dialog: `submit_review_comments`'s own input. */
export interface ReviewCommentsPayload {
  pr: number;
  comments: ReviewCommentDraft[];
  summary?: string;
}

/** Maestro's answer to a dialog; the sidecar maps it to the CLI's result shape. */
export interface DialogAnswer {
  answers?: Record<string, string>;
  annotations?: Record<string, { preview?: string; notes?: string }>;
  /**
   * Free text for the agent. For a declined approval (the plan review, or review
   * comments): what to change before trying again. For an *approved* review-comments
   * submission: a human-readable summary of what actually got posted.
   */
  feedback?: string;
  /** Approval dialogs (the plan review, review comments) answer with this instead of `answers`. */
  approved?: boolean;
  /** The (possibly edited) comments the human approved — review_comments dialogs only. */
  comments?: ReviewCommentDraft[];
}

/** What an MCP server is asking for. */
export interface ElicitationPayload {
  server: string;
  message: string;
  mode: string;
  url?: string;
  title?: string;
  description?: string;
  /** True when the server wants structured input Maestro cannot render. */
  form: boolean;
}

/** Narrow an unknown payload to the elicitation shape, or null. */
export function asElicitation(payload: unknown): ElicitationPayload | null {
  if (typeof payload !== "object" || payload === null) return null;
  const p = payload as Partial<ElicitationPayload>;
  if (typeof p.server !== "string" || typeof p.message !== "string") return null;
  return {
    server: p.server,
    message: p.message,
    mode: typeof p.mode === "string" ? p.mode : "form",
    url: typeof p.url === "string" ? p.url : undefined,
    title: typeof p.title === "string" ? p.title : undefined,
    description: typeof p.description === "string" ? p.description : undefined,
    form: p.form === true,
  };
}

/** Read the plan out of a `plan_approval` payload, or null when it has no plan text. */
export function asPlanText(payload: unknown): string | null {
  if (typeof payload !== "object" || payload === null) return null;
  const plan = (payload as { plan?: unknown }).plan;
  return typeof plan === "string" && plan.trim().length > 0 ? plan : null;
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

/** Narrow an unknown dialog payload to the review-comments shape, or null. */
export function asReviewCommentsPayload(payload: unknown): ReviewCommentsPayload | null {
  if (typeof payload !== "object" || payload === null) return null;
  const p = payload as Partial<ReviewCommentsPayload>;
  if (typeof p.pr !== "number" || !Array.isArray(p.comments) || p.comments.length === 0) {
    return null;
  }
  const valid = p.comments.every(
    (c) =>
      typeof c === "object" &&
      c !== null &&
      typeof (c as ReviewCommentDraft).path === "string" &&
      typeof (c as ReviewCommentDraft).line === "number" &&
      typeof (c as ReviewCommentDraft).body === "string",
  );
  return valid
    ? {
        pr: p.pr,
        comments: p.comments,
        summary: typeof p.summary === "string" ? p.summary : undefined,
      }
    : null;
}

/** An image pasted into the chat, sent to the model alongside the prompt. */
export interface Attachment {
  media_type: string;
  /** Base64 payload, no data-URI prefix. */
  data: string;
}

/** Largest image Maestro will attach; bigger pastes are rejected with a message. */
export const MAX_ATTACHMENT_BYTES = 5 * 1024 * 1024;

/** A subagent profile a session can delegate to (`Task` with this type). */
export interface AgentInfo {
  name: string;
  description: string;
  model: string;
}

/** Connection state of one MCP server of a session. */
export interface McpServerInfo {
  name: string;
  status: string;
  tool_count: number;
  detail: string;
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

/**
 * Session types the user can start. `escalation` is absent on purpose: those are spawned by
 * the core to answer `ask_original_agent`, never by hand.
 */
export const SESSION_TYPES = ["manual", "research", "implementation", "review_fix"] as const;

export const SESSION_TYPE_LABELS: Record<string, string> = {
  manual: "manual (no extra behaviour)",
  research: "research (read-only work)",
  implementation: "implementation (writes TASK_NOTES.md on close)",
  review_fix: "review fix (can ask the original agent)",
};

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
/**
 * `bypassPermissions` and `dontAsk` are deliberately absent: they are not needed now that
 * `auto` is gate-safe, and neither has been verified against the PreToolUse gate. The
 * plumbing still accepts them from the config file — they just are not offered here.
 */
export const PERMISSION_MODES = ["default", "acceptEdits", "auto", "plan"] as const;
export const READ_ONLY_MODE = "plan";

/** Mirrors the backend's `is_writer_mode` exactly: everything but plan mode
 * counts as a writer, including an unset mode (`null`). */
export function isWriterMode(permissionMode: string | null): boolean {
  return permissionMode !== READ_ONLY_MODE;
}

export const PERMISSION_MODE_LABELS: Record<string, string> = {
  default: "default (asks each time)",
  acceptEdits: "acceptEdits (edits auto, commands ask)",
  auto: "auto (a classifier answers prompts; the gate still applies)",
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

/** Mirrors `SessionPreset` in src-tauri/src/core/store/. A named combo of
 * composer settings, reusable with one click instead of reconfiguring the
 * same repeat workflow every time. */
export interface SessionPreset {
  id: string;
  name: string;
  session_type: string | null;
  model: string | null;
  effort: string | null;
  permission_mode: string | null;
  tools_profile: string | null;
  created_at: string;
}

/** Mirrors `SessionSearchResult` in src-tauri/src/core/store/. */
export interface SessionSearchResult {
  session_id: string;
  branch: string;
  session_type: string;
  status: string;
  snippet: string;
}
