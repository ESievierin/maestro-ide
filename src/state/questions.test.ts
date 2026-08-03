import { describe, expect, it, vi } from "vitest";

// The store subscribes to Tauri events at import time; stub the bridge so importing it
// in a plain Node test does not need a Tauri window.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

const { selectQuestions, useQuestions } = await import("./questions");
import type { LineQuestion } from "../types/questions";

function question(id: string, sessionId = "s1"): LineQuestion {
  return {
    id,
    sessionId,
    path: "src/lib.rs",
    lineStart: 3,
    lineEnd: 5,
    question: "why?",
    answer: "",
    status: "waiting",
  };
}

function seed(branch: string, path: string, items: LineQuestion[]) {
  const key = `${branch}|${path}`;
  useQuestions.setState({
    byFile: { [key]: items },
    fileOfQuestion: Object.fromEntries(items.map((q) => [q.id, key])),
    answeringBySession: {},
  });
}

/**
 * zustand 5 reads selectors on every `getSnapshot()` without memoizing, so a selector
 * that derives a new value each call makes React see an endless stream of changed
 * snapshots and throw "Maximum update depth exceeded". This crashed the diff viewer
 * twice, hence the guard.
 */
describe("selectQuestions identity stability", () => {
  it("returns the same reference for a file with no questions", () => {
    useQuestions.setState({ byFile: {}, fileOfQuestion: {}, answeringBySession: {} });
    const select = selectQuestions("main", "src/lib.rs");
    const first = select(useQuestions.getState());
    const second = select(useQuestions.getState());
    expect(first).toBe(second);
    expect(first).toHaveLength(0);
  });

  it("returns the same reference across unrelated state updates", () => {
    seed("main", "src/lib.rs", [question("q1")]);
    const select = selectQuestions("main", "src/lib.rs");
    const before = select(useQuestions.getState());

    // An update that does not touch this file must not change its array identity.
    useQuestions.setState((s) => ({
      answeringBySession: { ...s.answeringBySession, other: "q9" },
    }));
    expect(select(useQuestions.getState())).toBe(before);

    // A question in a different file must not either.
    useQuestions.setState((s) => ({
      byFile: { ...s.byFile, "main|src/other.rs": [question("q2")] },
      fileOfQuestion: { ...s.fileOfQuestion, q2: "main|src/other.rs" },
    }));
    expect(select(useQuestions.getState())).toBe(before);
  });

  it("changes the reference only for the file whose question was updated", () => {
    seed("main", "src/lib.rs", [question("q1")]);
    useQuestions.setState((s) => ({
      byFile: { ...s.byFile, "main|src/other.rs": [question("q2", "s2")] },
      fileOfQuestion: { ...s.fileOfQuestion, q2: "main|src/other.rs" },
      answeringBySession: { s1: "q1" },
    }));

    const selectA = selectQuestions("main", "src/lib.rs");
    const selectB = selectQuestions("main", "src/other.rs");
    const beforeB = selectB(useQuestions.getState());
    const beforeA = selectA(useQuestions.getState());

    // Simulate what a streamed delta does to the store.
    useQuestions.setState((s) => {
      const list = s.byFile["main|src/lib.rs"];
      return {
        byFile: {
          ...s.byFile,
          "main|src/lib.rs": list.map((q) =>
            q.id === "q1" ? { ...q, answer: "partial", status: "streaming" as const } : q,
          ),
        },
      };
    });

    const afterA = selectA(useQuestions.getState());
    expect(afterA).not.toBe(beforeA);
    expect(afterA[0].answer).toBe("partial");
    expect(selectB(useQuestions.getState())).toBe(beforeB);
  });
});
