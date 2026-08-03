import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { GateParam, PendingGate } from "../types/gates";
import { onBusEvent } from "./events";

interface GatesState {
  /** Pending gates, oldest first — the dialog shows the head of the queue. */
  pending: PendingGate[];
  error: string | null;

  /** Reload from the core (dialog restore after a UI reload). */
  fetch: () => Promise<void>;
  respond: (
    gateId: string,
    allow: boolean,
    editedParams: GateParam[],
    feedback?: string,
  ) => Promise<void>;
  clearError: () => void;
}

export const useGates = create<GatesState>((set) => ({
  pending: [],
  error: null,

  fetch: async () => {
    try {
      const pending = await invoke<PendingGate[]>("list_pending_gates");
      set({ pending });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  respond: async (gateId, allow, editedParams, feedback) => {
    try {
      await invoke("respond_gate", { gateId, allow, editedParams, feedback: feedback ?? null });
      set((s) => ({ pending: s.pending.filter((g) => g.gate_id !== gateId) }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  clearError: () => set({ error: null }),
}));

onBusEvent((event) => {
  if (event.type !== "gate.pending") return;
  const gate: PendingGate = event.data;
  useGates.setState((s) =>
    s.pending.some((g) => g.gate_id === gate.gate_id) ? s : { pending: [...s.pending, gate] },
  );
});

// Gates pending in the core survive a frontend reload — pick them up at startup.
void useGates.getState().fetch();
