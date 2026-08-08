import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { onBusEvent } from "./events";

/** Mirrors `CheckResult` in src-tauri/src/core/checks/. */
export interface CheckResult {
  branch: string;
  status: "running" | "passed" | "failed";
  exit_code: number | null;
  command: string;
  output_tail: string;
  started_at: string;
  finished_at: string | null;
}

interface ChecksState {
  /** The configured check command; null = feature hidden. Fetched once. */
  command: string | null;
  results: Record<string, CheckResult>;

  fetchCommand: () => Promise<void>;
  run: (branch: string) => Promise<void>;
  fetchResult: (branch: string) => Promise<void>;
}

let commandFetched = false;

export const useChecks = create<ChecksState>((set) => ({
  command: null,
  results: {},

  fetchCommand: async () => {
    if (commandFetched) return;
    commandFetched = true;
    try {
      set({ command: await invoke<string | null>("get_check_command") });
    } catch {
      commandFetched = false; // error.raised already surfaced it; allow a retry
    }
  },

  run: async (branch) => {
    try {
      await invoke("run_check", { branch });
    } catch {
      // "already running" / "not configured" arrive as error toasts via error.raised
    }
  },

  fetchResult: async (branch) => {
    try {
      const result = await invoke<CheckResult | null>("get_check", { branch });
      if (result) {
        set((s) => ({ results: { ...s.results, [branch]: result } }));
      }
    } catch {
      // error.raised already surfaced it
    }
  },
}));

onBusEvent((event) => {
  if (event.type === "check.started" || event.type === "check.finished") {
    void useChecks.getState().fetchResult(event.data.branch);
  }
  if (event.type === "check.finished") {
    void (async () => {
      const { useToasts } = await import("./toasts");
      const { branch, passed, exit_code } = event.data;
      useToasts.getState().push(
        passed
          ? { severity: "info", code: "check", message: `Checks passed on '${branch}'.` }
          : {
              severity: "warning",
              code: "check",
              message: `Checks FAILED on '${branch}'${exit_code !== null ? ` (exit ${exit_code})` : ""} — open the checks panel for the output.`,
            },
      );
    })();
  }
});
