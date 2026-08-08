import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/** Mirrors `PrPromptResult` in src-tauri/src/ipc/. */
export interface PrPromptResult {
  /** The base actually used — echoes back an auto-detected one. */
  base: string;
  prompt: string;
}

/** Mirrors `CreatedPr` in src-tauri/src/core/pr/. */
export interface CreatedPr {
  url: string;
  push_report: string;
}

/** Mirrors `PrComment` in src-tauri/src/core/pr/. */
export interface PrComment {
  pr: number;
  id: number;
  author: string;
  path: string;
  body: string;
  url: string;
}

/** Mirrors `ReplyOutcome` in src-tauri/src/core/pr/. */
export interface ReplyOutcome {
  comment_id: number;
  ok: boolean;
  detail: string;
}

/** Thin invoke wrappers around the PR workflow's backend surface. Prompt
 * rendering (git context in, text out) lives here; the actual generation
 * happens through a real session — see `src/utils/agentAsk.ts`. Every
 * failure already surfaced as an error toast via error.raised, so callers
 * just get null and keep their dialog state. */
interface PrState {
  renderCommitPrompt: (branch: string, base?: string | null) => Promise<string | null>;
  renderPrPrompt: (branch: string, base?: string | null) => Promise<PrPromptResult | null>;
  renderReplyFollowup: (extra?: string) => Promise<string | null>;
  createPr: (
    branch: string,
    title: string,
    body: string,
    base?: string | null,
  ) => Promise<CreatedPr | null>;
  listComments: (branch: string) => Promise<PrComment[] | null>;
  postReplies: (
    pr: number,
    replies: { comment_id: number; body: string }[],
  ) => Promise<ReplyOutcome[] | null>;
}

export const usePr = create<PrState>(() => ({
  renderCommitPrompt: async (branch, base) => {
    try {
      return await invoke<string>("render_commit_prompt", { branch, base: base ?? null });
    } catch {
      return null;
    }
  },

  renderPrPrompt: async (branch, base) => {
    try {
      return await invoke<PrPromptResult>("render_pr_prompt", { branch, base: base ?? null });
    } catch {
      return null;
    }
  },

  renderReplyFollowup: async (extra) => {
    try {
      return await invoke<string>("render_pr_reply_followup", { extra: extra ?? null });
    } catch {
      return null;
    }
  },

  createPr: async (branch, title, body, base) => {
    try {
      return await invoke<CreatedPr>("create_pr", { branch, title, body, base: base ?? null });
    } catch {
      return null;
    }
  },

  listComments: async (branch) => {
    try {
      return await invoke<PrComment[]>("list_pr_comments", { branch });
    } catch {
      return null;
    }
  },

  postReplies: async (pr, replies) => {
    try {
      return await invoke<ReplyOutcome[]>("reply_pr_comments", { pr, replies });
    } catch {
      return null;
    }
  },
}));

export async function openUrl(url: string): Promise<void> {
  try {
    await invoke("open_url", { url });
  } catch {
    // error.raised already surfaced it
  }
}
