import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/** Mirrors `PrDraft` in src-tauri/src/core/compose/. */
export interface PrDraft {
  title: string;
  body: string;
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

/** Thin invoke wrappers; every failure already surfaced as an error toast via
 * error.raised, so callers just get null and keep their dialog state. */
interface PrState {
  generateCommitMessage: (branch: string) => Promise<string | null>;
  generatePrDescription: (branch: string) => Promise<PrDraft | null>;
  createPr: (branch: string, title: string, body: string) => Promise<CreatedPr | null>;
  listComments: (branch: string) => Promise<PrComment[] | null>;
  generateReplies: (
    branch: string,
    comments: PrComment[],
  ) => Promise<Record<number, string> | null>;
  postReplies: (
    pr: number,
    replies: { comment_id: number; body: string }[],
  ) => Promise<ReplyOutcome[] | null>;
}

export const usePr = create<PrState>(() => ({
  generateCommitMessage: async (branch) => {
    try {
      return await invoke<string>("generate_commit_message", { branch });
    } catch {
      return null;
    }
  },

  generatePrDescription: async (branch) => {
    try {
      return await invoke<PrDraft>("generate_pr_description", { branch });
    } catch {
      return null;
    }
  },

  createPr: async (branch, title, body) => {
    try {
      return await invoke<CreatedPr>("create_pr", { branch, title, body });
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

  generateReplies: async (branch, comments) => {
    try {
      return await invoke<Record<number, string>>("generate_pr_replies", {
        branch,
        comments: comments.map((c) => ({
          comment_id: c.id,
          author: c.author,
          path: c.path,
          body: c.body,
        })),
      });
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
