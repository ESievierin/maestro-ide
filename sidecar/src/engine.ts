// Real session runner: wraps Claude Agent SDK `query()` in streaming-input mode.
//
// One AgentSession per Maestro session. The Rust core owns all state; this class
// only executes the session and maps the SDK stream to protocol events.

import {
  query,
  type Options,
  type PermissionMode,
  type PermissionResult,
  type Query,
  type SDKMessage,
  type SDKUserMessage,
} from "@anthropic-ai/claude-agent-sdk";

import type { SessionHandle, SidecarEvent, SpawnRequest } from "./protocol.js";
import { AsyncQueue } from "./queue.js";

const EFFORTS = new Set(["low", "medium", "high", "xhigh", "max"]);
const PERMISSION_MODES = new Set([
  "default",
  "acceptEdits",
  "bypassPermissions",
  "plan",
  "dontAsk",
  "auto",
]);

type Emit = (event: SidecarEvent) => void;

export class AgentSession implements SessionHandle {
  private readonly input = new AsyncQueue<SDKUserMessage>();
  private readonly abort = new AbortController();
  private readonly pendingPermissions = new Map<string, (result: PermissionResult) => void>();
  private q: Query | null = null;
  private closedByRequest = false;

  constructor(
    private readonly sessionId: string,
    private readonly emit: Emit,
    private readonly onEnd: (sessionId: string) => void,
  ) {}

  async spawn(req: SpawnRequest): Promise<void> {
    const options: Options = {
      cwd: req.cwd,
      abortController: this.abort,
      includePartialMessages: true,
      // Respect user/project settings — including per-worktree CLAUDE.md.
      settingSources: ["user", "project", "local"],
      systemPrompt: { type: "preset", preset: "claude_code" },
      canUseTool: (toolName, input, opts) => this.onPermissionRequest(toolName, input, opts),
    };
    if (req.model) options.model = req.model;
    if (req.effort && EFFORTS.has(req.effort)) {
      options.effort = req.effort as Options["effort"];
    }
    if (req.permission_mode && PERMISSION_MODES.has(req.permission_mode)) {
      options.permissionMode = req.permission_mode as PermissionMode;
    }
    if (req.resume_id) options.resume = req.resume_id;

    this.q = query({ prompt: this.input, options });
    void this.pump();

    if (req.prompt.trim().length > 0) {
      this.send(req.prompt);
    } else {
      // Resume/attach without an initial prompt: the session is idle right away.
      this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
    }
  }

  send(prompt: string): void {
    this.input.push({
      type: "user",
      message: { role: "user", content: prompt },
      parent_tool_use_id: null,
    });
    this.emit({ type: "status", session_id: this.sessionId, status: "streaming" });
  }

  async interrupt(): Promise<void> {
    if (!this.q) return;
    try {
      await this.q.interrupt();
    } catch (err) {
      this.emit({
        type: "error",
        session_id: this.sessionId,
        message: `interrupt failed: ${String(err)}`,
      });
      return;
    }
    this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
  }

  close(): void {
    this.closedByRequest = true;
    this.input.end();
    // Deny anything still waiting so the query can unwind.
    for (const [, resolve] of this.pendingPermissions) {
      resolve({ behavior: "deny", message: "Session closed" });
    }
    this.pendingPermissions.clear();
    this.abort.abort();
  }

  respondPermission(
    requestId: string,
    allow: boolean,
    updatedArgs?: Record<string, unknown>,
    message?: string,
  ): boolean {
    const resolve = this.pendingPermissions.get(requestId);
    if (!resolve) return false;
    this.pendingPermissions.delete(requestId);
    resolve(
      allow
        ? { behavior: "allow", updatedInput: updatedArgs }
        : { behavior: "deny", message: message ?? "Denied by user" },
    );
    return true;
  }

  private async pump(): Promise<void> {
    try {
      for await (const message of this.q as Query) {
        this.onMessage(message);
      }
      this.emit({
        type: "session_closed",
        session_id: this.sessionId,
        reason: this.closedByRequest ? "closed" : "ended",
      });
    } catch (err) {
      if (this.closedByRequest) {
        // AbortError after an explicit close is the expected unwind path.
        this.emit({ type: "session_closed", session_id: this.sessionId, reason: "closed" });
      } else {
        this.emit({ type: "error", session_id: this.sessionId, message: String(err) });
        this.emit({ type: "session_closed", session_id: this.sessionId, reason: "error" });
      }
    } finally {
      this.onEnd(this.sessionId);
    }
  }

  private onMessage(message: SDKMessage): void {
    switch (message.type) {
      case "system": {
        if (message.subtype === "init") {
          this.emit({
            type: "session_init",
            session_id: this.sessionId,
            sdk_session_id: message.session_id,
            model: message.model,
          });
          void this.reportCommands();
        }
        break;
      }
      case "stream_event": {
        // Only top-level assistant text; subagent output has parent_tool_use_id set.
        if (message.parent_tool_use_id !== null) break;
        const event = message.event as {
          type: string;
          delta?: { type: string; text?: string };
        };
        if (event.type === "content_block_delta" && event.delta?.type === "text_delta") {
          const text = event.delta.text ?? "";
          if (text.length > 0) {
            this.emit({ type: "stream_delta", session_id: this.sessionId, text });
          }
        }
        break;
      }
      case "assistant": {
        if (message.parent_tool_use_id !== null) break;
        for (const block of message.message.content) {
          if (block.type === "tool_use") {
            this.emit({
              type: "tool_use",
              session_id: this.sessionId,
              name: block.name,
              summary: summarizeInput(block.input),
            });
          }
        }
        break;
      }
      case "result": {
        this.emit({
          type: "result",
          session_id: this.sessionId,
          subtype: message.subtype,
          is_error: message.is_error,
          duration_ms: message.duration_ms,
          total_cost_usd: message.total_cost_usd,
          num_turns: message.num_turns,
        });
        this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
        break;
      }
      default:
        break;
    }
  }

  /** Fetch the slash commands this session supports (for input autocomplete). */
  private async reportCommands(): Promise<void> {
    if (!this.q) return;
    try {
      const commands = await this.q.supportedCommands();
      this.emit({
        type: "commands",
        session_id: this.sessionId,
        commands: commands.map((c) => ({
          name: c.name,
          description: c.description,
          argument_hint: c.argumentHint ?? "",
        })),
      });
    } catch (err) {
      // Autocomplete is best-effort; the chat works without it.
      this.emit({
        type: "error",
        session_id: this.sessionId,
        message: `supportedCommands failed: ${String(err)}`,
      });
    }
  }

  private onPermissionRequest(
    toolName: string,
    input: Record<string, unknown>,
    opts: { signal: AbortSignal; requestId?: string; title?: string },
  ): Promise<PermissionResult> {
    const requestId = opts.requestId ?? `perm-${this.sessionId}-${Date.now()}`;
    return new Promise<PermissionResult>((resolve) => {
      this.pendingPermissions.set(requestId, resolve);
      opts.signal.addEventListener("abort", () => {
        if (this.pendingPermissions.delete(requestId)) {
          resolve({ behavior: "deny", message: "Aborted" });
        }
      });
      this.emit({
        type: "permission_request",
        session_id: this.sessionId,
        request_id: requestId,
        tool: toolName,
        args: input,
        title: opts.title,
      });
    });
  }
}

function summarizeInput(input: unknown): string {
  try {
    const text = JSON.stringify(input);
    return text.length > 300 ? text.slice(0, 300) + "…" : text;
  } catch {
    return "<unserializable input>";
  }
}
