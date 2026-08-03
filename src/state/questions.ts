import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { LineQuestion, LineQuestionInfo } from "../types/questions";
import type { DiffScope } from "../types/diffs";
import { onBusEvent } from "./events";

const fileKey = (branch: string, path: string) => `${branch}|${path}`;

/// Stable empty array: zustand 5 has no snapshot memoization, so a fresh `[]` from a
/// selector makes React see a changed store on every render and loop forever.
const EMPTY: LineQuestion[] = [];

export interface AskLineQuestionInput {
  branch: string;
  path: string;
  start: number;
  end: number;
  question: string;
  scope: DiffScope;
}

interface QuestionsState {
  /** Every question by id — the single place its answer is mutated. */
  byId: Record<string, LineQuestion>;
  /** Question ids per branch+path, in ask order; persists per file while the app runs. */
  idsByFile: Record<string, string[]>;
  /** Which question each session is currently answering (core-driven). */
  answeringBySession: Record<string, string>;
  error: string | null;

  ask: (input: AskLineQuestionInput) => Promise<LineQuestion | null>;
  clearError: () => void;
}

export const useQuestions = create<QuestionsState>((set) => ({
  byId: {},
  idsByFile: {},
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
        byId: { ...s.byId, [item.id]: item },
        idsByFile: { ...s.idsByFile, [key]: [...(s.idsByFile[key] ?? []), item.id] },
      }));
      return item;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  clearError: () => set({ error: null }),
}));

/**
 * Questions of one file, newest last. Returns a cached array so the reference is
 * stable between renders when nothing changed (see `EMPTY`).
 */
export function selectQuestions(branch: string, path: string) {
  const key = fileKey(branch, path);
  return (s: QuestionsState): LineQuestion[] => {
    const ids = s.idsByFile[key];
    if (!ids || ids.length === 0) return EMPTY;
    return ids.map((id) => s.byId[id]).filter((q): q is LineQuestion => Boolean(q));
  };
}

function patch(questionId: string, update: (q: LineQuestion) => LineQuestion) {
  useQuestions.setState((s) => {
    const current = s.byId[questionId];
    if (!current) return {};
    return { byId: { ...s.byId, [questionId]: update(current) } };
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
        const answeringBySession = { ...s.answeringBySession };
        if (answeringBySession[session_id] === question_id) {
          delete answeringBySession[session_id];
        }
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
