import { create } from "zustand";

/** Dialogs that can be opened from anywhere (header buttons, command palette).
 * The worktree-scoped ones ("snapshots"…"merge") act on the selected worktree. */
export type UIDialog =
  | "snapshots"
  | "checks"
  | "push"
  | "log"
  | "merge"
  | "prompts"
  | "hotkeys"
  | "create"
  | "daemon"
  | "createpr"
  | "replies"
  | "settings"
  | "search-sessions";

interface UIState {
  dialog: UIDialog | null;
  paletteOpen: boolean;
  eventLogOpen: boolean;

  openDialog: (dialog: UIDialog) => void;
  closeDialog: () => void;
  setPalette: (open: boolean) => void;
  toggleEventLog: () => void;
}

export const useUI = create<UIState>((set) => ({
  dialog: null,
  paletteOpen: false,
  eventLogOpen: false,

  openDialog: (dialog) => set({ dialog, paletteOpen: false }),
  closeDialog: () => set({ dialog: null }),
  setPalette: (open) => set({ paletteOpen: open }),
  toggleEventLog: () => set((s) => ({ eventLogOpen: !s.eventLogOpen, paletteOpen: false })),
}));
