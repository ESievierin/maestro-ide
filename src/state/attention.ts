import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import type { AttentionItem } from "../types/attention";
import { onBusEvent } from "./events";

/** Stable empty list — selectors must never build a fresh value (see state/questions.ts). */
const EMPTY: readonly AttentionItem[] = Object.freeze([]);

const NOTIFICATIONS_KEY = "maestro.osNotifications";

interface AttentionState {
  items: readonly AttentionItem[];
  /** Whether OS notifications are enabled (persisted locally, config-gated per brief). */
  notificationsEnabled: boolean;
  error: string | null;

  fetch: () => Promise<void>;
  dismiss: (id: string) => Promise<void>;
  setNotificationsEnabled: (enabled: boolean) => Promise<void>;
  clearError: () => void;
}

export const useAttention = create<AttentionState>((set) => ({
  items: EMPTY,
  notificationsEnabled: localStorage.getItem(NOTIFICATIONS_KEY) === "true",
  error: null,

  fetch: async () => {
    try {
      const items = await invoke<AttentionItem[]>("list_attention");
      set({ items: items.length === 0 ? EMPTY : items });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  dismiss: async (id) => {
    try {
      await invoke("dismiss_attention", { id });
      set((s) => {
        const items = s.items.filter((i) => i.id !== id);
        return { items: items.length === 0 ? EMPTY : items };
      });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  setNotificationsEnabled: async (enabled) => {
    if (enabled) {
      // Ask the OS once; if the user says no, keep the toggle off rather than
      // pretending notifications work.
      let granted = await isPermissionGranted();
      if (!granted) granted = (await requestPermission()) === "granted";
      if (!granted) {
        set({ error: "The OS denied notification permission." });
        return;
      }
    }
    localStorage.setItem(NOTIFICATIONS_KEY, String(enabled));
    set({ notificationsEnabled: enabled });
  },

  clearError: () => set({ error: null }),
}));

/** Count for the header badge. */
export function selectAttentionCount(s: AttentionState): number {
  return s.items.length;
}

// The core owns the queue and announces every change; refetch instead of mirroring
// event-by-event, so the panel can never drift from the core's view.
onBusEvent((event) => {
  if (event.type === "attention.updated") {
    void useAttention.getState().fetch();
  }
});

// Notify on the events that mean "an agent is blocked", not on every queue change:
// re-notifying for the same situation would be noise.
onBusEvent((event) => {
  const { notificationsEnabled } = useAttention.getState();
  if (!notificationsEnabled) return;

  let body: string | null = null;
  switch (event.type) {
    case "gate.pending":
      body = `Approval required: ${event.data.kind} on ${event.data.branch}`;
      break;
    case "session.permission_request":
      body = event.data.title ?? `${event.data.tool} needs permission`;
      break;
    case "session.status_changed":
      if (event.data.status === "failed") body = `Session failed on ${event.data.branch}`;
      break;
    case "attention.required":
      body = event.data.message;
      break;
  }
  if (body) sendNotification({ title: "MaestroIDE", body });
});

// Pick up anything already waiting when the UI (re)loads.
void useAttention.getState().fetch();
