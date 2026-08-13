import { describe, expect, it } from "vitest";
import { parseReviewGuide } from "./reviewGuideSteps";

const KNOWN = ["src/core/logic.rs", "src/wiring.rs", "tests/logic_test.rs"];

describe("parseReviewGuide", () => {
  it("parses a clean JSON reply", () => {
    const reply = JSON.stringify({
      steps: [
        {
          title: "Read the core rule change",
          why: "The invariant moved.",
          files: ["src/core/logic.rs"],
          category: "core-logic",
        },
        { title: "Check tests", why: "", files: ["tests/logic_test.rs"], category: "tests" },
      ],
    });
    const steps = parseReviewGuide(reply, KNOWN);
    expect(steps).toHaveLength(2);
    expect(steps?.[0].category).toBe("core-logic");
  });

  it("tolerates fences and prose around the JSON", () => {
    const reply =
      'Here is your roadmap:\n```json\n{"steps":[{"title":"Start","why":"x","files":["src/wiring.rs"],"category":"supporting"}]}\n```\nHappy reviewing!';
    const steps = parseReviewGuide(reply, KNOWN);
    expect(steps).toHaveLength(1);
    expect(steps?.[0].title).toBe("Start");
  });

  it("drops files not present in the diff and steps left empty", () => {
    const reply = JSON.stringify({
      steps: [
        { title: "Hallucinated", why: "", files: ["src/imaginary.rs"], category: "core-logic" },
        {
          title: "Real",
          why: "",
          files: ["src/core/logic.rs", "src/phantom.ts"],
          category: "core-logic",
        },
      ],
    });
    const steps = parseReviewGuide(reply, KNOWN);
    expect(steps).toHaveLength(1);
    expect(steps?.[0].files).toEqual(["src/core/logic.rs"]);
  });

  it("keeps each file in only its first step", () => {
    const reply = JSON.stringify({
      steps: [
        { title: "One", why: "", files: ["src/core/logic.rs"], category: "core-logic" },
        {
          title: "Two",
          why: "",
          files: ["src/core/logic.rs", "src/wiring.rs"],
          category: "supporting",
        },
      ],
    });
    const steps = parseReviewGuide(reply, KNOWN);
    expect(steps).toHaveLength(2);
    expect(steps?.[1].files).toEqual(["src/wiring.rs"]);
  });

  it("coerces unknown categories to supporting", () => {
    const reply = JSON.stringify({
      steps: [{ title: "Odd", why: "", files: ["src/wiring.rs"], category: "critical!!" }],
    });
    expect(parseReviewGuide(reply, KNOWN)?.[0].category).toBe("supporting");
  });

  it("returns null on garbage", () => {
    expect(parseReviewGuide("no json here", KNOWN)).toBeNull();
    expect(parseReviewGuide('{"steps": "not an array"}', KNOWN)).toBeNull();
    expect(parseReviewGuide('{"steps": []}', KNOWN)).toBeNull();
  });
});
