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
  dismiss: (key: string) => Promise<void>;
}

export const useDaemon = create<DaemonState>((set) => ({
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

  dismiss: async (key) => {
    try {
      await invoke("dismiss_daemon_task", { key });
    } catch {
      // error.raised already surfaced it
    }
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
