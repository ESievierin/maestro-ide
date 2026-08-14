import { describe, expect, it } from "vitest";
import { stepDependents } from "./impactReports";
import type { ImpactReport } from "../types/diffs";

function report(impacted: ImpactReport["impacted"]): ImpactReport {
  return {
    branch: "impl/x",
    analyzed: ["src/core.ts", "src/util.ts"],
    skipped: [],
    impacted,
    scanned: 10,
    truncated: false,
  };
}

describe("stepDependents", () => {
  it("counts distinct first-ring files referencing any of the step's files", () => {
    const rep = report([
      {
        path: "src/a.ts",
        distance: 1,
        kind: "import",
        links: [{ target: "src/core.ts", kind: "import", matched: "core" }],
      },
      {
        path: "src/b.ts",
        distance: 1,
        kind: "reference",
        links: [
          { target: "src/core.ts", kind: "reference", matched: "core" },
          { target: "src/util.ts", kind: "reference", matched: "util" },
        ],
      },
    ]);
    expect(stepDependents(rep, ["src/core.ts"])).toBe(2);
    expect(stepDependents(rep, ["src/util.ts"])).toBe(1);
    // A file referencing both step files still counts once.
    expect(stepDependents(rep, ["src/core.ts", "src/util.ts"])).toBe(2);
  });

  it("ignores the second ring — those import ring-1 files, not the step's", () => {
    const rep = report([
      {
        path: "src/c.ts",
        distance: 2,
        kind: "import",
        links: [{ target: "src/core.ts", kind: "import", matched: "core" }],
      },
    ]);
    expect(stepDependents(rep, ["src/core.ts"])).toBe(0);
  });

  it("is zero without a report or without matches", () => {
    expect(stepDependents(undefined, ["src/core.ts"])).toBe(0);
    expect(stepDependents(report([]), ["src/core.ts"])).toBe(0);
    const rep = report([
      {
        path: "src/a.ts",
        distance: 1,
        kind: "import",
        links: [{ target: "src/other.ts", kind: "import", matched: "other" }],
      },
    ]);
    expect(stepDependents(rep, ["src/core.ts"])).toBe(0);
  });
});
