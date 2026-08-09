// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * A mount test, because green unit tests have twice hidden a panel that crashed on mount —
 * usually a zustand selector building a fresh value on every snapshot, which React turns
 * into an infinite update loop. Rendering the real component with the real store is the
 * only check that catches that.
 */
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(async (_command: string, args: { branch: string }) => ({
    branch: args.branch,
    path: `/tmp/${args.branch}/TASK_NOTES.md`,
    exists: true,
    unavailable: null,
    sections: [{ title: "Decisions", body: "- kept it simple" }],
    raw: "## Decisions\n\n- kept it simple\n",
    updated_at: null,
  })),
}));
vi.mock("../state/events", () => ({ onBusEvent: () => () => {} }));

const { NotesPanel } = await import("./NotesPanel");
const { useNotes } = await import("../state/notes");

const worktree = {
  branch: "impl/T-1-demo",
  path: "/tmp/impl/T-1-demo",
  is_primary: false,
  task_id: "T-1",
  base_branch: "main",
  pinned: false,
  status: null,
};

afterEach(() => {
  cleanup();
  useNotes.setState({ byBranch: {}, loading: {}, error: null });
});

describe("NotesPanel", () => {
  it("renders the empty state when the branch has no notes", () => {
    useNotes.setState({
      byBranch: {
        "impl/T-1-demo": {
          branch: "impl/T-1-demo",
          path: "/tmp/impl/T-1-demo/TASK_NOTES.md",
          exists: false,
          unavailable: null,
          sections: [],
          raw: "",
          updated_at: null,
        },
      },
    });
    render(<NotesPanel worktree={worktree} />);
    expect(screen.getByText(/it is written when an implementation session closes/)).toBeTruthy();
  });

  it("renders the notes markdown when they exist", () => {
    useNotes.setState({
      byBranch: {
        "impl/T-1-demo": {
          branch: "impl/T-1-demo",
          path: "/tmp/impl/T-1-demo/TASK_NOTES.md",
          exists: true,
          unavailable: null,
          sections: [{ title: "Decisions", body: "- kept it simple" }],
          raw: "## Decisions\n\n- kept it simple\n",
          updated_at: "2026-08-05T00:00:00Z",
        },
      },
    });
    render(<NotesPanel worktree={worktree} />);
    expect(screen.getByRole("heading", { name: "Decisions" })).toBeTruthy();
    // The dash becomes a real list item, so match the text, not the markdown source.
    expect(screen.getByRole("listitem").textContent).toBe("kept it simple");
  });

  it("shows the export button only once notes exist", () => {
    useNotes.setState({
      byBranch: {
        "impl/T-1-demo": {
          branch: "impl/T-1-demo",
          path: "/tmp/impl/T-1-demo/TASK_NOTES.md",
          exists: false,
          unavailable: null,
          sections: [],
          raw: "",
          updated_at: null,
        },
      },
    });
    const { rerender } = render(<NotesPanel worktree={worktree} />);
    expect(screen.queryByTitle("Export these notes as a markdown file")).toBeNull();

    useNotes.setState({
      byBranch: {
        "impl/T-1-demo": {
          branch: "impl/T-1-demo",
          path: "/tmp/impl/T-1-demo/TASK_NOTES.md",
          exists: true,
          unavailable: null,
          sections: [{ title: "Decisions", body: "- kept it simple" }],
          raw: "## Decisions\n\n- kept it simple\n",
          updated_at: null,
        },
      },
    });
    rerender(<NotesPanel worktree={worktree} />);
    expect(screen.getByTitle("Export these notes as a markdown file")).toBeTruthy();
  });

  it("explains why notes are unavailable instead of showing an empty panel", () => {
    useNotes.setState({
      byBranch: {
        "impl/T-1-demo": {
          branch: "impl/T-1-demo",
          path: null,
          exists: false,
          unavailable: "no worktree for impl/T-1-demo — notes live in the worktree",
          sections: [],
          raw: "",
          updated_at: null,
        },
      },
    });
    render(<NotesPanel worktree={worktree} />);
    expect(screen.getByText(/no worktree for impl\/T-1-demo/)).toBeTruthy();
  });
});
