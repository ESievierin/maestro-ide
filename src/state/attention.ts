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

/** Legacy frontend-only flag — migrated once into the real `os_notifications`
 * setting below, then never read again. */
const LEGACY_NOTIFICATIONS_KEY = "maestro.osNotifications";

/** How long to wait for more notifications before flushing a digest — long
 * enough to catch a burst of agents finishing near the same time, short
 * enough that a single item still shows up promptly. */
const DIGEST_WINDOW_MS = 4000;

interface AttentionState {
  items: readonly AttentionItem[];
  /** Whether OS notifications are enabled — backed by the `os_notifications`
   * setting (config.toml-gated per the original brief), not just this window. */
  notificationsEnabled: boolean;
  /** Whether notifications arriving close together are coalesced into one
   * "N items need you" notification instead of firing one per item. */
  digestEnabled: boolean;
  error: string | null;

  fetch: () => Promise<void>;
  fetchNotificationsEnabled: () => Promise<void>;
  fetchDigestEnabled: () => Promise<void>;
  dismiss: (id: string) => Promise<void>;
  setNotificationsEnabled: (enabled: boolean) => Promise<void>;
  setDigestEnabled: (enabled: boolean) => Promise<void>;
  clearError: () => void;
}

export const useAttention = create<AttentionState>((set) => ({
  items: EMPTY,
  notificationsEnabled: false, // hydrated from the backend below
  digestEnabled: false, // hydrated from the backend below
  error: null,

  fetch: async () => {
    try {
      const items = await invoke<AttentionItem[]>("list_attention");
      set({ items: items.length === 0 ? EMPTY : items });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  fetchNotificationsEnabled: async () => {
    try {
      const enabled = await invoke<boolean>("get_os_notifications_enabled");
      set({ notificationsEnabled: enabled });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  fetchDigestEnabled: async () => {
    try {
      const enabled = await invoke<boolean>("get_notification_digest_enabled");
      set({ digestEnabled: enabled });
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
    try {
      await invoke("set_os_notifications_enabled", { enabled });
      set({ notificationsEnabled: enabled });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  setDigestEnabled: async (enabled) => {
    try {
      await invoke("set_notification_digest_enabled", { enabled });
      set({ digestEnabled: enabled });
    } catch (e) {
      set({ error: String(e) });
    }
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

// Digest mode buffers bodies here instead of sending immediately; module-level
// since it is purely a delivery-timing concern, not UI state anything renders.
let pendingDigest: string[] = [];
let digestTimer: ReturnType<typeof setTimeout> | null = null;

function flushDigest() {
  digestTimer = null;
  if (pendingDigest.length === 0) return;
  const body =
    pendingDigest.length === 1
      ? pendingDigest[0]
      : `${pendingDigest.length} items need your attention`;
  pendingDigest = [];
  sendNotification({ title: "MaestroIDE", body });
}

// Notify on the events that mean "an agent is blocked", not on every queue change:
// re-notifying for the same situation would be noise.
onBusEvent((event) => {
  const { notificationsEnabled, digestEnabled } = useAttention.getState();
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
  if (!body) return;

  if (!digestEnabled) {
    sendNotification({ title: "MaestroIDE", body });
    return;
  }
  pendingDigest.push(body);
  if (digestTimer) clearTimeout(digestTimer);
  digestTimer = setTimeout(flushDigest, DIGEST_WINDOW_MS);
});

// Pick up anything already waiting when the UI (re)loads.
void useAttention.getState().fetch();

// One-time migration from the old frontend-only flag to the real setting,
// then hydrate for real. Runs once: the legacy key is gone after this, so
// later loads go straight to fetchNotificationsEnabled().
void (async () => {
  const legacy = localStorage.getItem(LEGACY_NOTIFICATIONS_KEY);
  if (legacy === "true") {
    try {
      await invoke("set_os_notifications_enabled", { enabled: true });
    } catch {
      // Best-effort — worst case the user re-enables it once from Settings.
    }
  }
  if (legacy !== null) localStorage.removeItem(LEGACY_NOTIFICATIONS_KEY);
  await useAttention.getState().fetchNotificationsEnabled();
  await useAttention.getState().fetchDigestEnabled();
})();
