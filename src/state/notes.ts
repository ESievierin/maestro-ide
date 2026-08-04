import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { Notes } from "../types/notes";
import { onBusEvent } from "./events";

/**
 * `TASK_NOTES.md` per branch. Read on demand (panel open, Refresh, `notes.updated`, and
 * when a session on the branch finishes) — there is no watcher, so an edit made outside
 * Maestro shows up on the next read.
 *
 * zustand 5 has no snapshot memoization: every selector here returns a value already in
 * state, never a derived array or object literal, or React loops forever on "changed"
 * snapshots. See `state/questions.ts` for the same rule and its test.
 */
interface NotesState {
  byBranch: Record<string, Notes>;
  /** Branches with a read in flight, so the panel can show it without a boolean per call. */
  loading: Record<string, true>;
  error: string | null;

  fetch: (branch: string) => Promise<void>;
  refresh: (branch: string) => Promise<void>;
  clearError: () => void;
}

async function load(
  set: (fn: (state: NotesState) => Partial<NotesState>) => void,
  branch: string,
  command: "get_notes" | "refresh_notes",
): Promise<void> {
  set((s) => ({ loading: { ...s.loading, [branch]: true } }));
  try {
    const notes = await invoke<Notes>(command, { branch });
    set((s) => {
      const loading = { ...s.loading };
      delete loading[branch];
      return { byBranch: { ...s.byBranch, [branch]: notes }, loading };
    });
  } catch (e) {
    set((s) => {
      const loading = { ...s.loading };
      delete loading[branch];
      return { loading, error: String(e) };
    });
  }
}

export const useNotes = create<NotesState>((set) => ({
  byBranch: {},
  loading: {},
  error: null,

  fetch: (branch) => load(set, branch, "get_notes"),
  refresh: (branch) => load(set, branch, "refresh_notes"),
  clearError: () => set({ error: null }),
}));

onBusEvent((event) => {
  switch (event.type) {
    case "notes.updated": {
      void useNotes.getState().refresh(event.data.branch);
      break;
    }
    case "session.status_changed": {
      // A finished session is the moment notes are most likely to have changed — but only
      // re-read a branch the user has already looked at, so this stays free otherwise.
      const { branch, status } = event.data;
      if (status !== "done" && status !== "failed" && status !== "cancelled") break;
      if (useNotes.getState().byBranch[branch]) {
        void useNotes.getState().refresh(branch);
      }
      break;
    }
  }
});
