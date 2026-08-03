// Mirrors the Rust types in src-tauri/src/core/questions/mod.rs.

export type LineQuestionStatus = "waiting" | "streaming" | "done";

export interface LineQuestion {
  id: string;
  sessionId: string;
  path: string;
  lineStart: number;
  lineEnd: number;
  question: string;
  answer: string;
  status: LineQuestionStatus;
}

/** Shape returned by the `ask_line_question` IPC command. */
export interface LineQuestionInfo {
  question_id: string;
  session_id: string;
  branch: string;
  path: string;
  line_start: number;
  line_end: number;
  question: string;
}
