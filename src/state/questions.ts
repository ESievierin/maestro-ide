import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { LineQuestion, LineQuestionInfo } from "../types/questions";
import { onBusEvent } from "./events";

const fileKey = (branch: string, path: string) => `${branch}|${path}`;

export interface AskLineQuestionInput {
  branch: string;
  path: string;
  start: number;
  end: number;
  question: string;
}

interface QuestionsState {
  /** Line questions per branch+path; persists per file while the app runs. */
  byFile: Record<string, LineQuestion[]>;
  error: string | null;

  ask: (input: AskLineQuestionInput) => Promise<LineQuestion | null>;
  clearError: () => void;
}

export const useQuestions = create<QuestionsState>((set) => ({
  byFile: {},
  error: null,

  ask: async ({ branch, path, start, end, question }) => {
    try {
      const info = await invoke<LineQuestionInfo>("ask_line_question", {
        branch,
        path,
        start,
        end,
        question,
      });
      const item: LineQuestion = {
        id: info.question_id,
        sessionId: info.session_id,
        path: info.path,
        lineStart: info.line_start,
        lineEnd: info.line_end,
        question: info.question,
        answer: "",
        status: "waiting",
      };
      const key = fileKey(branch, path);
      set((s) => ({
        byFile: { ...s.byFile, [key]: [...(s.byFile[key] ?? []), item] },
      }));
      return item;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  clearError: () => set({ error: null }),
}));

export function selectQuestions(branch: string, path: string) {
  return (s: QuestionsState) => s.byFile[fileKey(branch, path)] ?? [];
}

/** Apply `update` to the first non-done question tracking `sessionId`, across all files. */
function updateBySession(sessionId: string, update: (q: LineQuestion) => LineQuestion) {
  useQuestions.setState((s) => {
    let changed = false;
    const byFile = { ...s.byFile };
    for (const key of Object.keys(byFile)) {
      const list = byFile[key];
      const idx = list.findIndex((q) => q.sessionId === sessionId && q.status !== "done");
      if (idx === -1) continue;
      const next = [...list];
      next[idx] = update(next[idx]);
      byFile[key] = next;
      changed = true;
    }
    return changed ? { byFile } : {};
  });
}

// Answers stream in through the same session events the chat panel uses; we just
// collect from the moment the question is asked onward — keep it simple.
onBusEvent((event) => {
  switch (event.type) {
    case "session.stream_delta": {
      const { session_id, text } = event.data;
      updateBySession(session_id, (q) => ({
        ...q,
        answer: q.answer + text,
        status: "streaming",
      }));
      break;
    }
    case "session.status_changed": {
      const { session_id, status } = event.data;
      if (status !== "streaming" && status !== "spawning") {
        updateBySession(session_id, (q) => ({ ...q, status: "done" }));
      }
      break;
    }
  }
});
