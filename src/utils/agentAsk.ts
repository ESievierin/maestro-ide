import { useSessions } from "../state/sessions";
import { isTerminalStatus } from "../types/sessions";
import type { TranscriptItem } from "../types/sessions";

/**
 * "Ask an agent and wait for the answer" — the primitive behind commit-message
 * / PR-description generation and the PR-review reply flow. Both want the
 * *agent's own context* (what it knows about the branch), not a stateless
 * read of the diff, so they go through the branch's persistent main agent
 * instead of a one-shot CLI call.
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

export interface AskResult {
  sessionId: string;
  text: string;
  ok: boolean;
}

/** Send a follow-up message to an already-open session and wait for its
 * answer — used to ask a live review session for its final reply drafts. */
export async function askViaFollowup(opts: {
  sessionId: string;
  prompt: string;
  model?: string;
  effort?: string;
  timeoutMs?: number;
}): Promise<{ text: string; ok: boolean }> {
  const fromIndex = (useSessions.getState().transcripts[opts.sessionId] ?? []).length;
  if (opts.model) {
    await useSessions.getState().setModel(opts.sessionId, opts.model);
  }
  if (opts.effort) {
    await useSessions.getState().setEffort(opts.sessionId, opts.effort);
  }
  await useSessions.getState().send(opts.sessionId, opts.prompt);
  return waitForTurn(opts.sessionId, fromIndex, opts.timeoutMs);
}

/** Wait until `sessionId` is idle (`awaiting_input`). Sending while another
 * turn streams wouldn't corrupt anything — the SDK queues messages — but
 * `waitForTurn` would latch onto the *in-flight* turn's completion and return
 * the wrong task's text as the answer. `false` on timeout, disappearance, or
 * a terminal status. */
function waitForIdle(sessionId: string, branch: string, timeoutMs = 120_000): Promise<boolean> {
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
      if (!session || isTerminalStatus(session.status)) return finish(false);
      if (session.status === "awaiting_input") finish(true);
    };
    check();
    const timer = setTimeout(() => finish(false), timeoutMs);
    const unsub = useSessions.subscribe(check);
  });
}

/**
 * Resolve (creating if necessary) the branch's persistent main-agent session,
 * send it `prompt`, and wait for the reply — the primitive behind PR review
 * drafts and commit/PR-description generation now that they all go through
 * one continuous conversation instead of a throwaway resumed session.
 *
 * Waits for the agent to go idle first: it may be mid-turn on something else
 * (a daemon review, a revival's opening turn), and the answer must belong to
 * *this* question.
 */
export async function askMainAgent(opts: {
  branch: string;
  prompt: string;
  model?: string;
  effort?: string;
  timeoutMs?: number;
}): Promise<AskResult | null> {
  const session = await useSessions.getState().ensureMain(opts.branch);
  if (!session) return null;
  const idle = await waitForIdle(session.id, opts.branch);
  if (!idle) {
    useSessions.setState({
      error: "The main agent is busy with another turn — try again when it finishes.",
    });
    return null;
  }
  const { text, ok } = await askViaFollowup({
    sessionId: session.id,
    prompt: opts.prompt,
    model: opts.model,
    effort: opts.effort,
    timeoutMs: opts.timeoutMs,
  });
  return { sessionId: session.id, text, ok };
}
