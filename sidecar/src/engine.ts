// Real session runner: wraps Claude Agent SDK `query()` in streaming-input mode.
//
// One AgentSession per Maestro session. The Rust core owns all state; this class
// only executes the session and maps the SDK stream to protocol events.

import {
  query,
  type EffortLevel,
  type Options,
  type PermissionMode,
  type PermissionResult,
  type Query,
  type SDKMessage,
  type SDKUserMessage,
  type UserDialogRequest,
  type UserDialogResult,
} from "@anthropic-ai/claude-agent-sdk";

import type { DialogAnswer, SessionHandle, SidecarEvent, SpawnRequest } from "./protocol.js";
import { AsyncQueue } from "./queue.js";

const EFFORTS = new Set(["low", "medium", "high", "xhigh", "max"]);
export const PERMISSION_MODES = new Set([
  "default",
  "acceptEdits",
  "bypassPermissions",
  "plan",
  "dontAsk",
  "auto",
]);

type Emit = (event: SidecarEvent) => void;

/**
 * A dialog the CLI itself raises has a park deadline on its side; ours is shorter so the
 * agent is never stuck on a modal nobody is looking at.
 */
const DIALOG_TIMEOUT_MS = 5 * 60 * 1000;

/**
 * The tool that asks the user a question. Verified against the CLI: its questions are
 * **not** delivered as a `request_user_dialog` — they arrive as an ordinary `canUseTool`
 * permission request, and the answers ride back on the decision:
 * `{behavior: "allow", updatedInput: {...input, answers, annotations}}`. Denying with a
 * message is how "ask me something else instead" is expressed. So Maestro turns this one
 * permission request into a dialog for the UI, and the dialog answer back into a decision.
 */
const ASK_TOOL = "AskUserQuestion";

/** Dialog kind Maestro publishes for `AskUserQuestion` (Maestro's own name, not the CLI's). */
const ASK_USER_QUESTION = "ask_user_question";

/**
 * Dialog kinds Maestro renders when the CLI raises one itself. The CLI **fails closed** on
 * this list — an undeclared kind is never emitted and the flow behind it degrades to its
 * no-dialog behaviour — so declaring a kind means having a renderer for it. Question
 * dialogs are absent on purpose: they arrive through the permission channel instead (see
 * `ASK_TOOL`). Candidates for later: `permission_exit_plan_mode_v2` (plan approval),
 * `refusal_fallback_prompt`, `fable_overage_consent_prompt`.
 */
export const SUPPORTED_DIALOG_KINDS: string[] = [];

/**
 * A question waiting on the user. `settle` is built by whichever channel raised it, so one
 * answer from the UI resolves either a permission decision or a CLI dialog result.
 */
interface PendingDialog {
  kind: string;
  settle: (behavior: "completed" | "cancelled", answer?: DialogAnswer) => void;
}

export class AgentSession implements SessionHandle {
  private readonly input = new AsyncQueue<SDKUserMessage>();
  private readonly abort = new AbortController();
  private readonly pendingPermissions = new Map<string, (result: PermissionResult) => void>();
  private readonly pendingDialogs = new Map<string, PendingDialog>();
  private dialogCounter = 0;
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
      // Answer CLI-raised dialogs too. `supportedDialogKinds` is the actual opt-in — the
      // callback alone receives nothing — so this stays inert until a kind is declared.
      onUserDialog: (request, opts) => this.onUserDialog(request, opts),
      ...(SUPPORTED_DIALOG_KINDS.length > 0 && {
        supportedDialogKinds: SUPPORTED_DIALOG_KINDS,
      }),
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

  /** Change the model mid-session; an empty string restores the default. */
  async setModel(model: string): Promise<void> {
    await this.q?.setModel(model.trim() === "" ? undefined : model);
  }

  /**
   * Change the effort mid-session. `effortLevel` is the settings key the CLI maps to
   * effort; `null` clears the override.
   */
  async setEffort(effort: string): Promise<void> {
    const level = effort.trim() === "" ? null : (effort as EffortLevel);
    await this.q?.applyFlagSettings({ effortLevel: level });
  }

  async setPermissionMode(mode: string): Promise<void> {
    if (!PERMISSION_MODES.has(mode)) {
      throw new Error(`unknown permission mode: ${mode}`);
    }
    await this.q?.setPermissionMode(mode as PermissionMode);
  }

  respondUserDialog(
    requestId: string,
    behavior: "completed" | "cancelled",
    result?: DialogAnswer,
  ): boolean {
    const pending = this.pendingDialogs.get(requestId);
    if (!pending) return false;
    this.pendingDialogs.delete(requestId);
    pending.settle(behavior, result);
    return true;
  }

  close(): void {
    this.closedByRequest = true;
    this.input.end();
    // Unanswered dialogs would keep the query parked; cancel them so it can unwind.
    for (const [, pending] of this.pendingDialogs) {
      pending.settle("cancelled");
    }
    this.pendingDialogs.clear();
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

  /**
   * Forward a CLI-raised dialog to the core and wait for the host's answer. Anything that
   * goes wrong resolves as `cancelled`: the CLI then applies the dialog's own default,
   * which is always better than parking the agent forever. Kinds outside
   * {@link SUPPORTED_DIALOG_KINDS} are cancelled straight away — inventing a result shape
   * for a dialog we cannot render is worse than letting its default stand.
   */
  private onUserDialog(
    request: UserDialogRequest,
    opts: { signal: AbortSignal },
  ): Promise<UserDialogResult> {
    const requestId = `dialog-${this.sessionId}-${++this.dialogCounter}`;
    return new Promise<UserDialogResult>((resolve) => {
      if (!SUPPORTED_DIALOG_KINDS.includes(request.dialogKind)) {
        resolve({ behavior: "cancelled" });
        return;
      }

      let settled = false;
      const finish = (result: UserDialogResult) => {
        if (settled) return;
        settled = true;
        this.pendingDialogs.delete(requestId);
        clearTimeout(timer);
        resolve(result);
      };

      const timer = setTimeout(() => {
        this.emit({
          type: "error",
          session_id: this.sessionId,
          message: `dialog "${request.dialogKind}" was not answered in time; cancelled`,
        });
        finish({ behavior: "cancelled" });
      }, DIALOG_TIMEOUT_MS);

      this.pendingDialogs.set(requestId, {
        kind: request.dialogKind,
        settle: (behavior, answer) =>
          finish(
            behavior === "cancelled"
              ? { behavior: "cancelled" }
              : { behavior: "completed", result: answer },
          ),
      });
      opts.signal.addEventListener("abort", () => finish({ behavior: "cancelled" }));

      this.emit({
        type: "user_dialog_request",
        session_id: this.sessionId,
        request_id: requestId,
        dialog_kind: request.dialogKind,
        payload: request.payload,
        tool_use_id: request.toolUseID,
      });
    });
  }

  /**
   * `AskUserQuestion` arrives as a permission request; render it as a dialog and answer the
   * permission with the user's choices. Allow carries the answers in `updatedInput` (the
   * tool reads them from there); deny with a message routes the user's own words back to
   * the agent, which is how "that is the wrong question" gets across.
   */
  private askQuestion(
    input: Record<string, unknown>,
    resolve: (result: PermissionResult) => void,
    signal: AbortSignal,
  ): void {
    const requestId = `dialog-${this.sessionId}-${++this.dialogCounter}`;
    let settled = false;
    const settle = (behavior: "completed" | "cancelled", answer?: DialogAnswer) => {
      if (settled) return;
      settled = true;
      this.pendingDialogs.delete(requestId);
      const feedback = answer?.feedback?.trim();
      if (behavior === "cancelled") {
        resolve({ behavior: "deny", message: "The user dismissed the question." });
      } else if (feedback) {
        resolve({ behavior: "deny", message: feedback });
      } else {
        resolve({
          behavior: "allow",
          updatedInput: {
            ...input,
            answers: answer?.answers ?? {},
            annotations: answer?.annotations ?? {},
          },
        });
      }
    };

    this.pendingDialogs.set(requestId, { kind: ASK_USER_QUESTION, settle });
    signal.addEventListener("abort", () => settle("cancelled"));

    this.emit({
      type: "user_dialog_request",
      session_id: this.sessionId,
      request_id: requestId,
      dialog_kind: ASK_USER_QUESTION,
      payload: input,
    });
  }

  private onPermissionRequest(
    toolName: string,
    input: Record<string, unknown>,
    opts: { signal: AbortSignal; requestId?: string; title?: string },
  ): Promise<PermissionResult> {
    const requestId = opts.requestId ?? `perm-${this.sessionId}-${Date.now()}`;
    return new Promise<PermissionResult>((resolve) => {
      if (toolName === ASK_TOOL) {
        // Not a question about permission — an actual question for the user.
        this.askQuestion(input, resolve, opts.signal);
        return;
      }
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
