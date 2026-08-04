import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type {
  CreateWorktreeRequest,
  RemoveOutcome,
  RepoInfo,
  WorktreeInfo,
} from "../types/worktrees";
import { onBusEvent } from "./events";

/** Which panel of the selected worktree is showing. */
export type MainTab = "chat" | "diff" | "notes";

interface WorktreesState {
  repo: RepoInfo | null;
  worktrees: WorktreeInfo[];
  /** Branch selected in the UI; panels (diff, chat) key off this. */
  selected: string | null;
  /** Active panel; the attention queue navigates by setting it. */
  tab: MainTab;
  loading: boolean;
  error: string | null;

  refresh: () => Promise<void>;
  setRepo: (path: string) => Promise<boolean>;
  create: (request: CreateWorktreeRequest) => Promise<boolean>;
  remove: (branch: string, force: boolean) => Promise<RemoveOutcome | null>;
  select: (branch: string | null) => void;
  setTab: (tab: MainTab) => void;
  clearError: () => void;
}

export const useWorktrees = create<WorktreesState>((set, get) => ({
  repo: null,
  worktrees: [],
  selected: null,
  tab: "chat",
  loading: false,
  error: null,

  refresh: async () => {
    set({ loading: true });
    try {
      const repo = await invoke<RepoInfo | null>("get_workspace");
      const worktrees = repo ? await invoke<WorktreeInfo[]>("list_worktrees") : [];
      const { selected } = get();
      const stillThere = worktrees.some((w) => w.branch === selected);
      set({
        repo,
        worktrees,
        loading: false,
        selected: stillThere ? selected : null,
      });
      // Load session lists so WorktreeList badges are accurate.
      const branches = worktrees.flatMap((w) => (w.branch ? [w.branch] : []));
      const { useSessions } = await import("./sessions");
      void useSessions.getState().fetchMany(branches);
    } catch (e) {
      set({ loading: false, error: String(e) });
    }
  },

  setRepo: async (path) => {
    try {
      await invoke<RepoInfo>("set_repo", { path });
      await get().refresh();
      return true;
    } catch (e) {
      set({ error: String(e) });
      return false;
    }
  },

  create: async (request) => {
    try {
      await invoke<WorktreeInfo>("create_worktree", { request });
      await get().refresh();
      return true;
    } catch (e) {
      set({ error: String(e) });
      return false;
    }
  },

  remove: async (branch, force) => {
    try {
      const outcome = await invoke<RemoveOutcome>("remove_worktree", { branch, force });
      if (outcome.outcome === "removed") {
        await get().refresh();
      }
      return outcome;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  select: (branch) => set({ selected: branch }),
  setTab: (tab) => set({ tab }),
  clearError: () => set({ error: null }),
}));

// Keep the list in sync with core events without polling.
onBusEvent((event) => {
  if (event.type === "worktree.created" || event.type === "worktree.removed") {
    void useWorktrees.getState().refresh();
  }
});
