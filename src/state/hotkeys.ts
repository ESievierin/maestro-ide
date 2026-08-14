import { create } from "zustand";

export interface HotkeyAction {
  id: string;
  label: string;
  defaultCombo: string;
}

/**
 * Every globally-rebindable shortcut. Alt+1…9 (select the nth worktree) is
 * deliberately left out — it is positional (slot N, not a single named
 * action) and in practice never the one that conflicts with an OS/IDE
 * shortcut, so there is no real case for remapping it.
 */
export const HOTKEY_ACTIONS: readonly HotkeyAction[] = [
  { id: "command-palette", label: "Command palette", defaultCombo: "Ctrl+K" },
  { id: "tab-chat", label: "Chat tab", defaultCombo: "Alt+C" },
  { id: "tab-diff", label: "Diff tab", defaultCombo: "Alt+D" },
  { id: "tab-notes", label: "Notes tab", defaultCombo: "Alt+N" },
  { id: "worktree-prev", label: "Previous worktree", defaultCombo: "Alt+ArrowUp" },
  { id: "worktree-next", label: "Next worktree", defaultCombo: "Alt+ArrowDown" },
  { id: "needs-you", label: "Needs-you drawer", defaultCombo: "Alt+A" },
];

const STORAGE_KEY = "maestro.hotkeyOverrides";
const MODIFIER_KEYS = new Set(["Control", "Alt", "Shift", "Meta"]);

function loadOverrides(): Record<string, string> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw);
    return typeof parsed === "object" && parsed !== null ? parsed : {};
  } catch {
    return {};
  }
}

function persist(overrides: Record<string, string>) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(overrides));
}

/** Canonical combo string for a keydown event — e.g. "Ctrl+Alt+ArrowUp". Both
 * `ctrlKey` and `metaKey` map to "Ctrl" so the same binding works cross-platform. */
export function comboFromEvent(
  e: Pick<KeyboardEvent, "key" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">,
): string {
  const parts: string[] = [];
  if (e.ctrlKey || e.metaKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  parts.push(e.key.length === 1 ? e.key.toUpperCase() : e.key);
  return parts.join("+");
}

/** Whether a keydown event's modifiers+key exactly match a stored combo string. */
export function eventMatchesCombo(
  e: Pick<KeyboardEvent, "key" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">,
  combo: string,
): boolean {
  return !!combo && comboFromEvent(e) === combo;
}

/** True for a bare modifier press (e.g. just tapping Alt) — never a usable combo on its own. */
export function isBareModifierKey(key: string): boolean {
  return MODIFIER_KEYS.has(key);
}

interface HotkeyBindingsState {
  overrides: Record<string, string>;
  comboFor: (id: string) => string;
  /** The action id currently bound to `combo`, if any (for conflict detection). */
  actionBoundTo: (combo: string) => string | null;
  setBinding: (id: string, combo: string) => void;
  resetOne: (id: string) => void;
  resetAll: () => void;
}

export const useHotkeyBindings = create<HotkeyBindingsState>((set, get) => ({
  overrides: loadOverrides(),

  comboFor: (id) => {
    const override = get().overrides[id];
    if (override !== undefined) return override;
    return HOTKEY_ACTIONS.find((a) => a.id === id)?.defaultCombo ?? "";
  },

  actionBoundTo: (combo) => {
    const { comboFor } = get();
    return HOTKEY_ACTIONS.find((a) => comboFor(a.id) === combo)?.id ?? null;
  },

  setBinding: (id, combo) =>
    set((s) => {
      const overrides = { ...s.overrides, [id]: combo };
      persist(overrides);
      return { overrides };
    }),

  resetOne: (id) =>
    set((s) => {
      if (!(id in s.overrides)) return s;
      const overrides = { ...s.overrides };
      delete overrides[id];
      persist(overrides);
      return { overrides };
    }),

  resetAll: () => {
    localStorage.removeItem(STORAGE_KEY);
    set({ overrides: {} });
  },
}));
