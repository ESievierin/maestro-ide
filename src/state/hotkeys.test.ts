// @vitest-environment jsdom
// hotkeys.ts reads localStorage at import time (initial overrides) — the
// default "node" environment for this directory has no such global.
import { beforeEach, describe, expect, it } from "vitest";
import {
  comboFromEvent,
  eventMatchesCombo,
  HOTKEY_ACTIONS,
  isBareModifierKey,
  useHotkeyBindings,
} from "./hotkeys";

function keyEvent(overrides: Partial<KeyboardEvent> & { key: string }) {
  return {
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    metaKey: false,
    ...overrides,
  };
}

describe("comboFromEvent", () => {
  it("uppercases a single-character key with no modifiers", () => {
    expect(comboFromEvent(keyEvent({ key: "c" }))).toBe("C");
  });

  it("prefixes Alt for an alt-held letter", () => {
    expect(comboFromEvent(keyEvent({ key: "c", altKey: true }))).toBe("Alt+C");
  });

  it("merges ctrlKey and metaKey into the same Ctrl prefix", () => {
    expect(comboFromEvent(keyEvent({ key: "k", ctrlKey: true }))).toBe("Ctrl+K");
    expect(comboFromEvent(keyEvent({ key: "k", metaKey: true }))).toBe("Ctrl+K");
  });

  it("stacks modifiers in Ctrl, Alt, Shift order", () => {
    expect(
      comboFromEvent(keyEvent({ key: "x", ctrlKey: true, altKey: true, shiftKey: true })),
    ).toBe("Ctrl+Alt+Shift+X");
  });

  it("keeps a named key (e.g. ArrowUp) as-is instead of uppercasing it", () => {
    expect(comboFromEvent(keyEvent({ key: "ArrowUp", altKey: true }))).toBe("Alt+ArrowUp");
  });
});

describe("eventMatchesCombo", () => {
  it("matches when modifiers and key line up exactly", () => {
    expect(eventMatchesCombo(keyEvent({ key: "d", altKey: true }), "Alt+D")).toBe(true);
  });

  it("does not match a plain keypress against a modified combo", () => {
    expect(eventMatchesCombo(keyEvent({ key: "d" }), "Alt+D")).toBe(false);
  });

  it("is false for an empty combo string", () => {
    expect(eventMatchesCombo(keyEvent({ key: "d", altKey: true }), "")).toBe(false);
  });
});

describe("isBareModifierKey", () => {
  it("is true for the four modifier key names", () => {
    expect(isBareModifierKey("Control")).toBe(true);
    expect(isBareModifierKey("Alt")).toBe(true);
    expect(isBareModifierKey("Shift")).toBe(true);
    expect(isBareModifierKey("Meta")).toBe(true);
  });

  it("is false for an ordinary key", () => {
    expect(isBareModifierKey("c")).toBe(false);
    expect(isBareModifierKey("ArrowUp")).toBe(false);
  });
});

describe("useHotkeyBindings", () => {
  beforeEach(() => {
    localStorage.clear();
    useHotkeyBindings.setState({ overrides: {} });
  });

  it("falls back to each action's default combo with no overrides", () => {
    for (const action of HOTKEY_ACTIONS) {
      expect(useHotkeyBindings.getState().comboFor(action.id)).toBe(action.defaultCombo);
    }
  });

  it("setBinding overrides one action and persists it to localStorage", () => {
    useHotkeyBindings.getState().setBinding("tab-chat", "Alt+Q");
    expect(useHotkeyBindings.getState().comboFor("tab-chat")).toBe("Alt+Q");
    expect(JSON.parse(localStorage.getItem("maestro.hotkeyOverrides") ?? "{}")).toEqual({
      "tab-chat": "Alt+Q",
    });
  });

  it("resetOne clears only the named action's override", () => {
    useHotkeyBindings.getState().setBinding("tab-chat", "Alt+Q");
    useHotkeyBindings.getState().setBinding("tab-diff", "Alt+W");
    useHotkeyBindings.getState().resetOne("tab-chat");
    expect(useHotkeyBindings.getState().comboFor("tab-chat")).toBe("Alt+C");
    expect(useHotkeyBindings.getState().comboFor("tab-diff")).toBe("Alt+W");
  });

  it("resetAll clears every override and the localStorage key", () => {
    useHotkeyBindings.getState().setBinding("tab-chat", "Alt+Q");
    useHotkeyBindings.getState().resetAll();
    expect(useHotkeyBindings.getState().overrides).toEqual({});
    expect(localStorage.getItem("maestro.hotkeyOverrides")).toBeNull();
  });

  it("actionBoundTo finds the action currently using a combo, default or overridden", () => {
    expect(useHotkeyBindings.getState().actionBoundTo("Alt+C")).toBe("tab-chat");
    useHotkeyBindings.getState().setBinding("tab-diff", "Alt+C");
    // tab-chat still nominally defaults to Alt+C, but tab-diff's explicit
    // override collides with it — actionBoundTo just reports the first match,
    // which is what the rebind UI uses to flag the conflict either way.
    expect(useHotkeyBindings.getState().actionBoundTo("Alt+C")).not.toBeNull();
  });

  it("actionBoundTo returns null for a combo nothing uses", () => {
    expect(useHotkeyBindings.getState().actionBoundTo("Ctrl+Alt+Shift+Z")).toBeNull();
  });
});
