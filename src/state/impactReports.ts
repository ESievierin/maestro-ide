import { create } from "zustand";
import type { ImpactReport } from "../types/diffs";

/** One computed blast radius per branch, with the diff signature it was built
 * against. Lives in a store (not a component-local cache) so other panels —
 * the review guide flags high-impact steps — can react when a radius lands. */
interface ImpactReportsState {
  byBranch: Record<string, { report: ImpactReport; signature: string }>;
  set: (branch: string, report: ImpactReport, signature: string) => void;
}

export const useImpactReports = create<ImpactReportsState>((set) => ({
  byBranch: {},
  set: (branch, report, signature) =>
    set((s) => ({ byBranch: { ...s.byBranch, [branch]: { report, signature } } })),
}));

/**
 * How many distinct outside files reference each of `files`, according to the
 * report's first ring. The union across a step's files, so a reviewer reads
 * "N dependents" as "N files elsewhere care about this step".
 */
export function stepDependents(report: ImpactReport | undefined, files: string[]): number {
  if (!report) return 0;
  const wanted = new Set(files);
  const dependents = new Set<string>();
  for (const impacted of report.impacted) {
    if (impacted.distance !== 1) continue;
    if (impacted.links.some((l) => wanted.has(l.target))) dependents.add(impacted.path);
  }
  return dependents.size;
}
