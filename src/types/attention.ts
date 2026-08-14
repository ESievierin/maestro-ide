// Mirrors src-tauri/src/core/attention/mod.rs.

export type AttentionKind =
  | "permission_request"
  | "question"
  | "gate"
  | "session_failed"
  | "line_question"
  | "pr_review_ready"
  | "red_team_ready"
  | "red_team_launched";

export type AttentionTarget = "chat" | "gate" | "diff" | "pr_replies" | "notes";

export interface AttentionItem {
  id: string;
  kind: AttentionKind;
  target: AttentionTarget;
  branch: string | null;
  session_id: string | null;
  message: string;
  created_at: string;
}

export const KIND_LABEL: Record<AttentionKind, string> = {
  gate: "approval",
  permission_request: "permission",
  question: "question",
  session_failed: "failed",
  line_question: "answer",
  pr_review_ready: "reply ready",
  red_team_ready: "findings",
  red_team_launched: "red team",
};
