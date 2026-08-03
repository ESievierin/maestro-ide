import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { LineQuestion, LineQuestionInfo } from "../types/questions";
import type { DiffScope } from "../types/diffs";
import { onBusEvent } from "./events";

const fileKey = (branch: string, path: string) => `${branch}|${path}`;

/**
 * Stable empty array. zustand 5 reads the selector on every `getSnapshot()` with no
 * memoization, so a selector that builds a fresh value each call makes React see an
 * endless stream of "changed" snapshots and blow the update depth. Every selector here
 * must therefore return a value already stored in state — never a mapped/derived one.
 */
const EMPTY: readonly LineQuestion[] = Object.freeze([]);

export interface AskLineQuestionInput {
  branch: string;
  path: string;
  start: number;
  end: number;
  question: string;
  scope: DiffScope;
}

interface QuestionsState {
  /** Questions per branch+path, ask order. The array identity only changes when that
   *  file's questions change, which is what keeps the selector stable. */
  byFile: Record<string, readonly LineQuestion[]>;
  /** question id → file key, so a streamed delta can find its file in one step. */
  fileOfQuestion: Record<string, string>;
  /** Which question each session is currently answering (core-driven). */
  answeringBySession: Record<string, string>;
  error: string | null;

  ask: (input: AskLineQuestionInput) => Promise<LineQuestion | null>;
  clearError: () => void;
}

export const useQuestions = create<QuestionsState>((set) => ({
  byFile: {},
  fileOfQuestion: {},
  answeringBySession: {},
  error: null,

  ask: async ({ branch, path, start, end, question, scope }) => {
    try {
      const info = await invoke<LineQuestionInfo>("ask_line_question", {
        branch,
        path,
        start,
        end,
        question,
        scope,
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
        fileOfQuestion: { ...s.fileOfQuestion, [item.id]: key },
      }));
      return item;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  clearError: () => set({ error: null }),
}));

/** Questions of one file, newest last. Returns the stored array as-is. */
export function selectQuestions(branch: string, path: string) {
  const key = fileKey(branch, path);
  return (s: QuestionsState): readonly LineQuestion[] => s.byFile[key] ?? EMPTY;
}

/** Replace one question in place; only its file's array gets a new identity. */
function patch(questionId: string, update: (q: LineQuestion) => LineQuestion) {
  useQuestions.setState((s) => {
    const key = s.fileOfQuestion[questionId];
    const list = key ? s.byFile[key] : undefined;
    if (!key || !list) return {};
    const next = list.map((q) => (q.id === questionId ? update(q) : q));
    return { byFile: { ...s.byFile, [key]: next } };
  });
}

// The core owns the question lifecycle: `question.answering` says which question a
// session's stream belongs to, `question.answered` closes it. Deltas outside that
// window belong to the session's own task and are ignored here.
onBusEvent((event) => {
  switch (event.type) {
    case "question.answering": {
      const { question_id, session_id } = event.data;
      useQuestions.setState((s) => ({
        answeringBySession: { ...s.answeringBySession, [session_id]: question_id },
      }));
      patch(question_id, (q) => ({ ...q, status: "streaming" }));
      break;
    }
    case "question.answered": {
      const { question_id, session_id, ok } = event.data;
      useQuestions.setState((s) => {
        if (s.answeringBySession[session_id] !== question_id) return {};
        const answeringBySession = { ...s.answeringBySession };
        delete answeringBySession[session_id];
        return { answeringBySession };
      });
      patch(question_id, (q) => ({
        ...q,
        status: "done",
        answer: ok ? q.answer : q.answer || "(the session ended without answering)",
      }));
      break;
    }
    case "session.stream_delta": {
      const { session_id, text } = event.data;
      const questionId = useQuestions.getState().answeringBySession[session_id];
      if (!questionId) break;
      patch(questionId, (q) => ({ ...q, answer: q.answer + text, status: "streaming" }));
      break;
    }
  }
});
