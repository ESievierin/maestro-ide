import { describe, expect, it } from "vitest";
import { parseDiffStats, parseFileHunks } from "./diffStats";

describe("parseDiffStats", () => {
  it("counts additions and deletions for a modified file", () => {
    const unified = [
      "diff --git a/src/lib.rs b/src/lib.rs",
      "index 111..222 100644",
      "--- a/src/lib.rs",
      "+++ b/src/lib.rs",
      "@@ -1,3 +1,3 @@",
      " fn main() {",
      "-    old();",
      "+    new_one();",
      "+    new_two();",
      " }",
      "",
    ].join("\n");
    expect(parseDiffStats(unified)).toEqual({
      "src/lib.rs": { additions: 2, deletions: 1 },
    });
  });

  it("attributes an added file's lines under its path", () => {
    const unified = [
      "diff --git a/new.txt b/new.txt",
      "new file mode 100644",
      "index 000..abc",
      "--- /dev/null",
      "+++ b/new.txt",
      "@@ -0,0 +1,2 @@",
      "+line one",
      "+line two",
      "",
    ].join("\n");
    expect(parseDiffStats(unified)).toEqual({
      "new.txt": { additions: 2, deletions: 0 },
    });
  });

  it("falls back to the a/ path for a deleted file (no b/ side)", () => {
    const unified = [
      "diff --git a/old.txt b/old.txt",
      "deleted file mode 100644",
      "index abc..000",
      "--- a/old.txt",
      "+++ /dev/null",
      "@@ -1,2 +0,0 @@",
      "-gone one",
      "-gone two",
      "",
    ].join("\n");
    expect(parseDiffStats(unified)).toEqual({
      "old.txt": { additions: 0, deletions: 2 },
    });
  });

  it("attributes a rename's lines under the new path", () => {
    const unified = [
      "diff --git a/old-name.rs b/new-name.rs",
      "similarity index 90%",
      "rename from old-name.rs",
      "rename to new-name.rs",
      "index 111..222 100644",
      "--- a/old-name.rs",
      "+++ b/new-name.rs",
      "@@ -1,1 +1,2 @@",
      " unchanged();",
      "+added();",
      "",
    ].join("\n");
    expect(parseDiffStats(unified)).toEqual({
      "new-name.rs": { additions: 1, deletions: 0 },
    });
  });

  it("handles multiple files in one unified diff independently", () => {
    const unified = [
      "diff --git a/a.txt b/a.txt",
      "--- a/a.txt",
      "+++ b/a.txt",
      "@@ -1,1 +1,1 @@",
      "-old a",
      "+new a",
      "diff --git a/b.txt b/b.txt",
      "--- a/b.txt",
      "+++ b/b.txt",
      "@@ -1,1 +1,2 @@",
      " unchanged b",
      "+new b line",
      "",
    ].join("\n");
    expect(parseDiffStats(unified)).toEqual({
      "a.txt": { additions: 1, deletions: 1 },
      "b.txt": { additions: 1, deletions: 0 },
    });
  });

  it("returns an empty object for an empty diff", () => {
    expect(parseDiffStats("")).toEqual({});
  });
});

describe("parseFileHunks", () => {
  it("reads the new-file line range from a hunk header", () => {
    const unified = [
      "diff --git a/src/lib.rs b/src/lib.rs",
      "--- a/src/lib.rs",
      "+++ b/src/lib.rs",
      "@@ -10,3 +10,4 @@",
      " fn main() {",
      "-    old();",
      "+    new_one();",
      "+    new_two();",
      " }",
      "",
    ].join("\n");
    expect(parseFileHunks(unified, "src/lib.rs")).toEqual([{ start: 10, end: 13 }]);
  });

  it("collects multiple hunks in one file, ignoring other files", () => {
    const unified = [
      "diff --git a/a.txt b/a.txt",
      "--- a/a.txt",
      "+++ b/a.txt",
      "@@ -1,1 +1,1 @@",
      "-old a",
      "+new a",
      "@@ -20,1 +20,2 @@",
      " unchanged",
      "+extra",
      "diff --git a/b.txt b/b.txt",
      "--- a/b.txt",
      "+++ b/b.txt",
      "@@ -5,1 +5,1 @@",
      "-old b",
      "+new b",
      "",
    ].join("\n");
    expect(parseFileHunks(unified, "a.txt")).toEqual([
      { start: 1, end: 1 },
      { start: 20, end: 21 },
    ]);
  });

  it("clamps a pure-deletion hunk (new count 0) to a single line marker", () => {
    const unified = [
      "diff --git a/x.txt b/x.txt",
      "--- a/x.txt",
      "+++ b/x.txt",
      "@@ -5,2 +4,0 @@",
      "-gone one",
      "-gone two",
      "",
    ].join("\n");
    expect(parseFileHunks(unified, "x.txt")).toEqual([{ start: 4, end: 4 }]);
  });

  it("defaults an omitted count to 1", () => {
    const unified = [
      "diff --git a/x.txt b/x.txt",
      "--- a/x.txt",
      "+++ b/x.txt",
      "@@ -1 +1 @@",
      "-a",
      "+b",
      "",
    ].join("\n");
    expect(parseFileHunks(unified, "x.txt")).toEqual([{ start: 1, end: 1 }]);
  });

  it("returns nothing for an unknown path or empty diff", () => {
    expect(parseFileHunks("", "x.txt")).toEqual([]);
    expect(parseFileHunks("diff --git a/x.txt b/x.txt\n", "")).toEqual([]);
  });
});
