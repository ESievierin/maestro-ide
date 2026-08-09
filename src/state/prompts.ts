import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { PromptFile } from "../types/prompts";

/** Stable empty list — selectors must never build a fresh value (see state/questions.ts). */
const EMPTY: readonly PromptFile[] = Object.freeze([]);

interface PromptsState {
  templates: readonly PromptFile[];
  loading: boolean;
  error: string | null;

  fetch: () => Promise<void>;
  save: (name: string, content: string) => Promise<boolean>;
  reset: (name: string) => Promise<boolean>;
  delete: (name: string) => Promise<boolean>;
  clearError: () => void;
}

function replace(templates: readonly PromptFile[], updated: PromptFile): PromptFile[] {
  const next = templates.filter((t) => t.name !== updated.name);
  next.push(updated);
  next.sort((a, b) => a.name.localeCompare(b.name));
  return next;
}

export const usePrompts = create<PromptsState>((set) => ({
  templates: EMPTY,
  loading: false,
  error: null,

  fetch: async () => {
    set({ loading: true });
    try {
      const templates = await invoke<PromptFile[]>("list_prompts");
      set({ templates, loading: false });
    } catch (e) {
      set({ loading: false, error: String(e) });
    }
  },

  save: async (name, content) => {
    try {
      const updated = await invoke<PromptFile>("save_prompt", { name, content });
      set((s) => ({ templates: replace(s.templates, updated) }));
      return true;
    } catch (e) {
      set({ error: String(e) });
      return false;
    }
  },

  reset: async (name) => {
    try {
      const updated = await invoke<PromptFile>("reset_prompt", { name });
      set((s) => ({ templates: replace(s.templates, updated) }));
      return true;
    } catch (e) {
      set({ error: String(e) });
      return false;
    }
  },

  delete: async (name) => {
    try {
      await invoke("delete_prompt", { name });
      set((s) => ({ templates: s.templates.filter((t) => t.name !== name) }));
      return true;
    } catch (e) {
      set({ error: String(e) });
      return false;
    }
  },

  clearError: () => set({ error: null }),
}));
