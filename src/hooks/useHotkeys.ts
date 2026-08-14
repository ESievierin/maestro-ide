import { useEffect } from "react";
import { eventMatchesCombo, useHotkeyBindings } from "../state/hotkeys";
import { useUI } from "../state/ui";
import { useWorktrees } from "../state/worktrees";

/**
 * Fleet navigation without the mouse (T10):
 *   Alt+1…9        select the nth worktree
 *   Alt+↑ / Alt+↓  previous / next worktree
 *   Alt+C / Alt+D / Alt+N  chat / diff / notes panel
 *   Alt+A          toggle the "Needs you" drawer
 *
 * Everything but the digit shortcuts is rebindable (Settings → Keyboard
 * shortcuts, see `state/hotkeys.ts`) — the defaults above are what a fresh
 * install starts with. Keystrokes are ignored while typing so a prompt can
 * contain any character.
 */
export function useHotkeys(): void {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target?.isContentEditable) return;
      const tag = target?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;

      const { worktrees, selected, select, setTab } = useWorktrees.getState();
      const branches = worktrees.flatMap((w) => (w.branch ? [w.branch] : []));
      const comboFor = useHotkeyBindings.getState().comboFor;

      if (eventMatchesCombo(event, comboFor("tab-chat"))) {
        setTab("chat");
      } else if (eventMatchesCombo(event, comboFor("tab-diff"))) {
        setTab("diff");
      } else if (eventMatchesCombo(event, comboFor("tab-notes"))) {
        setTab("notes");
      } else if (
        eventMatchesCombo(event, comboFor("worktree-prev")) ||
        eventMatchesCombo(event, comboFor("worktree-next"))
      ) {
        if (branches.length === 0) return;
        const step = eventMatchesCombo(event, comboFor("worktree-next")) ? 1 : -1;
        const current = selected ? branches.indexOf(selected) : -1;
        // Wrap around; an unknown/absent selection starts at the first entry.
        const next = (current + step + branches.length) % branches.length;
        select(branches[current === -1 ? 0 : next]);
      } else if (eventMatchesCombo(event, comboFor("needs-you"))) {
        useUI.getState().setAttentionOpen(!useUI.getState().attentionOpen);
      } else if (event.altKey && !event.ctrlKey && !event.metaKey && /^[1-9]$/.test(event.key)) {
        const index = Number(event.key) - 1;
        if (index >= branches.length) return;
        select(branches[index]);
      } else {
        return;
      }
      event.preventDefault();
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);
}
