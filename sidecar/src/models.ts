// Global model list (no session): the CLI is the only authority on which models the
// user actually has, and asking it costs nothing — the query is closed before any turn
// runs, so no prompt is ever sent.

import { query, type Options } from "@anthropic-ai/claude-agent-sdk";

import { writeEvent, type ModelOption } from "./protocol.js";
import { AsyncQueue } from "./queue.js";

/** Give up rather than hang the request forever if the CLI never initializes. */
const TIMEOUT_MS = 30_000;

export async function reportModels(cwd: string): Promise<void> {
  // An input queue that stays empty: the query initializes, answers our metadata
  // question, and is closed without a turn.
  const input = new AsyncQueue<never>();
  const abort = new AbortController();
  const options: Options = {
    cwd,
    abortController: abort,
    settingSources: ["user", "project", "local"],
  };

  const q = query({ prompt: input, options });
  const timer = setTimeout(() => abort.abort(), TIMEOUT_MS);
  try {
    const models = await q.supportedModels();
    const mapped: ModelOption[] = models.map((m) => ({
      id: m.value,
      display_name: m.displayName,
    }));
    writeEvent({ type: "models", session_id: "", models: mapped });
  } catch (err) {
    writeEvent({ type: "error", message: `list_models failed: ${String(err)}` });
  } finally {
    clearTimeout(timer);
    input.end();
    try {
      q.close();
    } catch {
      // Already gone — nothing to clean up.
    }
  }
}
