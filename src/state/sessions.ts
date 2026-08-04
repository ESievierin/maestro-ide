import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type {
  CommandInfo,
  DialogAnswer,
  ModelOption,
  Session,
  TranscriptItem,
  UserDialog,
} from "../types/sessions";
import { ACTIVE_STATUSES, FALLBACK_MODELS, isTerminalStatus } from "../types/sessions";
import { onBusEvent } from "./events";

const MODELS_CACHE_KEY = "maestro.models";

/** True once the CLI has answered this app run; until then the list is a cached guess. */
let modelsRefreshed = false;

function loadCachedModels(): ModelOption[] {
  try {
    const raw = localStorage.getItem(MODELS_CACHE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as ModelOption[];
      if (Array.isArray(parsed) && parsed.length > 0) return parsed;
    }
  } catch {
    // Fall through to the static list.
  }
  return FALLBACK_MODELS;
}

export interface SpawnSessionInput {
  branch: string;
  prompt: string;
  session_type?: string;
  model?: string;
  effort?: string;
  permission_mode?: string;
  resume_from?: string;
}

interface SessionsState {
  /** Sessions per branch, as loaded from the store. */
  byBranch: Record<string, Session[]>;
  /** Live transcripts per session id, built from bus events (not persisted). */
  transcripts: Record<string, TranscriptItem[]>;
  /** Slash commands supported by each live session (for autocomplete). */
  commands: Record<string, CommandInfo[]>;
  /** Model options as reported by the CLI (cached across app runs). */
  models: ModelOption[];
  /** Ask the CLI for the authoritative list; safe to call repeatedly. */
  refreshModels: () => Promise<void>;
  /**
   * Dialogs the agent is blocked on, per session id. One at a time per session: the CLI
   * raises the next only after this one is answered.
   */
  dialogs: Record<string, UserDialog>;
  error: string | null;

  fetch: (branch: string) => Promise<void>;
  fetchMany: (branches: string[]) => Promise<void>;
  spawn: (input: SpawnSessionInput) => Promise<Session | null>;
  send: (sessionId: string, prompt: string) => Promise<void>;
  interrupt: (sessionId: string) => Promise<void>;
  close: (sessionId: string) => Promise<void>;
  remove: (sessionId: string, branch: string) => Promise<boolean>;
  respondPermission: (sessionId: string, requestId: string, allow: boolean) => Promise<void>;
  /** Answer (or dismiss, with `null`) the dialog the agent is waiting on. */
  respondDialog: (sessionId: string, answer: DialogAnswer | null) => Promise<void>;
  setModel: (sessionId: string, model: string) => Promise<void>;
  setEffort: (sessionId: string, effort: string) => Promise<void>;
  setPermissionMode: (sessionId: string, mode: string) => Promise<void>;
  clearError: () => void;
}

function appendTranscript(
  transcripts: Record<string, TranscriptItem[]>,
  sessionId: string,
  item: TranscriptItem,
): Record<string, TranscriptItem[]> {
  const existing = transcripts[sessionId] ?? [];
  // Merge consecutive text deltas so the transcript stays small.
  if (item.kind === "text") {
    const last = existing[existing.length - 1];
    if (last && last.kind === "text") {
      return {
        ...transcripts,
        [sessionId]: [...existing.slice(0, -1), { kind: "text", text: last.text + item.text }],
      };
    }
  }
  return { ...transcripts, [sessionId]: [...existing, item] };
}

export const useSessions = create<SessionsState>((set, get) => ({
  byBranch: {},
  transcripts: {},
  commands: {},
  models: loadCachedModels(),
  dialogs: {},
  error: null,

  refreshModels: async () => {
    if (modelsRefreshed) return;
    modelsRefreshed = true;
    try {
      // Answers arrive as a session.models event; this call only asks.
      await invoke("refresh_models");
    } catch (e) {
      // A stale cached list is better than an empty selector — just report it.
      modelsRefreshed = false;
      set({ error: String(e) });
    }
  },

  fetch: async (branch) => {
    try {
      const sessions = await invoke<Session[]>("list_sessions", { branch });
      set((s) => ({ byBranch: { ...s.byBranch, [branch]: sessions } }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  fetchMany: async (branches) => {
    await Promise.all(branches.map((b) => get().fetch(b)));
  },

  spawn: async (input) => {
    try {
      const session = await invoke<Session>("spawn_session", { args: input });
      if (input.prompt.trim().length > 0) {
        set((s) => ({
          transcripts: appendTranscript(s.transcripts, session.id, {
            kind: "user",
            text: input.prompt,
          }),
        }));
      }
      await get().fetch(input.branch);
      return session;
    } catch (e) {
      set({ error: String(e) });
      return null;
    }
  },

  send: async (sessionId, prompt) => {
    try {
      await invoke("send_prompt", { sessionId, prompt });
      set((s) => ({
        transcripts: appendTranscript(s.transcripts, sessionId, { kind: "user", text: prompt }),
      }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  interrupt: async (sessionId) => {
    try {
      await invoke("interrupt_session", { sessionId });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  close: async (sessionId) => {
    try {
      await invoke("close_session", { sessionId });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  remove: async (sessionId, branch) => {
    try {
      await invoke("delete_session", { sessionId });
      set((s) => {
        const transcripts = { ...s.transcripts };
        const commands = { ...s.commands };
        delete transcripts[sessionId];
        delete commands[sessionId];
        return { transcripts, commands };
      });
      await get().fetch(branch);
      return true;
    } catch (e) {
      set({ error: String(e) });
      return false;
    }
  },

  respondPermission: async (sessionId, requestId, allow) => {
    try {
      await invoke("respond_permission", { requestId, allow });
      set((s) => {
        const items = (s.transcripts[sessionId] ?? []).map((item) =>
          item.kind === "permission_request" && item.requestId === requestId
            ? { ...item, resolved: allow ? ("allowed" as const) : ("denied" as const) }
            : item,
        );
        return { transcripts: { ...s.transcripts, [sessionId]: items } };
      });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  respondDialog: async (sessionId, answer) => {
    const dialog = get().dialogs[sessionId];
    if (!dialog) return;
    try {
      // `null` result reaches the core as "dismissed"; the sidecar then tells the agent
      // the question went unanswered instead of leaving it to a CLI default.
      await invoke("respond_user_dialog", { requestId: dialog.requestId, result: answer });
    } catch (e) {
      set({ error: String(e) });
      return;
    }
    set((s) => {
      const dialogs = { ...s.dialogs };
      delete dialogs[sessionId];
      return {
        dialogs,
        transcripts: appendTranscript(s.transcripts, sessionId, summarize(answer)),
      };
    });
  },

  setModel: async (sessionId, model) => {
    try {
      await invoke("set_session_model", { sessionId, model });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  setEffort: async (sessionId, effort) => {
    try {
      await invoke("set_session_effort", { sessionId, effort });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  setPermissionMode: async (sessionId, mode) => {
    try {
      await invoke("set_session_permission_mode", { sessionId, mode });
    } catch (e) {
      // Refused when it would create a second writer on the branch — show the reason.
      set({ error: String(e) });
    }
  },

  clearError: () => set({ error: null }),
}));

/** Transcript entry for an answered dialog, built from the answer alone. */
function summarize(answer: DialogAnswer | null): TranscriptItem {
  if (!answer) return { kind: "dialog", title: "Question dismissed", lines: [] };
  if (answer.feedback?.trim()) {
    return { kind: "dialog", title: "Asked the agent to clarify", lines: [answer.feedback.trim()] };
  }
  const lines = Object.entries(answer.answers ?? {}).map(([q, a]) => `${q} → ${a}`);
  return { kind: "dialog", title: "Answered", lines };
}

/** Count of live sessions per branch — used for WorktreeList badges. */
export function activeSessionCount(sessions: Session[] | undefined): number {
  if (!sessions) return 0;
  return sessions.filter((s) => ACTIVE_STATUSES.includes(s.status)).length;
}

onBusEvent((event) => {
  const state = useSessions.getState();
  switch (event.type) {
    case "session.status_changed": {
      const { branch, session_id, status } = event.data;
      useSessions.setState((s) => {
        // A finished session can no longer answer a dialog; drop it or the modal sticks.
        const dialogs =
          s.dialogs[session_id] && isTerminalStatus(status) ? { ...s.dialogs } : s.dialogs;
        if (dialogs !== s.dialogs) delete dialogs[session_id];
        const sessions = s.byBranch[branch];
        if (!sessions) return { dialogs };
        return {
          dialogs,
          byBranch: {
            ...s.byBranch,
            [branch]: sessions.map((sess) => (sess.id === session_id ? { ...sess, status } : sess)),
          },
          transcripts: appendTranscript(s.transcripts, session_id, { kind: "status", status }),
        };
      });
      // A session we don't know yet (spawned elsewhere): reload the branch.
      if (!state.byBranch[branch]?.some((sess) => sess.id === session_id)) {
        void state.fetch(branch);
      }
      break;
    }
    case "session.stream_delta": {
      const { session_id, text } = event.data;
      useSessions.setState((s) => ({
        transcripts: appendTranscript(s.transcripts, session_id, { kind: "text", text }),
      }));
      break;
    }
    case "session.tool_use": {
      const { session_id, name, summary } = event.data;
      useSessions.setState((s) => ({
        transcripts: appendTranscript(s.transcripts, session_id, {
          kind: "tool_use",
          name,
          summary,
        }),
      }));
      break;
    }
    case "session.permission_request": {
      const { session_id, request_id, tool, args, title } = event.data;
      useSessions.setState((s) => ({
        transcripts: appendTranscript(s.transcripts, session_id, {
          kind: "permission_request",
          requestId: request_id,
          tool,
          args,
          title,
          resolved: "pending",
        }),
      }));
      break;
    }
    case "session.user_dialog": {
      const { session_id, request_id, dialog_kind, payload } = event.data;
      useSessions.setState((s) => ({
        dialogs: {
          ...s.dialogs,
          [session_id]: {
            sessionId: session_id,
            requestId: request_id,
            dialogKind: dialog_kind,
            payload,
          },
        },
      }));
      break;
    }
    case "session.user_dialog_resolved": {
      // Answered here or elsewhere (another window, a timeout): the modal must go.
      const { session_id, request_id } = event.data;
      useSessions.setState((s) => {
        if (s.dialogs[session_id]?.requestId !== request_id) return {};
        const dialogs = { ...s.dialogs };
        delete dialogs[session_id];
        return { dialogs };
      });
      break;
    }
    case "session.settings_changed": {
      const { session_id, model, effort, permission_mode } = event.data;
      const parts = [
        model !== null && `model: ${model || "default"}`,
        effort !== null && `effort: ${effort || "default"}`,
        permission_mode !== null && `permissions: ${permission_mode}`,
      ].filter(Boolean) as string[];
      useSessions.setState((s) => {
        const byBranch = { ...s.byBranch };
        for (const [branch, list] of Object.entries(byBranch)) {
          if (!list.some((sess) => sess.id === session_id)) continue;
          byBranch[branch] = list.map((sess) =>
            sess.id === session_id
              ? {
                  ...sess,
                  model: model ?? sess.model,
                  effort: effort ?? sess.effort,
                  permission_mode: permission_mode ?? sess.permission_mode,
                }
              : sess,
          );
        }
        return {
          byBranch,
          transcripts: appendTranscript(s.transcripts, session_id, {
            kind: "settings",
            text: parts.join(" · "),
          }),
        };
      });
      break;
    }
    case "session.commands": {
      const { session_id, commands } = event.data;
      useSessions.setState((s) => ({
        commands: { ...s.commands, [session_id]: commands },
      }));
      break;
    }
    case "session.models": {
      // An empty session_id means the global list from refresh_models.
      const { models } = event.data;
      if (models.length > 0) {
        useSessions.setState({ models });
        try {
          localStorage.setItem(MODELS_CACHE_KEY, JSON.stringify(models));
        } catch {
          // Cache is best-effort.
        }
      }
      break;
    }
  }
});
