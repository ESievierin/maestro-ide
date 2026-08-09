import { useSessions } from "../state/sessions";
import { isTerminalStatus } from "../types/sessions";
import type { Session, TranscriptItem } from "../types/sessions";

/**
 * "Ask an agent and wait for the answer" — the primitive behind commit-message
 * / PR-description generation and the PR-review reply flow. Both want the
 * *agent's own context* (what it knows about the branch), not a stateless
 * read of the diff, so they go through a real session instead of a one-shot
 * CLI call: either a throwaway session resumed from the branch's own
 * implementation session, or a follow-up message on an already-open one.
 */

function collectAnswerText(items: TranscriptItem[], fromIndex: number): string {
  return items
    .slice(fromIndex)
    .filter((item): item is Extract<TranscriptItem, { kind: "text" }> => item.kind === "text")
    .map((item) => item.text)
    .join("\n\n")
    .trim();
}

/**
 * Wait for the next turn on `sessionId` to end — `awaiting_input` or a
 * terminal status — counting only transcript items appended from
 * `fromIndex` on, and return the concatenated assistant text. `ok: false`
 * on a failed/cancelled turn or a timeout (the partial text, if any, is
 * still returned — better than nothing).
 */
export function waitForTurn(
  sessionId: string,
  fromIndex: number,
  timeoutMs = 180_000,
): Promise<{ text: string; ok: boolean }> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (ok: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      unsub();
      const items = useSessions.getState().transcripts[sessionId] ?? [];
      resolve({ text: collectAnswerText(items, fromIndex), ok });
    };
    const timer = setTimeout(() => finish(false), timeoutMs);
    const unsub = useSessions.subscribe((state) => {
      const items = state.transcripts[sessionId] ?? [];
      for (let i = fromIndex; i < items.length; i += 1) {
        const item = items[i];
        if (item.kind !== "status") continue;
        if (item.status === "awaiting_input" || item.status === "done") {
          finish(true);
          return;
        }
        if (item.status === "failed" || item.status === "cancelled") {
          finish(false);
          return;
        }
      }
    });
  });
}

/** Wait for `sessionId` (on `branch`) to reach a terminal status, or to
 * disappear from the store entirely — used after closing a conflicting
 * writer session before retrying a plan approval on another one. `close()`
 * only *requests* the close; an implementation session's finalize-turn can
 * keep it non-terminal for a while afterward. */
export function waitForTerminal(
  sessionId: string,
  branch: string,
  timeoutMs = 20_000,
): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const finish = (ok: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      unsub();
      resolve(ok);
    };
    const check = () => {
      const session = (useSessions.getState().byBranch[branch] ?? []).find(
        (s) => s.id === sessionId,
      );
      if (!session || isTerminalStatus(session.status)) finish(true);
    };
    check();
    const timer = setTimeout(() => finish(false), timeoutMs);
    const unsub = useSessions.subscribe(check);
  });
}

/** The latest session of `branch` matching one of `types` (checked in that
 * priority order) that has a resumable SDK context. Mirrors the backend's
 * own `ask_original_agent` target resolution. */
export function findResumableSession(branch: string, types: string[]): Session | undefined {
  const sessions = useSessions.getState().byBranch[branch] ?? [];
  for (const type of types) {
    const candidates = sessions
      .filter((s) => s.session_type === type && s.sdk_session_id)
      .sort((a, b) => a.created_at.localeCompare(b.created_at));
    if (candidates.length > 0) return candidates[candidates.length - 1];
  }
  return undefined;
}

export interface AskResult {
  sessionId: string;
  text: string;
  ok: boolean;
}

/**
 * Spawn a throwaway session (plan mode by default) to answer one question,
 * optionally resuming another session's context, then close it — the
 * conversation the user sees is unaffected, only the answer matters.
 */
export async function askViaNewSession(opts: {
  branch: string;
  prompt: string;
  sessionType?: string;
  resumeFrom?: string;
  model?: string;
  effort?: string;
  permissionMode?: string;
  timeoutMs?: number;
  autoClose?: boolean;
}): Promise<AskResult | null> {
  const session = await useSessions.getState().spawn({
    branch: opts.branch,
    prompt: opts.prompt,
    session_type: opts.sessionType ?? "manual",
    model: opts.model,
    effort: opts.effort,
    permission_mode: opts.permissionMode ?? "plan",
    resume_from: opts.resumeFrom,
  });
  if (!session) return null;
  const { text, ok } = await waitForTurn(session.id, 0, opts.timeoutMs);
  if (opts.autoClose !== false) {
    void useSessions.getState().close(session.id);
  }
  return { sessionId: session.id, text, ok };
}

/** Send a follow-up message to an already-open session and wait for its
 * answer — used to ask a live review session for its final reply drafts. */
export async function askViaFollowup(opts: {
  sessionId: string;
  prompt: string;
  effort?: string;
  timeoutMs?: number;
}): Promise<{ text: string; ok: boolean }> {
  const fromIndex = (useSessions.getState().transcripts[opts.sessionId] ?? []).length;
  if (opts.effort) {
    await useSessions.getState().setEffort(opts.sessionId, opts.effort);
  }
  await useSessions.getState().send(opts.sessionId, opts.prompt);
  return waitForTurn(opts.sessionId, fromIndex, opts.timeoutMs);
}
