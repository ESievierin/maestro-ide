import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { onBusEvent } from "./events";

interface BranchUsageRow {
  branch: string;
  totals: { cost_usd: number; turns: number; input_tokens: number; output_tokens: number };
}

/** Lifetime agent cost per branch, from the store's persisted session_usage
 * table — unlike the in-memory usage map, this survives app restarts. Kept
 * fresh by refetching whenever any session reports usage. */
interface BranchUsageState {
  costByBranch: Record<string, number>;
  fetch: () => Promise<void>;
}

export const useBranchUsage = create<BranchUsageState>((set) => ({
  costByBranch: {},
  fetch: async () => {
    try {
      const rows = await invoke<BranchUsageRow[]>("get_usage_by_branch");
      const costByBranch: Record<string, number> = {};
      for (const row of rows) costByBranch[row.branch] = row.totals.cost_usd;
      set({ costByBranch });
    } catch {
      // Non-critical decoration; the badge just stays at its last value.
    }
  },
}));

// One usage event ends one turn — cheap enough to refetch the aggregate, and
// it keeps this store correct even for spend recorded by other windows/runs.
let scheduled = false;
onBusEvent((event) => {
  if (event.type !== "session.usage" || scheduled) return;
  scheduled = true;
  setTimeout(() => {
    scheduled = false;
    void useBranchUsage.getState().fetch();
  }, 500);
});
