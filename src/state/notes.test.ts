import { beforeEach, describe, expect, it, vi } from "vitest";

/**
 * zustand 5 calls a selector on every `getSnapshot()` with no memoization, so a selector
 * that builds a fresh value each call makes React see an endless stream of changed
 * snapshots and blow the update depth. That crashed this app twice, so the notes store gets
 * the same guard the questions store has: the values selectors read must be stable across
 * calls when nothing changed.
 */
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (command: string, args: { branch: string }) => {
    if (command === "get_notes" || command === "refresh_notes") {
      return {
        branch: args.branch,
        path: `/tmp/${args.branch}/TASK_NOTES.md`,
        exists: true,
        unavailable: null,
        sections: [{ title: "Decisions", body: "- kept it simple" }],
        raw: "## Decisions\n\n- kept it simple\n",
        updated_at: null,
      };
    }
    throw new Error(`unexpected command: ${command}`);
  }),
}));

vi.mock("./events", () => ({ onBusEvent: () => () => {} }));

const { useNotes } = await import("./notes");

describe("notes store", () => {
  beforeEach(() => {
    useNotes.setState({ byBranch: {}, loading: {}, error: null });
  });

  it("keeps the selected notes object stable between reads", async () => {
    await useNotes.getState().fetch("impl/T-1");
    const first = useNotes.getState().byBranch["impl/T-1"];
    const second = useNotes.getState().byBranch["impl/T-1"];
    expect(first).toBe(second);
    expect(first.exists).toBe(true);
  });

  it("reading another branch leaves the first one's identity alone", async () => {
    await useNotes.getState().fetch("impl/T-1");
    const before = useNotes.getState().byBranch["impl/T-1"];
    await useNotes.getState().fetch("impl/T-2");
    expect(useNotes.getState().byBranch["impl/T-1"]).toBe(before);
    expect(useNotes.getState().byBranch["impl/T-2"].branch).toBe("impl/T-2");
  });

  it("clears the loading flag after a read, so no branch is stuck spinning", async () => {
    const pending = useNotes.getState().fetch("impl/T-3");
    expect(useNotes.getState().loading["impl/T-3"]).toBe(true);
    await pending;
    expect(useNotes.getState().loading["impl/T-3"]).toBeUndefined();
  });
});
