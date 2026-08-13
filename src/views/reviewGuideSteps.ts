// Parsing for the agent-generated review guide — kept out of the component so
// the tolerant-JSON handling is unit-testable.

export type GuideCategory = "core-logic" | "supporting" | "boilerplate" | "tests";

export interface GuideStep {
  title: string;
  why: string;
  files: string[];
  category: GuideCategory;
}

const CATEGORIES: GuideCategory[] = ["core-logic", "supporting", "boilerplate", "tests"];

/**
 * Parse the agent's reply into review-guide steps. Tolerant on purpose: models
 * wrap JSON in fences, prepend a sentence, or invent a category — all of that
 * is recoverable. Only entries whose files actually exist in the diff survive
 * (`knownFiles`); a step left with no valid files is dropped. Returns `null`
 * when nothing parseable is found.
 */
export function parseReviewGuide(text: string, knownFiles: string[]): GuideStep[] | null {
  const start = text.indexOf("{");
  const end = text.lastIndexOf("}");
  if (start === -1 || end <= start) return null;

  let parsed: unknown;
  try {
    parsed = JSON.parse(text.slice(start, end + 1));
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null) return null;
  const rawSteps = (parsed as { steps?: unknown }).steps;
  if (!Array.isArray(rawSteps)) return null;

  const known = new Set(knownFiles);
  const seen = new Set<string>();
  const steps: GuideStep[] = [];
  for (const raw of rawSteps) {
    if (typeof raw !== "object" || raw === null) continue;
    const step = raw as Record<string, unknown>;
    const title = typeof step.title === "string" ? step.title.trim() : "";
    if (!title) continue;
    const files = (Array.isArray(step.files) ? step.files : [])
      .filter((f): f is string => typeof f === "string")
      .map((f) => f.trim())
      .filter((f) => known.has(f) && !seen.has(f));
    if (files.length === 0) continue;
    for (const f of files) seen.add(f);
    const category = CATEGORIES.includes(step.category as GuideCategory)
      ? (step.category as GuideCategory)
      : "supporting";
    steps.push({
      title,
      why: typeof step.why === "string" ? step.why.trim() : "",
      files,
      category,
    });
  }
  return steps.length > 0 ? steps : null;
}

/** Short badge text per category. */
export const CATEGORY_LABELS: Record<GuideCategory, string> = {
  "core-logic": "logic",
  supporting: "support",
  boilerplate: "boiler",
  tests: "tests",
};
