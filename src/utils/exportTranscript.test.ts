import { describe, expect, it } from "vitest";
import type { Session, TranscriptItem } from "../types/sessions";
import { defaultTranscriptFilename, transcriptToMarkdown } from "./exportTranscript";

function session(overrides: Partial<Session> = {}): Session {
  return {
    id: "sess-12345678-abcd",
    branch: "impl/T-9-x",
    session_type: "implementation",
    status: "done",
    model: "sonnet",
    effort: "high",
    permission_mode: null,
    thinking: null,
    tools_profile: null,
    sdk_session_id: null,
    created_at: "2026-08-09T00:00:00Z",
    updated_at: "2026-08-09T00:05:00Z",
    ...overrides,
  };
}

describe("transcriptToMarkdown", () => {
  it("includes a header with branch, type, model, and effort", () => {
    const md = transcriptToMarkdown(session(), []);
    expect(md).toContain("# Session transcript — impl/T-9-x (implementation)");
    expect(md).toContain("- Model: sonnet");
    expect(md).toContain("- Effort: high");
    expect(md).toContain("- Started: 2026-08-09T00:00:00Z");
  });

  it("falls back to 'default' for an unset model/effort", () => {
    const md = transcriptToMarkdown(session({ model: null, effort: null }), []);
    expect(md).toContain("- Model: default");
    expect(md).toContain("- Effort: default");
  });

  it("renders user and assistant text with role labels", () => {
    const items: TranscriptItem[] = [
      { kind: "user", text: "fix the bug" },
      { kind: "text", text: "Found it, fixing now." },
    ];
    const md = transcriptToMarkdown(session(), items);
    expect(md).toContain("**You:**\n\nfix the bug");
    expect(md).toContain("**Claude:**\n\nFound it, fixing now.");
  });

  it("renders a tool call with its result and nested subagent children", () => {
    const items: TranscriptItem[] = [
      {
        kind: "tool_use",
        id: "t1",
        name: "Task",
        summary: "spawn a research subagent",
        children: [
          { kind: "text", text: "subagent output" },
          { kind: "tool_use", id: "c1", name: "Read", summary: "read file.ts" },
        ],
        result: { isError: false, text: "done" },
      },
    ];
    const md = transcriptToMarkdown(session(), items);
    expect(md).toContain("**Tool: `Task`**");
    expect(md).toContain("spawn a research subagent");
    expect(md).toContain("subagent output");
    expect(md).toContain("`Read`: read file.ts");
    expect(md).toContain("Result:\n\n```\ndone\n```");
  });

  it("labels a failed tool result as an error, not a result", () => {
    const items: TranscriptItem[] = [
      {
        kind: "tool_use",
        id: "t1",
        name: "Bash",
        summary: "run tests",
        children: [],
        result: { isError: true, text: "2 tests failed" },
      },
    ];
    const md = transcriptToMarkdown(session(), items);
    expect(md).toContain("Error:\n\n```\n2 tests failed\n```");
  });

  it("renders every other item kind without throwing", () => {
    const items: TranscriptItem[] = [
      { kind: "thinking", text: "considering approaches" },
      { kind: "denied", tool: "Bash", reason: "rule", message: "blocked by a deny rule" },
      {
        kind: "permission_request",
        requestId: "r1",
        tool: "Edit",
        args: {},
        title: "Edit file.ts",
        resolved: "allowed",
      },
      { kind: "status", status: "failed" },
      { kind: "dialog", title: "Pick one", lines: ["option a", "option b"] },
      { kind: "settings", text: "model → opus" },
    ];
    const md = transcriptToMarkdown(session(), items);
    expect(md).toContain("considering approaches");
    expect(md).toContain("Denied — Bash");
    expect(md).toContain("Permission requested — Edit: Edit file.ts");
    expect(md).toContain("— failed —");
    expect(md).toContain("- option a");
    expect(md).toContain("Settings changed: model → opus");
  });

  it("separates items with a horizontal rule", () => {
    const items: TranscriptItem[] = [
      { kind: "user", text: "a" },
      { kind: "text", text: "b" },
    ];
    const md = transcriptToMarkdown(session(), items);
    expect(md).toContain("a\n\n---\n\n**Claude:**");
  });
});

describe("defaultTranscriptFilename", () => {
  it("sanitizes slashes out of the branch name", () => {
    const name = defaultTranscriptFilename(session({ branch: "impl/T-9-x" }));
    expect(name).not.toContain("/");
    expect(name).toBe("transcript-impl-T-9-x-sess-123.md");
  });

  it("strips windows-reserved characters too", () => {
    const name = defaultTranscriptFilename(session({ branch: 'weird:branch*name?"<>|' }));
    expect(name).not.toMatch(/[\\/:*?"<>|]/);
    expect(name.startsWith("transcript-weird-branch-name")).toBe(true);
    expect(name.endsWith("-sess-123.md")).toBe(true);
  });
});
