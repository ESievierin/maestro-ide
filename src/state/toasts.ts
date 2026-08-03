import { create } from "zustand";
import type { Severity } from "../types/events";
import { onBusEvent } from "./events";

/** Stable empty list — selectors must never build a fresh value (see state/questions.ts). */
const EMPTY: readonly Toast[] = Object.freeze([]);

/** How long a toast stays up; errors stick until dismissed. */
const AUTO_DISMISS_MS: Record<Severity, number | null> = {
  info: 4000,
  warning: 8000,
  error: null,
  critical: null,
};

export interface Toast {
  id: number;
  severity: Severity;
  code: string;
  message: string;
}

interface ToastsState {
  toasts: readonly Toast[];
  push: (toast: Omit<Toast, "id">) => void;
  dismiss: (id: number) => void;
}

let nextId = 1;

export const useToasts = create<ToastsState>((set) => ({
  toasts: EMPTY,

  push: (toast) => {
    const id = nextId++;
    set((s) => ({ toasts: [...s.toasts, { ...toast, id }] }));
    const ttl = AUTO_DISMISS_MS[toast.severity];
    if (ttl !== null) {
      setTimeout(() => useToasts.getState().dismiss(id), ttl);
    }
  },

  dismiss: (id) =>
    set((s) => {
      const toasts = s.toasts.filter((t) => t.id !== id);
      return { toasts: toasts.length === 0 ? EMPTY : toasts };
    }),
}));

// Every typed core error already travels the bus as error.raised; surface them instead
// of leaving failures visible only in the log panel.
onBusEvent((event) => {
  if (event.type !== "error.raised") return;
  const { severity, code, message } = event.data;
  useToasts.getState().push({ severity, code, message });
});
