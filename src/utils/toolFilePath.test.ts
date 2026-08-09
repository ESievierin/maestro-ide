import { describe, expect, it } from "vitest";
import type { TranscriptItem } from "../types/sessions";
import { editedFilePath, relativeToWorktree } from "./toolFilePath";

function toolUse(name: string, summary: string): Extract<TranscriptItem, { kind: "tool_use" }> {
  return { kind: "tool_use", id: "t1", name, summary, children: [] };
}

describe("editedFilePath", () => {
  it("extracts the file path from an Edit call", () => {
    const item = toolUse(
      "Edit",
      '{"file_path":"C:\\\\work\\\\repo\\\\src\\\\lib.rs","old_string":"a","new_string":"b"}',
    );
    expect(editedFilePath(item)).toBe("C:\\work\\repo\\src\\lib.rs");
  });

  it("extracts the file path from a Write call", () => {
    const item = toolUse("Write", '{"file_path":"/repo/src/main.ts","content":"..."}');
    expect(editedFilePath(item)).toBe("/repo/src/main.ts");
  });

  it("survives truncation of the rest of the input", () => {
    const longEdit = "x".repeat(500);
    const summary = `{"file_path":"/repo/src/lib.rs","old_string":"${longEdit}`.slice(0, 300);
    expect(editedFilePath(toolUse("Edit", summary))).toBe("/repo/src/lib.rs");
  });

  it("returns null for a non-file-editing tool", () => {
    expect(editedFilePath(toolUse("Bash", '{"command":"ls"}'))).toBeNull();
  });

  it("returns null when file_path is not present", () => {
    expect(editedFilePath(toolUse("Edit", '{"old_string":"a"}'))).toBeNull();
  });

  it("unescapes JSON string escapes in the path", () => {
    const item = toolUse("Write", '{"file_path":"C:\\\\Users\\\\me\\\\a.txt","content":""}');
    expect(editedFilePath(item)).toBe("C:\\Users\\me\\a.txt");
  });
});

describe("relativeToWorktree", () => {
  it("strips the worktree root and normalizes slashes", () => {
    expect(relativeToWorktree("C:\\work\\repo\\src\\lib.rs", "C:/work/repo")).toBe("src/lib.rs");
  });

  it("is case-insensitive on the drive/root prefix (Windows paths)", () => {
    expect(relativeToWorktree("c:\\Work\\Repo\\src\\lib.rs", "C:/Work/Repo")).toBe("src/lib.rs");
  });

  it("returns null when the path is not inside the worktree", () => {
    expect(relativeToWorktree("C:/elsewhere/lib.rs", "C:/work/repo")).toBeNull();
  });

  it("handles a trailing slash on the worktree root", () => {
    expect(relativeToWorktree("C:/work/repo/src/lib.rs", "C:/work/repo/")).toBe("src/lib.rs");
  });
});
