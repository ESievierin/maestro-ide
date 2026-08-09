import { create } from "zustand";

/**
 * A one-shot signal from "somewhere in the chat transcript" to "the Diff tab
 * of this branch, please select this file" — the two views have no other
 * shared state to carry that through. `DiffViewer` consumes and clears it
 * once it has the file list to check the path against.
 */
interface DiffJumpState {
  pending: { branch: string; path: string } | null;
  request: (branch: string, path: string) => void;
  clear: () => void;
}

export const useDiffJump = create<DiffJumpState>((set) => ({
  pending: null,
  request: (branch, path) => set({ pending: { branch, path } }),
  clear: () => set({ pending: null }),
}));
