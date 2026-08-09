import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type {
  CreateWorktreeRequest,
  MergeOutcome,
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
  merge: (sourceBranch: string, targetBranch: string) => Promise<MergeOutcome | null>;
  /** Merge the branch's (freshly fetched) base into it. */
  sync: (branch: string) => Promise<MergeOutcome | null>;
  setPinned: (branch: string, pinned: boolean) => Promise<void>;
  select: (branch: string | null) => void;
  setTab: (tab: MainTab) => void;
  clearError: () => void;
}

/** Where the last-selected worktree/tab live between app runs. */
const SELECTED_KEY = "maestro.selectedBranch";
const TAB_KEY = "maestro.tab";

function loadTab(): MainTab {
  const saved = localStorage.getItem(TAB_KEY);
  return saved === "diff" || saved === "notes" ? saved : "chat";
}

export const useWorktrees = create<WorktreesState>((set, get) => ({
  repo: null,
  worktrees: [],
  selected: null,
  tab: loadTab(),
  loading: false,
  error: null,

  refresh: async () => {
    set({ loading: true });
    try {
      const repo = await invoke<RepoInfo | null>("get_workspace");
      const worktrees = repo ? await invoke<WorktreeInfo[]>("list_worktrees") : [];
      // Restore last session's selection on the first load; keep the user's
      // current one afterwards (unless its worktree vanished).
      const current = get().selected ?? localStorage.getItem(SELECTED_KEY);
      const stillThere = worktrees.some((w) => w.branch === current);
      set({
        repo,
        worktrees,
        loading: false,
        selected: stillThere ? current : null,
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

  merge: async (sourceBranch, targetBranch) => {
    try {
      const outcome = await invoke<MergeOutcome>("merge_worktree", {
        sourceBranch,
        targetBranch,
      });
      if (outcome.merged) {
        await get().refresh();
      }
      return outcome;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  sync: async (branch) => {
    try {
      const outcome = await invoke<MergeOutcome>("sync_worktree", { branch });
      // A conflicted sync leaves the worktree dirty — refresh either way so the
      // sidebar's dirty/ahead/behind badges tell the truth.
      await get().refresh();
      return outcome;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  setPinned: async (branch, pinned) => {
    try {
      await invoke("set_worktree_pinned", { branch, pinned });
      await get().refresh();
    } catch (e) {
      set({ error: String(e) });
    }
  },

  select: (branch) => {
    if (branch) localStorage.setItem(SELECTED_KEY, branch);
    else localStorage.removeItem(SELECTED_KEY);
    set({ selected: branch });
  },
  setTab: (tab) => {
    localStorage.setItem(TAB_KEY, tab);
    set({ tab });
  },
  clearError: () => set({ error: null }),
}));

// The dirty/ahead/behind badges reflect on-disk git state that also changes
// outside the app (Rider commits, terminal pushes). A slow poll keeps them
// truthful; only while the window is focused, so a backgrounded app stays idle.
setInterval(() => {
  if (document.hasFocus() && useWorktrees.getState().repo) {
    void useWorktrees.getState().refresh();
  }
}, 30_000);

// Keep the list in sync with core events without polling.
onBusEvent((event) => {
  if (
    event.type === "worktree.created" ||
    event.type === "worktree.removed" ||
    event.type === "worktree.merged"
  ) {
    void useWorktrees
      .getState()
      .refresh()
      .then(async () => {
        if (event.type !== "worktree.merged") return;
        // Something landed in `target`; siblings based on it are now behind.
        const { source, target } = event.data;
        const behind = useWorktrees
          .getState()
          .worktrees.filter(
            (w) =>
              w.branch &&
              w.branch !== source &&
              w.branch !== target &&
              (w.base_branch === target || w.base_branch?.endsWith(`/${target}`)),
          );
        if (behind.length === 0) return;
        const { useToasts } = await import("./toasts");
        useToasts.getState().push({
          severity: "info",
          code: "sync-hint",
          message: `${behind.length} worktree${behind.length > 1 ? "s are" : " is"} based on '${target}' — use the sync action (↓) to bring them up to date.`,
        });
      });
  }
});
