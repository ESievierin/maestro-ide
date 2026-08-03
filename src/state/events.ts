import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import type { BusEvent } from "../types/events";

export interface ReceivedEvent {
  event: BusEvent;
  receivedAt: number;
}

interface EventLogState {
  events: ReceivedEvent[];
  push: (event: BusEvent) => void;
  clear: () => void;
}

const MAX_EVENTS = 200;

export const useEventLog = create<EventLogState>((set) => ({
  events: [],
  push: (event) =>
    set((state) => ({
      events: [...state.events, { event, receivedAt: Date.now() }].slice(-MAX_EVENTS),
    })),
  clear: () => set({ events: [] }),
}));

type BusEventListener = (event: BusEvent) => void;

const listeners = new Set<BusEventListener>();

/** Register a callback for every core event; returns an unsubscribe function. */
export function onBusEvent(listener: BusEventListener): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

let bridgeStarted = false;

/** Subscribe the zustand store to core events forwarded over the Tauri event channel. */
export function startEventBridge(): void {
  if (bridgeStarted) return;
  bridgeStarted = true;
  void listen<BusEvent>("maestro:event", (e) => {
    useEventLog.getState().push(e.payload);
    for (const listener of listeners) {
      listener(e.payload);
    }
  });
}
