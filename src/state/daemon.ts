import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { onBusEvent } from "./events";

/** Mirrors `GhAccount` in src-tauri/src/core/daemon/github.rs. */
export interface GhAccount {
  login: string;
  active: boolean;
}

/** Mirrors `DaemonTask` in src-tauri/src/core/store/. */
export interface DaemonTask {
  key: string;
  kind: "pr_review" | "pr_comment" | "jira" | string;
  state: "queued" | "running" | "done" | "failed" | "dismissed";
  title: string;
  payload: string;
  branch: string | null;
  session_id: string | null;
  attempts: number;
  created_at: string;
  updated_at: string;
}

/** Mirrors `DaemonStatus` in src-tauri/src/core/daemon/. */
export interface DaemonStatus {
  enabled: boolean;
  account: string;
  accounts: GhAccount[];
  watched_accounts: string[];
  skip_labels: string[];
  repo: string | null;
  jira_configured: boolean;
  queued: number;
  running: DaemonTask | null;
  last_poll: string | null;
  last_error: string | null;
  utilization: number | null;
}

interface DaemonState {
  status: DaemonStatus | null;
  tasks: DaemonTask[];

  fetchStatus: () => Promise<void>;
  fetchTasks: () => Promise<void>;
  setEnabled: (enabled: boolean) => Promise<void>;
  setAccount: (account: string) => Promise<void>;
  setWatchedAccounts: (accounts: string[]) => Promise<void>;
  setSkipLabels: (labels: string[]) => Promise<void>;
  /** Run one polling pass right now instead of waiting for the next
   * scheduled tick — works regardless of the master switch, same as
   * inspecting/changing any other daemon setting does. */
  pollNow: () => Promise<void>;
  dismiss: (key: string) => Promise<void>;
  /** Dismiss every done/failed task in one go. Dismissal only hides a row
   * from the panel (it neither deletes the row nor touches GitHub/Jira), so
   * this needs no confirmation — same reasoning as the single-task dismiss
   * button already not having one. Returns how many were dismissed. */
  dismissFinished: () => Promise<number>;
}

export const useDaemon = create<DaemonState>((set, get) => ({
  status: null,
  tasks: [],

  fetchStatus: async () => {
    try {
      set({ status: await invoke<DaemonStatus>("daemon_status") });
    } catch {
      // error.raised already surfaced it
    }
  },

  fetchTasks: async () => {
    try {
      set({ tasks: await invoke<DaemonTask[]>("list_daemon_tasks") });
    } catch {
      // error.raised already surfaced it
    }
  },

  setEnabled: async (enabled) => {
    try {
      await invoke("set_daemon_enabled", { enabled });
    } catch {
      // error.raised already surfaced it
    }
  },

  setAccount: async (account) => {
    try {
      await invoke("set_daemon_account", { account });
    } catch {
      // error.raised already surfaced it
    }
  },

  setWatchedAccounts: async (accounts) => {
    try {
      await invoke("set_daemon_watched_accounts", { accounts });
    } catch {
      // error.raised already surfaced it
    }
  },

  setSkipLabels: async (labels) => {
    try {
      await invoke("set_daemon_skip_labels", { labels });
    } catch {
      // error.raised already surfaced it
    }
  },

  pollNow: async () => {
    try {
      await invoke("daemon_poll_now");
    } catch {
      // error.raised already surfaced it
    }
  },

  dismiss: async (key) => {
    try {
      await invoke("dismiss_daemon_task", { key });
    } catch {
      // error.raised already surfaced it
    }
  },

  dismissFinished: async () => {
    const targets = get().tasks.filter((t) => t.state === "done" || t.state === "failed");
    for (const task of targets) {
      try {
        await invoke("dismiss_daemon_task", { key: task.key });
      } catch {
        // error.raised already surfaced it — keep going, one stubborn row
        // should not stop the rest of the cleanup.
      }
    }
    await get().fetchTasks();
    return targets.length;
  },
}));

onBusEvent((event) => {
  if (event.type === "daemon.updated") {
    const { fetchStatus, fetchTasks } = useDaemon.getState();
    void fetchStatus();
    void fetchTasks();
  }
  if (event.type === "daemon.task_finished") {
    void (async () => {
      const { useToasts } = await import("./toasts");
      const { title, ok } = event.data;
      useToasts
        .getState()
        .push(
          ok
            ? { severity: "info", code: "daemon", message: `Daemon finished: ${title}` }
            : { severity: "warning", code: "daemon", message: `Daemon task failed: ${title}` },
        );
    })();
  }
});
