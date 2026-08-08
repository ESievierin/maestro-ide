import { useEffect } from "react";

/**
 * Close a dialog on Escape — every dismissible overlay behaves the same way.
 * Skips the shortcut while an autocomplete-style popup inside the dialog wants
 * Escape for itself only when that popup stops propagation (SelectMenu does).
 */
export function useEscapeToClose(onClose: () => void) {
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !e.defaultPrevented) {
        onClose();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);
}
