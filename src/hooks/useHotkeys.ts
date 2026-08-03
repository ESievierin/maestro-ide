import { useEffect } from "react";
import { useWorktrees } from "../state/worktrees";

/**
 * Fleet navigation without the mouse (T10):
 *   Alt+1…9        select the nth worktree
 *   Alt+↑ / Alt+↓  previous / next worktree
 *   Alt+C / Alt+D  chat / diff panel
 *
 * Alt is used because Ctrl+number and Ctrl+D belong to the editors and inputs the app
 * embeds. Keystrokes are ignored while typing so a prompt can contain any character.
 */
export function useHotkeys(): void {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!event.altKey || event.ctrlKey || event.metaKey) return;

      const target = event.target as HTMLElement | null;
      if (target?.isContentEditable) return;
      const tag = target?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;

      const { worktrees, selected, select, setTab } = useWorktrees.getState();
      const branches = worktrees.flatMap((w) => (w.branch ? [w.branch] : []));
      if (event.key === "c" || event.key === "C") {
        setTab("chat");
      } else if (event.key === "d" || event.key === "D") {
        setTab("diff");
      } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        if (branches.length === 0) return;
        const current = selected ? branches.indexOf(selected) : -1;
        const step = event.key === "ArrowDown" ? 1 : -1;
        // Wrap around; an unknown/absent selection starts at the first entry.
        const next = (current + step + branches.length) % branches.length;
        select(branches[current === -1 ? 0 : next]);
      } else if (/^[1-9]$/.test(event.key)) {
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
