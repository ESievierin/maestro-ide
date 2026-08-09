import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type {
  AgentInfo,
  Attachment,
  CommandInfo,
  DialogAnswer,
  McpServerInfo,
  ModelOption,
  RateLimitInfo,
  Session,
  SessionUsage,
  TodoItem,
  ToolChild,
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
  thinking?: string;
  resume_from?: string;
}

interface SessionsState {
  /** Sessions per branch, as loaded from the store. */
  byBranch: Record<string, Session[]>;
  /**
   * Transcripts per session id: built live from bus events for a session running in
   * this process, or hydrated once from the backend via `loadTranscript` for one that
   * isn't (a restart, or a session from before the app was closed).
   */
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
  /** Latest checklist per session (TodoWrite replaces it wholesale). */
  todos: Record<string, TodoItem[]>;
  /** Subagent profiles per session, as reported by the CLI. */
  agents: Record<string, AgentInfo[]>;
  /** MCP servers per session and their connection state. */
  mcpServers: Record<string, McpServerInfo[]>;
  /** Cost and context pressure per session, accumulated from `session.usage`. */
  usage: Record<string, SessionUsage>;
  /** Account-wide rate-limit state; null until the CLI reports one. */
  rateLimit: RateLimitInfo | null;
  error: string | null;

  fetch: (branch: string) => Promise<void>;
  fetchMany: (branches: string[]) => Promise<void>;
  /** Hydrate a session's transcript from the backend, once, if nothing is in memory yet. */
  loadTranscript: (sessionId: string) => Promise<void>;
  /**
   * Prepend `items` in front of whatever a session already has — used to carry a
   * resumed session's prior history into the new one it continues, so the chat view
   * reads as one continuous conversation instead of restarting empty. Prepending
   * (never overwriting) is what makes this safe against the new session's own
   * events arriving before this runs.
   */
  seedTranscript: (sessionId: string, items: TranscriptItem[]) => void;
  spawn: (input: SpawnSessionInput) => Promise<Session | null>;
  send: (sessionId: string, prompt: string, attachments?: Attachment[]) => Promise<void>;
  interrupt: (sessionId: string) => Promise<void>;
  close: (sessionId: string) => Promise<void>;
  remove: (sessionId: string, branch: string) => Promise<boolean>;
  /** Delete every terminal (done/failed/cancelled) session of `branch` in one go.
   * Returns how many were actually removed. */
  removeAllFinished: (branch: string) => Promise<number>;
  respondPermission: (sessionId: string, requestId: string, allow: boolean) => Promise<void>;
  /** Answer (or dismiss, with `null`) the dialog the agent is waiting on. */
  respondDialog: (sessionId: string, answer: DialogAnswer | null) => Promise<void>;
  setModel: (sessionId: string, model: string) => Promise<void>;
  setEffort: (sessionId: string, effort: string) => Promise<void>;
  setPermissionMode: (sessionId: string, mode: string) => Promise<void>;
  setThinking: (sessionId: string, thinking: string) => Promise<void>;
  /** `reconnect` | `enable` | `disable` one MCP server of a live session. */
  mcpAction: (sessionId: string, server: string, action: string) => Promise<void>;
  clearError: () => void;
}

/**
 * Replace the tool entry with this id, wherever it sits (top level or nested under a
 * Task call). Returns the same array when nothing matched, so React sees no change.
 */
function patchTool(
  items: TranscriptItem[],
  toolUseId: string,
  patch: (item: Extract<TranscriptItem, { kind: "tool_use" }>) => TranscriptItem,
): TranscriptItem[] {
  // Newest first: a repeated tool_use id would only ever mean the latest call.
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item.kind === "tool_use" && item.id === toolUseId) {
      const next = items.slice();
      next[i] = patch(item);
      return next;
    }
  }
  return items;
}

/** Append subagent activity to the tool call that spawned it. */
function appendChild(
  transcripts: Record<string, TranscriptItem[]>,
  sessionId: string,
  parentToolUseId: string,
  child: ToolChild,
): Record<string, TranscriptItem[]> {
  const items = transcripts[sessionId] ?? [];
  const patched = patchTool(items, parentToolUseId, (tool) => {
    const last = tool.children[tool.children.length - 1];
    // Merge consecutive text/thinking so a streamed answer stays one block.
    if (last && last.kind === child.kind && child.kind !== "tool_use" && last.kind !== "tool_use") {
      return {
        ...tool,
        children: [
          ...tool.children.slice(0, -1),
          { kind: child.kind, text: last.text + child.text },
        ],
      };
    }
    return { ...tool, children: [...tool.children, child] };
  });
  if (patched === items) return transcripts;
  return { ...transcripts, [sessionId]: patched };
}

function appendTranscript(
  transcripts: Record<string, TranscriptItem[]>,
  sessionId: string,
  item: TranscriptItem,
): Record<string, TranscriptItem[]> {
  const existing = transcripts[sessionId] ?? [];
  // Merge consecutive text (and thinking) deltas so the transcript stays small.
  if (item.kind === "text" || item.kind === "thinking") {
    const last = existing[existing.length - 1];
    if (last && last.kind === item.kind) {
      return {
        ...transcripts,
        [sessionId]: [...existing.slice(0, -1), { kind: item.kind, text: last.text + item.text }],
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
  todos: {},
  agents: {},
  mcpServers: {},
  usage: {},
  rateLimit: null,
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

  loadTranscript: async (sessionId) => {
    if (get().transcripts[sessionId] !== undefined) return;
    try {
      const items = await invoke<TranscriptItem[] | null>("get_session_transcript", {
        sessionId,
      });
      if (!items) return;
      // A live session may have started streaming while this fetch was in flight —
      // never overwrite something newer with a stale disk copy.
      if (get().transcripts[sessionId] !== undefined) return;
      set((s) => ({ transcripts: { ...s.transcripts, [sessionId]: items } }));
    } catch (e) {
      set({ error: String(e) });
    }
  },

  seedTranscript: (sessionId, items) => {
    if (items.length === 0) return;
    set((s) => ({
      transcripts: {
        ...s.transcripts,
        [sessionId]: [...items, ...(s.transcripts[sessionId] ?? [])],
      },
    }));
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

  send: async (sessionId, prompt, attachments = []) => {
    try {
      await invoke("send_prompt", {
        sessionId,
        prompt,
        ...(attachments.length > 0 && { attachments }),
      });
      const note = attachments.length > 0 ? `\n\n_(${attachments.length} image attached)_` : "";
      set((s) => ({
        transcripts: appendTranscript(s.transcripts, sessionId, {
          kind: "user",
          text: prompt + note,
        }),
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
        const todos = { ...s.todos };
        const usage = { ...s.usage };
        const agents = { ...s.agents };
        const mcpServers = { ...s.mcpServers };
        delete transcripts[sessionId];
        delete commands[sessionId];
        delete todos[sessionId];
        delete usage[sessionId];
        delete agents[sessionId];
        delete mcpServers[sessionId];
        return { transcripts, commands, todos, usage, agents, mcpServers };
      });
      await get().fetch(branch);
      return true;
    } catch (e) {
      set({ error: String(e) });
      return false;
    }
  },

  removeAllFinished: async (branch) => {
    const targets = (get().byBranch[branch] ?? []).filter((s) => isTerminalStatus(s.status));
    if (targets.length === 0) return 0;
    let removed = 0;
    for (const session of targets) {
      try {
        await invoke("delete_session", { sessionId: session.id });
        removed += 1;
      } catch (e) {
        set({ error: String(e) });
        // Keep going — one stubborn row (e.g. a race with a fresh spawn)
        // should not stop the rest of the cleanup.
      }
    }
    set((s) => {
      const transcripts = { ...s.transcripts };
      const commands = { ...s.commands };
      const todos = { ...s.todos };
      const usage = { ...s.usage };
      const agents = { ...s.agents };
      const mcpServers = { ...s.mcpServers };
      for (const session of targets) {
        delete transcripts[session.id];
        delete commands[session.id];
        delete todos[session.id];
        delete usage[session.id];
        delete agents[session.id];
        delete mcpServers[session.id];
      }
      return { transcripts, commands, todos, usage, agents, mcpServers };
    });
    await get().fetch(branch);
    return removed;
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

  setThinking: async (sessionId, thinking) => {
    try {
      await invoke("set_session_thinking", { sessionId, thinking });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  mcpAction: async (sessionId, server, action) => {
    try {
      // The new state comes back as a session.mcp_servers event.
      await invoke("mcp_server_action", { sessionId, server, action });
    } catch (e) {
      set({ error: String(e) });
    }
  },

  clearError: () => set({ error: null }),
}));

/** Transcript entry for an answered dialog, built from the answer alone. */
function summarize(answer: DialogAnswer | null): TranscriptItem {
  if (!answer) return { kind: "dialog", title: "Question dismissed", lines: [] };
  if (answer.approved !== undefined) {
    return {
      kind: "dialog",
      title: answer.approved ? "Plan approved" : "Kept planning",
      lines: answer.feedback?.trim() ? [answer.feedback.trim()] : [],
    };
  }
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
      // "The agent is done" is worth a ping: a toast always, and an OS
      // notification when the window isn't focused (same opt-in toggle the
      // attention panel uses). `awaiting_input` is the attention panel's job.
      if (status === "done" || status === "failed") {
        void (async () => {
          const { useToasts } = await import("./toasts");
          useToasts.getState().push({
            severity: status === "done" ? "info" : "warning",
            code: "session",
            message:
              status === "done"
                ? `Session on '${branch}' finished — diff is ready to review.`
                : `Session on '${branch}' failed.`,
          });
          const { useAttention } = await import("./attention");
          if (useAttention.getState().notificationsEnabled && !document.hasFocus()) {
            const { sendNotification } = await import("@tauri-apps/plugin-notification");
            sendNotification({
              title: "MaestroIDE",
              body:
                status === "done"
                  ? `Session on '${branch}' finished.`
                  : `Session on '${branch}' failed.`,
            });
          }
        })();
      }
      break;
    }
    case "session.stream_delta": {
      const { session_id, text, parent_tool_use_id } = event.data;
      useSessions.setState((s) =>
        parent_tool_use_id
          ? {
              transcripts: appendChild(s.transcripts, session_id, parent_tool_use_id, {
                kind: "text",
                text,
              }),
            }
          : {
              transcripts: appendTranscript(s.transcripts, session_id, { kind: "text", text }),
            },
      );
      break;
    }
    case "session.thinking_delta": {
      const { session_id, text, parent_tool_use_id } = event.data;
      useSessions.setState((s) =>
        parent_tool_use_id
          ? {
              transcripts: appendChild(s.transcripts, session_id, parent_tool_use_id, {
                kind: "thinking",
                text,
              }),
            }
          : {
              transcripts: appendTranscript(s.transcripts, session_id, { kind: "thinking", text }),
            },
      );
      break;
    }
    case "session.tool_use": {
      const { session_id, tool_use_id, name, summary, parent_tool_use_id } = event.data;
      useSessions.setState((s) =>
        parent_tool_use_id
          ? {
              transcripts: appendChild(s.transcripts, session_id, parent_tool_use_id, {
                kind: "tool_use",
                id: tool_use_id,
                name,
                summary,
              }),
            }
          : {
              transcripts: appendTranscript(s.transcripts, session_id, {
                kind: "tool_use",
                id: tool_use_id,
                name,
                summary,
                children: [],
              }),
            },
      );
      break;
    }
    case "session.tool_result": {
      const { session_id, tool_use_id, is_error, text } = event.data;
      useSessions.setState((s) => {
        const items = s.transcripts[session_id] ?? [];
        const patched = patchTool(items, tool_use_id, (tool) => ({
          ...tool,
          result: { isError: is_error, text },
        }));
        if (patched === items) return {};
        return { transcripts: { ...s.transcripts, [session_id]: patched } };
      });
      break;
    }
    case "session.agents": {
      const { session_id, agents } = event.data;
      useSessions.setState((s) => ({ agents: { ...s.agents, [session_id]: agents } }));
      break;
    }
    case "session.mcp_servers": {
      const { session_id, servers } = event.data;
      useSessions.setState((s) => {
        // A single-server reply (after a reconnect) patches that entry, not the list.
        const previous = s.mcpServers[session_id] ?? [];
        const merged =
          servers.length === 1 && previous.length > 1
            ? previous.map((p) => (p.name === servers[0].name ? servers[0] : p))
            : servers;
        return { mcpServers: { ...s.mcpServers, [session_id]: merged } };
      });
      break;
    }
    case "session.todos": {
      const { session_id, items } = event.data;
      useSessions.setState((s) => ({ todos: { ...s.todos, [session_id]: items } }));
      break;
    }
    case "session.usage": {
      const d = event.data;
      useSessions.setState((s) => {
        // Two flavours arrive per turn (turn totals, then a context reading); each only
        // carries its own fields, so they are merged rather than replaced.
        const previous = s.usage[d.session_id] ?? {};
        const next: SessionUsage = {
          ...previous,
          ...(d.total_cost_usd !== null && { costUsd: d.total_cost_usd }),
          ...(d.num_turns !== null && { turns: d.num_turns }),
          ...(d.input_tokens !== null && { inputTokens: d.input_tokens }),
          ...(d.output_tokens !== null && { outputTokens: d.output_tokens }),
          ...(d.context_tokens !== null && { contextTokens: d.context_tokens }),
          ...(d.context_max_tokens !== null && { contextMaxTokens: d.context_max_tokens }),
          ...(d.context_percent !== null && { contextPercent: d.context_percent }),
        };
        return { usage: { ...s.usage, [d.session_id]: next } };
      });
      break;
    }
    case "session.rate_limit": {
      const { status, limit_type, utilization, resets_at } = event.data;
      useSessions.setState({
        rateLimit: {
          status,
          ...(limit_type !== null && { limitType: limit_type }),
          ...(utilization !== null && { utilization }),
          ...(resets_at !== null && { resetsAt: resets_at }),
        },
      });
      break;
    }
    case "session.permission_denied": {
      const { session_id, tool, reason, message } = event.data;
      useSessions.setState((s) => ({
        transcripts: appendTranscript(s.transcripts, session_id, {
          kind: "denied",
          tool,
          reason,
          message,
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
      const { session_id, model, effort, permission_mode, thinking } = event.data;
      const parts = [
        model !== null && `model: ${model || "default"}`,
        effort !== null && `effort: ${effort || "default"}`,
        permission_mode !== null && `permissions: ${permission_mode}`,
        thinking !== null && `thinking: ${thinking || "default"}`,
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
                  thinking: thinking ?? sess.thinking,
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

// Autosave: every transcript change is debounced to the backend, so history survives
// a restart (or a clean `done`/`cancelled` close) without saving on every streamed
// character.
const SAVE_DEBOUNCE_MS = 800;
const pendingSaves = new Map<string, ReturnType<typeof setTimeout>>();

function persistTranscript(sessionId: string): Promise<void> {
  const items = useSessions.getState().transcripts[sessionId];
  if (!items) return Promise.resolve();
  return invoke("save_session_transcript", { sessionId, items }).then(
    () => undefined,
    () => undefined, // best-effort — one missed autosave isn't worth surfacing
  );
}

function scheduleSave(sessionId: string): void {
  const existing = pendingSaves.get(sessionId);
  if (existing) clearTimeout(existing);
  pendingSaves.set(
    sessionId,
    setTimeout(() => {
      pendingSaves.delete(sessionId);
      void persistTranscript(sessionId);
    }, SAVE_DEBOUNCE_MS),
  );
}

/** Flush every pending autosave immediately — await this before the app can exit. */
export async function flushTranscripts(): Promise<void> {
  const sessionIds = [...pendingSaves.keys()];
  for (const sessionId of sessionIds) {
    clearTimeout(pendingSaves.get(sessionId));
    pendingSaves.delete(sessionId);
  }
  await Promise.all(sessionIds.map((id) => persistTranscript(id)));
}

let previousTranscripts: SessionsState["transcripts"] = {};
useSessions.subscribe((state) => {
  const prev = previousTranscripts;
  previousTranscripts = state.transcripts;
  if (prev === state.transcripts) return;
  for (const sessionId of Object.keys(state.transcripts)) {
    if (state.transcripts[sessionId] !== prev[sessionId]) scheduleSave(sessionId);
  }
});
