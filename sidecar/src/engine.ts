// Real session runner: wraps Claude Agent SDK `query()` in streaming-input mode.
//
// One AgentSession per Maestro session. The Rust core owns all state; this class
// only executes the session and maps the SDK stream to protocol events.

import {
  query,
  type EffortLevel,
  type ElicitationRequest,
  type ElicitationResult,
  type Options,
  type PermissionMode,
  type PermissionResult,
  type Query,
  type SDKMessage,
  type SDKUserMessage,
  type UserDialogRequest,
  type UserDialogResult,
} from "@anthropic-ai/claude-agent-sdk";

import type {
  AgentInfo,
  Attachment,
  DialogAnswer,
  McpServerInfo,
  SessionHandle,
  SidecarEvent,
  SpawnRequest,
  TodoItem,
} from "./protocol.js";
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
 * Leaving plan mode. Like the question tool, this reaches us as a permission request whose
 * input is the plan itself, and approving it is what lets the agent start writing — so it
 * is rendered as its own dialog rather than a JSON blob in a permission prompt.
 */
const PLAN_TOOL = "ExitPlanMode";

/** Dialog kind Maestro publishes for `ExitPlanMode`. */
const PLAN_APPROVAL = "plan_approval";

/**
 * Dialog kind for an MCP server asking the user for something — typically finishing an
 * OAuth flow in the browser. Without a host answer the SDK declines it silently, and the
 * server's tools stay unusable with no explanation.
 */
const ELICITATION = "elicitation";

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
  /**
   * The agent's checklist. This CLI tracks work as *tasks* (task_started/updated/
   * notification system messages) rather than the older TodoWrite tool, so the list is
   * assembled from those and republished whole on every change.
   */
  private readonly tasks = new Map<string, { description: string; status: string }>();
  /**
   * Thinking characters streamed since the last assistant message. Adaptive thinking does
   * not always stream deltas, so the assistant message's thinking block is the fallback —
   * but only when nothing was streamed, or the transcript would show it twice.
   */
  private streamedThinking = 0;
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
      onElicitation: (request, opts) => this.onElicitation(request, opts),
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
    const thinking = thinkingConfig(req.thinking);
    if (thinking) options.thinking = thinking;
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

  send(prompt: string, attachments: Attachment[] = []): void {
    // Images ride along as content blocks; a plain string stays a plain string so the
    // common case looks exactly as it did before.
    const content =
      attachments.length === 0
        ? prompt
        : [
            ...(prompt.length > 0 ? [{ type: "text" as const, text: prompt }] : []),
            ...attachments.map((a) => ({
              type: "image" as const,
              source: { type: "base64" as const, media_type: a.media_type, data: a.data },
            })),
          ];
    this.input.push({
      type: "user",
      message: { role: "user", content } as SDKUserMessage["message"],
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

  /**
   * Change the thinking budget mid-session. `null` restores the API default; a budget
   * re-enables thinking even for a session that started with it disabled.
   */
  async setThinking(thinking: string): Promise<void> {
    const value = thinking.trim();
    if (value === "" || value === "default") {
      await this.q?.setMaxThinkingTokens(null);
      return;
    }
    if (value === "off") {
      await this.q?.setMaxThinkingTokens(0);
      return;
    }
    const budget = Number(value);
    if (!Number.isInteger(budget) || budget <= 0) {
      throw new Error(`invalid thinking budget: ${thinking}`);
    }
    // Summarized display, for the same reason as at spawn: omitted content shows nothing.
    await this.q?.setMaxThinkingTokens(budget, "summarized");
  }

  /**
   * Reconnect or enable/disable an MCP server. Enabling a failed server is the only way
   * out of `needs-auth` without restarting the session.
   */
  async mcpAction(server: string, action: string): Promise<void> {
    if (!this.q) throw new Error("session is not running");
    if (action === "reconnect") await this.q.reconnectMcpServer(server);
    else if (action === "enable") await this.q.toggleMcpServer(server, true);
    else if (action === "disable") await this.q.toggleMcpServer(server, false);
    else throw new Error(`unknown mcp action: ${action}`);
    await this.reportMcpServers();
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
        if (
          message.subtype === "task_started" ||
          message.subtype === "task_updated" ||
          message.subtype === "task_notification"
        ) {
          this.trackTask(message);
          break;
        }
        if (message.subtype === "permission_denied") {
          this.emit({
            type: "permission_denied",
            session_id: this.sessionId,
            tool: message.tool_name,
            reason: message.decision_reason ?? message.decision_reason_type ?? "denied",
            message: message.message,
          });
          break;
        }
        if (message.subtype === "init") {
          this.emit({
            type: "session_init",
            session_id: this.sessionId,
            sdk_session_id: message.session_id,
            model: message.model,
          });
          void this.reportCommands();
          void this.reportAgents();
          void this.reportMcpServers();
        }
        break;
      }
      case "stream_event": {
        // Subagent output keeps its parent id so the UI can nest it under the Task call.
        const parent = message.parent_tool_use_id ?? undefined;
        const event = message.event as {
          type: string;
          delta?: { type: string; text?: string; thinking?: string };
        };
        if (event.type !== "content_block_delta") break;
        if (event.delta?.type === "text_delta") {
          const text = event.delta.text ?? "";
          if (text.length > 0) {
            this.emit({
              type: "stream_delta",
              session_id: this.sessionId,
              text,
              ...(parent && { parent_tool_use_id: parent }),
            });
          }
        } else if (event.delta?.type === "thinking_delta") {
          const text = event.delta.thinking ?? "";
          if (text.length > 0) {
            if (!parent) this.streamedThinking += text.length;
            this.emit({
              type: "thinking_delta",
              session_id: this.sessionId,
              text,
              ...(parent && { parent_tool_use_id: parent }),
            });
          }
        }
        break;
      }
      case "assistant": {
        const parent = message.parent_tool_use_id ?? undefined;
        for (const block of message.message.content) {
          // Thinking that never streamed as deltas still belongs in the transcript.
          if (block.type === "thinking" && !parent && this.streamedThinking === 0) {
            const text = (block as { thinking?: string }).thinking ?? "";
            if (text.length > 0) {
              this.emit({ type: "thinking_delta", session_id: this.sessionId, text });
            }
            continue;
          }
          if (block.type !== "tool_use") continue;
          this.emit({
            type: "tool_use",
            session_id: this.sessionId,
            tool_use_id: block.id,
            name: block.name,
            summary: summarizeInput(block.input),
            ...(parent && { parent_tool_use_id: parent }),
          });
          // Older CLIs keep the checklist in TodoWrite's input; newer ones use tasks.
          if (block.name === "TodoWrite") {
            const items = todoItems(block.input);
            if (items) {
              this.emit({ type: "todos", session_id: this.sessionId, items });
            }
          }
        }
        this.streamedThinking = 0;
        break;
      }
      case "user": {
        // Tool results ride back on the user turn, keyed by the tool_use they answer.
        const content = message.message.content;
        if (typeof content === "string" || !Array.isArray(content)) break;
        for (const block of content) {
          if (block.type !== "tool_result") continue;
          this.emit({
            type: "tool_result",
            session_id: this.sessionId,
            tool_use_id: block.tool_use_id,
            is_error: block.is_error === true,
            text: resultText(block.content),
          });
        }
        break;
      }
      case "rate_limit_event": {
        const info = message.rate_limit_info;
        this.emit({
          type: "rate_limit",
          session_id: this.sessionId,
          status: info.status,
          ...(info.rateLimitType && { limit_type: info.rateLimitType }),
          ...(info.utilization !== undefined && { utilization: info.utilization }),
          ...(info.resetsAt !== undefined && {
            resets_at: new Date(info.resetsAt * 1000).toISOString(),
          }),
        });
        break;
      }
      case "result": {
        // Usage first: a host that treats `result` as end-of-turn still gets the numbers.
        this.emit({
          type: "usage",
          session_id: this.sessionId,
          total_cost_usd: message.total_cost_usd,
          num_turns: message.num_turns,
          input_tokens: message.usage?.input_tokens,
          output_tokens: message.usage?.output_tokens,
        });
        this.emit({
          type: "result",
          session_id: this.sessionId,
          subtype: message.subtype,
          is_error: message.is_error,
          duration_ms: message.duration_ms,
          total_cost_usd: message.total_cost_usd,
          num_turns: message.num_turns,
        });
        void this.reportContextUsage();
        this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
        break;
      }
      default:
        break;
    }
  }

  /**
   * Fold a task lifecycle message into the checklist and republish it. Ambient tasks the
   * CLI marks `skip_transcript` are housekeeping, not the user's plan, so they stay out.
   */
  private trackTask(message: {
    subtype: string;
    task_id: string;
    description?: string;
    status?: string;
    summary?: string;
    skip_transcript?: boolean;
    patch?: { status?: string; description?: string };
  }): void {
    if (message.skip_transcript) return;
    const existing = this.tasks.get(message.task_id);
    if (message.subtype === "task_started") {
      this.tasks.set(message.task_id, {
        description: message.description ?? existing?.description ?? "task",
        status: "in_progress",
      });
    } else if (message.subtype === "task_updated") {
      if (!existing) return;
      this.tasks.set(message.task_id, {
        description: message.patch?.description ?? existing.description,
        status: taskStatus(message.patch?.status) ?? existing.status,
      });
    } else {
      if (!existing) return;
      this.tasks.set(message.task_id, {
        description: existing.description,
        status: taskStatus(message.status) ?? "completed",
      });
    }
    this.emit({
      type: "todos",
      session_id: this.sessionId,
      items: [...this.tasks.values()].map((t) => ({ content: t.description, status: t.status })),
    });
  }

  /** Subagent profiles this session can delegate to (`Task` targets). */
  private async reportAgents(): Promise<void> {
    if (!this.q) return;
    try {
      const agents = await this.q.supportedAgents();
      this.emit({
        type: "agents",
        session_id: this.sessionId,
        agents: agents.map((a): AgentInfo => ({
          name: a.name,
          description: a.description,
          model: a.model ?? "",
        })),
      });
    } catch {
      // Delegation still works without the list; it is only discoverability.
    }
  }

  /** MCP servers and their connection state, so a failed one is visible. */
  private async reportMcpServers(): Promise<void> {
    if (!this.q) return;
    try {
      const servers = await this.q.mcpServerStatus();
      this.emit({
        type: "mcp_servers",
        session_id: this.sessionId,
        servers: servers.map((s): McpServerInfo => ({
          name: s.name,
          status: s.status,
          tool_count: Array.isArray(s.tools) ? s.tools.length : 0,
          detail: s.serverInfo ? `${s.serverInfo.name} ${s.serverInfo.version}` : "",
        })),
      });
    } catch {
      // A session without MCP servers answers with an error on some CLI builds.
    }
  }

  /**
   * How full the context window is. A control request, so it costs no tokens; failures are
   * swallowed because a missing meter must never break a turn.
   */
  private async reportContextUsage(): Promise<void> {
    if (!this.q) return;
    try {
      const usage = await this.q.getContextUsage();
      this.emit({
        type: "usage",
        session_id: this.sessionId,
        context_tokens: usage.totalTokens,
        context_max_tokens: usage.maxTokens,
        context_percent: usage.percentage,
      });
    } catch {
      // The meter is best-effort.
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
   * An MCP server wants input. Form-mode requests carry a JSON schema Maestro cannot
   * render, so those are declined with the reason visible in the UI; URL-mode requests
   * (browser auth) are the ones a user can actually complete.
   */
  private onElicitation(
    request: ElicitationRequest,
    opts: { signal: AbortSignal },
  ): Promise<ElicitationResult> {
    const requestId = `dialog-${this.sessionId}-${++this.dialogCounter}`;
    return new Promise<ElicitationResult>((resolve) => {
      let settled = false;
      const settle = (behavior: "completed" | "cancelled", answer?: DialogAnswer) => {
        if (settled) return;
        settled = true;
        this.pendingDialogs.delete(requestId);
        clearTimeout(timer);
        if (behavior === "completed" && answer?.approved) {
          resolve({ action: "accept" });
        } else if (behavior === "cancelled") {
          resolve({ action: "cancel" });
        } else {
          resolve({ action: "decline" });
        }
      };

      const timer = setTimeout(() => {
        this.emit({
          type: "error",
          session_id: this.sessionId,
          message: `${request.serverName} asked for input and was not answered in time`,
        });
        settle("cancelled");
      }, DIALOG_TIMEOUT_MS);

      this.pendingDialogs.set(requestId, { kind: ELICITATION, settle });
      opts.signal.addEventListener("abort", () => settle("cancelled"));

      this.emit({
        type: "user_dialog_request",
        session_id: this.sessionId,
        request_id: requestId,
        dialog_kind: ELICITATION,
        payload: {
          server: request.serverName,
          message: request.message,
          mode: request.mode ?? "form",
          ...(request.url && { url: request.url }),
          ...(request.title && { title: request.title }),
          ...(request.description && { description: request.description }),
          // A schema means a form Maestro cannot fill in; the UI says so.
          form: request.requestedSchema !== undefined,
        },
      });
    });
  }

  /**
   * Ask the user to approve the plan. Allowing is what takes the session out of plan mode,
   * so the core gets a say first (the branch's single writer slot); rejecting sends the
   * user's words back so the agent can revise instead of stopping.
   */
  private askApproval(
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
      if (behavior === "completed" && answer?.approved) {
        resolve({ behavior: "allow", updatedInput: input });
      } else {
        resolve({
          behavior: "deny",
          message: feedback || "The user wants to keep planning.",
        });
      }
    };

    this.pendingDialogs.set(requestId, { kind: PLAN_APPROVAL, settle });
    signal.addEventListener("abort", () => settle("cancelled"));

    this.emit({
      type: "user_dialog_request",
      session_id: this.sessionId,
      request_id: requestId,
      dialog_kind: PLAN_APPROVAL,
      payload: input,
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
      if (toolName === PLAN_TOOL) {
        this.askApproval(input, resolve, opts.signal);
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

/**
 * Map Maestro's thinking setting to the SDK's config. `undefined` means "leave the CLI
 * alone", which is not the same as adaptive: it is whatever the CLI would do by itself.
 */
function thinkingConfig(thinking: string | undefined): Options["thinking"] | undefined {
  const value = thinking?.trim();
  if (!value || value === "default") return undefined;
  if (value === "off") return { type: "disabled" };
  if (value === "adaptive") return { type: "adaptive" };
  const budget = Number(value);
  if (!Number.isInteger(budget) || budget <= 0) return undefined;
  // `display` is the difference between reasoning we can show and an empty thinking
  // block: without it the CLI omits the content, and the transcript has nothing to render.
  return { type: "enabled", budgetTokens: budget, display: "summarized" };
}

/** CLI task status → the three states the checklist renders (plus failure). */
function taskStatus(status: string | undefined): string | undefined {
  switch (status) {
    case "running":
      return "in_progress";
    case "completed":
      return "completed";
    case "failed":
    case "killed":
    case "stopped":
      return "failed";
    case "pending":
    case "paused":
      return "pending";
    default:
      return undefined;
  }
}

/** TodoWrite's input, if it has the expected shape. */
function todoItems(input: unknown): TodoItem[] | null {
  if (typeof input !== "object" || input === null) return null;
  const todos = (input as { todos?: unknown }).todos;
  if (!Array.isArray(todos)) return null;
  const items = todos
    .filter((t): t is Record<string, unknown> => typeof t === "object" && t !== null)
    .map((t) => ({
      content: typeof t.content === "string" ? t.content : "",
      status: typeof t.status === "string" ? t.status : "pending",
    }))
    .filter((t) => t.content.length > 0);
  return items.length > 0 ? items : null;
}

/** Flatten a tool result's content blocks into the text the UI shows. */
function resultText(content: unknown): string {
  if (typeof content === "string") return truncate(content);
  if (!Array.isArray(content)) return "";
  const parts: string[] = [];
  for (const block of content) {
    if (typeof block !== "object" || block === null) continue;
    const b = block as { type?: string; text?: string };
    if (b.type === "text" && typeof b.text === "string") parts.push(b.text);
    else if (b.type === "image") parts.push("[image]");
  }
  return truncate(parts.join("\n"));
}

/** Tool output can be a whole file; the transcript only needs the head of it. */
function truncate(text: string, limit = 4000): string {
  return text.length > limit ? text.slice(0, limit) + "\n… (truncated)" : text;
}

function summarizeInput(input: unknown): string {
  try {
    const text = JSON.stringify(input);
    return text.length > 300 ? text.slice(0, 300) + "…" : text;
  } catch {
    return "<unserializable input>";
  }
}
