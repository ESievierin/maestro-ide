// Mock session runner (MAESTRO_SIDECAR_MOCK=1): scripted events, no SDK, no API usage.
// Used by Rust integration tests and for manual UI testing of the whole pipeline.
//
// Prompt keywords: "PERMISSION" pauses on a chat permission request; "GATE" asks to run
// a push+PR command (exercises the T7 approval dialog); "ASK" raises a question dialog
// (AskUserQuestion path); "HOOKCHECK" runs a push command through the PreToolUse gate;
// "ESCALATE" asks the original agent through the core;
// "PLAN" asks the user to approve a plan; "REVIEW_COMMENTS" raises the
// submit_review_comments approval dialog with a mix of a new comment and a
// reply; "AUTH" raises an MCP
// elicitation; "THINK" streams a
// thinking block; "TOOLS" runs a tool with a
// result; "EDIT_FILE" runs an Edit tool call against a real file in the
// session's own worktree (tests the diff-viewer "jump to this file" button);
// "SUBAGENT" nests subagent output under a Task call; "TODO" publishes a todo
// list; "DENY" reports an auto-denied tool call; "CRASH" kills the whole sidecar process
// (supervisor recovery testing). Every turn also reports usage.

import type { DialogAnswer, SessionHandle, SidecarEvent, SpawnRequest } from "./protocol.js";

type Emit = (event: SidecarEvent) => void;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export class MockSession implements SessionHandle {
  private interrupted = false;
  private closed = false;
  private permissionCounter = 0;
  private dialogCounter = 0;
  private toolCounter = 0;
  private turns = 0;
  private model = "";
  private effort = "";
  private thinking = "";
  private permissionMode = "";
  private cwd = "";
  private readonly pending = new Map<string, (allow: boolean) => void>();
  private readonly pendingDialogs = new Map<string, (answer: string) => void>();
  private readonly pendingEscalations = new Map<string, (result: string) => void>();
  private readonly pendingGates = new Map<string, (verdict: string) => void>();
  private gateCounter = 0;
  private escalationCounter = 0;

  constructor(
    private readonly sessionId: string,
    private readonly emit: Emit,
    private readonly onEnd: (sessionId: string) => void,
  ) {}

  async spawn(req: SpawnRequest): Promise<void> {
    this.cwd = req.cwd;
    this.emit({
      type: "session_init",
      session_id: this.sessionId,
      sdk_session_id: `mock-${this.sessionId}`,
      model: req.model ?? "mock-model",
    });
    this.emit({
      type: "commands",
      session_id: this.sessionId,
      commands: [
        { name: "compact", description: "Compact the conversation", argument_hint: "" },
        { name: "cost", description: "Show session cost", argument_hint: "" },
        { name: "review", description: "Review a pull request", argument_hint: "<pr>" },
      ],
    });
    this.emit({
      type: "models",
      session_id: this.sessionId,
      models: [
        { id: "mock-model", display_name: "Mock Model" },
        { id: "claude-fable-5", display_name: "Claude Fable 5" },
      ],
    });
    this.emit({
      type: "agents",
      session_id: this.sessionId,
      agents: [
        { name: "explore", description: "Read-only search agent", model: "" },
        { name: "plan", description: "Architect agent", model: "claude-opus-5" },
      ],
    });
    this.emit({
      type: "mcp_servers",
      session_id: this.sessionId,
      servers: [
        { name: "mock-mcp", status: "connected", tool_count: 3, detail: "mock 1.0" },
        { name: "broken-mcp", status: "needs-auth", tool_count: 0, detail: "" },
      ],
    });
    if (req.prompt.trim().length > 0) {
      this.send(req.prompt);
    } else {
      this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
    }
  }

  send(prompt: string, attachments: { media_type: string; data: string }[] = []): void {
    // Echo the attachment count so the UI round trip is observable in mock mode.
    const suffix = attachments.length > 0 ? ` (+${attachments.length} image)` : "";
    void this.runTurn(prompt + suffix);
  }

  /** Trigger words match as standalone tokens, never substrings — a real
   * prompt mentioning "TASK_NOTES.md" must not trip the "ASK" scenario.
   * `_` is a word character, so compound triggers (REVIEW_COMMENTS) still
   * match whole and "REVIEW_PLAN.md" does not trip "PLAN". */
  private triggered(prompt: string, word: string): boolean {
    return new RegExp(`\\b${word}\\b`).test(prompt);
  }

  private async runTurn(prompt: string): Promise<void> {
    this.interrupted = false;
    this.emit({ type: "status", session_id: this.sessionId, status: "streaming" });

    if (this.triggered(prompt, "CRASH")) {
      await sleep(20);
      process.exit(13);
    }

    if (this.triggered(prompt, "PERMISSION") || this.triggered(prompt, "GATE")) {
      const requestId = `mock-perm-${this.sessionId}-${++this.permissionCounter}`;
      // "GATE" exercises the T7 approval path: a push+PR command the gate must
      // intercept with editable PR fields. Plain "PERMISSION" stays a chat prompt.
      const command = this.triggered(prompt, "GATE")
        ? 'git push -u origin HEAD && gh pr create --title "Mock PR" --body "Body from the agent"'
        : "echo mock";
      const allowed = await new Promise<boolean>((resolve) => {
        this.pending.set(requestId, resolve);
        this.emit({
          type: "permission_request",
          session_id: this.sessionId,
          request_id: requestId,
          tool: "Bash",
          args: { command },
          title: `Mock wants to run: ${command}`,
        });
      });
      this.emit({
        type: "tool_use",
        session_id: this.sessionId,
        tool_use_id: `mock-tool-${this.permissionCounter}`,
        name: "Bash",
        summary: allowed ? JSON.stringify({ command }) : "(denied)",
      });
      this.emit({
        type: "tool_result",
        session_id: this.sessionId,
        tool_use_id: `mock-tool-${this.permissionCounter}`,
        is_error: !allowed,
        text: allowed ? "mock\n" : "Denied by user",
      });
    }

    if (this.triggered(prompt, "THINK")) {
      for (const part of ["Let me think. ", "The mock has two options. ", "Option two it is."]) {
        if (this.interrupted || this.closed) break;
        this.emit({ type: "thinking_delta", session_id: this.sessionId, text: part });
        await sleep(80);
      }
    }

    if (this.triggered(prompt, "TOOLS")) {
      const id = `mock-read-${++this.toolCounter}`;
      this.emit({
        type: "tool_use",
        session_id: this.sessionId,
        tool_use_id: id,
        name: "Read",
        summary: JSON.stringify({ file_path: "src/main.ts" }),
      });
      await sleep(120);
      this.emit({
        type: "tool_result",
        session_id: this.sessionId,
        tool_use_id: id,
        is_error: false,
        text: "1\timport { run } from './run.js';\n2\trun();",
      });
    }

    if (this.triggered(prompt, "EDIT_FILE")) {
      // Tests the diff-viewer "jump" button: a real Edit call reports an
      // absolute path, same as this — built from the session's own cwd so it
      // resolves to whatever worktree actually spawned this mock session.
      const id = `mock-edit-${++this.toolCounter}`;
      const filePath = `${this.cwd.replace(/[\\/]+$/, "")}/BusinessLogic/LinkedInActionService.cs`;
      this.emit({
        type: "tool_use",
        session_id: this.sessionId,
        tool_use_id: id,
        name: "Edit",
        summary: JSON.stringify({
          file_path: filePath,
          old_string: "old code",
          new_string: "new code",
        }),
      });
      await sleep(120);
      this.emit({
        type: "tool_result",
        session_id: this.sessionId,
        tool_use_id: id,
        is_error: false,
        text: "The file has been updated.",
      });
    }

    if (this.triggered(prompt, "SUBAGENT")) {
      const id = `mock-task-${++this.toolCounter}`;
      this.emit({
        type: "tool_use",
        session_id: this.sessionId,
        tool_use_id: id,
        name: "Task",
        summary: JSON.stringify({ subagent_type: "explore", prompt: "find the entry point" }),
      });
      this.emit({
        type: "tool_use",
        session_id: this.sessionId,
        tool_use_id: `${id}-inner`,
        name: "Grep",
        summary: JSON.stringify({ pattern: "main" }),
        parent_tool_use_id: id,
      });
      this.emit({
        type: "stream_delta",
        session_id: this.sessionId,
        text: "Found it in src/main.ts.",
        parent_tool_use_id: id,
      });
      await sleep(120);
      this.emit({
        type: "tool_result",
        session_id: this.sessionId,
        tool_use_id: id,
        is_error: false,
        text: "The entry point is src/main.ts.",
      });
    }

    if (this.triggered(prompt, "TODO")) {
      this.emit({
        type: "todos",
        session_id: this.sessionId,
        items: [
          { content: "Read the protocol", status: "completed" },
          { content: "Wire the events", status: "in_progress" },
          { content: "Render them", status: "pending" },
        ],
      });
    }

    if (this.triggered(prompt, "DENY")) {
      this.emit({
        type: "permission_denied",
        session_id: this.sessionId,
        tool: "Bash",
        reason: "classifier",
        message: "Auto mode refused: rm -rf looks destructive",
      });
    }

    if (this.triggered(prompt, "LIMIT")) {
      this.emit({
        type: "rate_limit",
        session_id: this.sessionId,
        status: "allowed_warning",
        limit_type: "five_hour",
        utilization: 82,
        resets_at: "2026-08-05T12:00:00.000Z",
      });
    }

    if (this.triggered(prompt, "HOOKCHECK")) {
      // Exercises the PreToolUse path: the core decides before the "tool" runs.
      const requestId = `mock-gate-${this.sessionId}-${++this.gateCounter}`;
      const verdict = await new Promise<string>((resolve) => {
        this.pendingGates.set(requestId, resolve);
        this.emit({
          type: "gate_check",
          session_id: this.sessionId,
          request_id: requestId,
          tool: "Bash",
          args: {
            command: 'git push -u origin HEAD && gh pr create --title "Mock PR" --body "Body"',
          },
        });
      });
      this.emit({
        type: "stream_delta",
        session_id: this.sessionId,
        text: `Gate verdict: ${verdict}. `,
      });
    }

    if (this.triggered(prompt, "ESCALATE")) {
      const requestId = `mock-esc-${this.sessionId}-${++this.escalationCounter}`;
      const result = await new Promise<string>((resolve) => {
        this.pendingEscalations.set(requestId, resolve);
        this.emit({
          type: "escalation_request",
          session_id: this.sessionId,
          request_id: requestId,
          question: "Why was the retry limit set to three?",
        });
      });
      this.emit({
        type: "stream_delta",
        session_id: this.sessionId,
        text: `Original agent said: ${result} `,
      });
    }

    if (this.triggered(prompt, "AUTH")) {
      const requestId = `mock-elicit-${this.sessionId}-${++this.dialogCounter}`;
      const answer = await new Promise<string>((resolve) => {
        this.pendingDialogs.set(requestId, resolve);
        this.emit({
          type: "user_dialog_request",
          session_id: this.sessionId,
          request_id: requestId,
          dialog_kind: "elicitation",
          payload: {
            server: "mock-mcp",
            message: "Authorise Maestro to read your mock account.",
            mode: "url",
            url: "https://example.invalid/oauth/mock",
            form: false,
          },
        });
      });
      this.emit({
        type: "stream_delta",
        session_id: this.sessionId,
        text: `Auth verdict: ${answer}. `,
      });
    }

    if (this.triggered(prompt, "PLAN")) {
      const requestId = `mock-plan-${this.sessionId}-${++this.dialogCounter}`;
      const answer = await new Promise<string>((resolve) => {
        this.pendingDialogs.set(requestId, resolve);
        this.emit({
          type: "user_dialog_request",
          session_id: this.sessionId,
          request_id: requestId,
          dialog_kind: "plan_approval",
          payload: {
            plan: "## Mock plan\n\n1. Read the protocol\n2. Wire the events\n3. Render them",
          },
        });
      });
      this.emit({
        type: "stream_delta",
        session_id: this.sessionId,
        text: `Plan verdict: ${answer}. `,
      });
    }

    if (this.triggered(prompt, "REVIEW_COMMENTS")) {
      const requestId = `mock-review-comments-${this.sessionId}-${++this.dialogCounter}`;
      const answer = await new Promise<string>((resolve) => {
        this.pendingDialogs.set(requestId, resolve);
        this.emit({
          type: "user_dialog_request",
          session_id: this.sessionId,
          request_id: requestId,
          dialog_kind: "review_comments",
          payload: {
            pr: 42,
            summary: "One new finding, one reply to an existing comment.",
            comments: [
              {
                path: "src/lib.rs",
                line: 17,
                body: "This could overflow on a very large input — worth a bounds check.",
              },
              {
                path: "src/retry.rs",
                line: 8,
                body: "Good catch — fixed by capping the backoff at 30s.",
                in_reply_to: 501,
              },
            ],
          },
        });
      });
      this.emit({
        type: "stream_delta",
        session_id: this.sessionId,
        text: `Review comments verdict: ${answer}. `,
      });
    }

    if (this.triggered(prompt, "ASK")) {
      const requestId = `mock-dialog-${this.sessionId}-${++this.dialogCounter}`;
      const answer = await new Promise<string>((resolve) => {
        this.pendingDialogs.set(requestId, resolve);
        this.emit({
          type: "user_dialog_request",
          session_id: this.sessionId,
          request_id: requestId,
          dialog_kind: "ask_user_question",
          payload: {
            questions: [
              {
                question: "Which approach should the mock take?",
                header: "Approach",
                multiSelect: false,
                options: [
                  { label: "Rewrite", description: "Start from scratch" },
                  { label: "Patch", description: "Minimal change on top" },
                ],
              },
              {
                question: "Which extras should it include?",
                header: "Extras",
                multiSelect: true,
                options: [
                  { label: "Tests", description: "Add unit tests" },
                  { label: "Docs", description: "Update the docs" },
                  { label: "Bench", description: "Add a benchmark" },
                ],
              },
            ],
          },
        });
      });
      this.emit({
        type: "stream_delta",
        session_id: this.sessionId,
        text: `You picked: ${answer}. `,
      });
    }

    // A review-roadmap prompt (the review-guide template) expects a JSON-only
    // reply; the default echo would hand the parser the template's own example
    // instead. Cover the prompt's changed-file list in two plausible steps.
    let roadmap: string | null = null;
    if (this.triggered(prompt, "roadmap")) {
      // The block is `git diff --stat` output (plus `?? name` untracked
      // lines): take the path before the pipe, drop the summary line.
      const fence = /Changed files:\s*```\s*([\s\S]*?)```/.exec(prompt);
      const files = (fence?.[1] ?? "")
        .split("\n")
        .map((line) => (line.replace(/^\?\?\s+/, "").split("|")[0] ?? "").trim())
        .filter((path) => path.length > 0 && !/\d+ files? changed/.test(path));
      if (files.length > 0) {
        const core = files.slice(0, Math.ceil(files.length / 2));
        const rest = files.slice(core.length);
        roadmap = JSON.stringify({
          steps: [
            {
              title: "Read the core change",
              why: "Mock: the heart of this diff.",
              files: core,
              category: "core-logic",
            },
            ...(rest.length > 0
              ? [
                  {
                    title: "Skim the supporting edits",
                    why: "Mock: wiring around the core.",
                    files: rest,
                    category: "supporting",
                  },
                ]
              : []),
          ],
        });
      }
    }

    const settings = [
      this.model && `model=${this.model}`,
      this.effort && `effort=${this.effort}`,
      this.thinking && `thinking=${this.thinking}`,
      this.permissionMode && `permissions=${this.permissionMode}`,
    ]
      .filter(Boolean)
      .join(" ");
    const words =
      roadmap !== null
        ? [roadmap]
        : `Mock reply${settings ? ` (${settings})` : ""} to: ${prompt}`.split(" ");
    for (const word of words) {
      if (this.interrupted || this.closed) break;
      this.emit({ type: "stream_delta", session_id: this.sessionId, text: word + " " });
      await sleep(60);
    }

    if (this.closed) return;
    this.turns += 1;
    this.emit({
      type: "usage",
      session_id: this.sessionId,
      total_cost_usd: 0.0123 * this.turns,
      num_turns: this.turns,
      input_tokens: 1200 * this.turns,
      output_tokens: 300 * this.turns,
      context_tokens: 18000 * this.turns,
      context_max_tokens: 200000,
      context_percent: Math.min(100, 9 * this.turns),
    });
    this.emit({
      type: "result",
      session_id: this.sessionId,
      subtype: this.interrupted ? "interrupted" : "success",
      is_error: false,
      duration_ms: 60 * words.length,
      total_cost_usd: 0,
      num_turns: 1,
    });
    this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
  }

  async interrupt(): Promise<void> {
    this.interrupted = true;
    this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
  }

  close(): void {
    this.closed = true;
    for (const [, resolve] of this.pending) resolve(false);
    this.pending.clear();
    for (const [, resolve] of this.pendingDialogs) resolve("(session closed)");
    this.pendingDialogs.clear();
    for (const [, resolve] of this.pendingEscalations) resolve("(session closed)");
    this.pendingEscalations.clear();
    for (const [, resolve] of this.pendingGates) resolve("pass");
    this.pendingGates.clear();
    this.emit({ type: "session_closed", session_id: this.sessionId, reason: "closed" });
    this.onEnd(this.sessionId);
  }

  respondPermission(requestId: string, allow: boolean): boolean {
    const resolve = this.pending.get(requestId);
    if (!resolve) return false;
    this.pending.delete(requestId);
    resolve(allow);
    return true;
  }

  respondGate(
    requestId: string,
    decision: string,
    _updatedArgs?: Record<string, unknown>,
    message?: string,
  ): boolean {
    const resolve = this.pendingGates.get(requestId);
    if (!resolve) return false;
    this.pendingGates.delete(requestId);
    resolve(`${decision}${message ? `: ${message}` : ""}`);
    return true;
  }

  respondEscalation(requestId: string, result: string): boolean {
    const resolve = this.pendingEscalations.get(requestId);
    if (!resolve) return false;
    this.pendingEscalations.delete(requestId);
    resolve(result);
    return true;
  }

  respondUserDialog(
    requestId: string,
    behavior: "completed" | "cancelled",
    result?: DialogAnswer,
  ): boolean {
    const resolve = this.pendingDialogs.get(requestId);
    if (!resolve) return false;
    this.pendingDialogs.delete(requestId);
    if (behavior === "cancelled") {
      resolve("(cancelled)");
      return true;
    }
    if (result?.approved !== undefined) {
      resolve(result.approved ? "approved" : `rejected: ${result.feedback?.trim() ?? ""}`);
      return true;
    }
    if (result?.feedback?.trim()) {
      resolve(`clarify: ${result.feedback.trim()}`);
      return true;
    }
    const answers = result?.answers ?? {};
    const text = Object.values(answers).join("; ");
    resolve(text || "(no answer)");
    return true;
  }

  // Runtime switches are echoed into the next reply so a test can see they landed.
  async setModel(model: string): Promise<void> {
    this.model = model;
  }

  async setEffort(effort: string): Promise<void> {
    this.effort = effort;
  }

  async setThinking(thinking: string): Promise<void> {
    this.thinking = thinking;
  }

  async mcpAction(server: string, action: string): Promise<void> {
    // Echo the new state so the UI round trip can be tested without a real server.
    this.emit({
      type: "mcp_servers",
      session_id: this.sessionId,
      servers: [
        {
          name: server,
          status: action === "disable" ? "disabled" : "connected",
          tool_count: action === "disable" ? 0 : 3,
          detail: `after ${action}`,
        },
      ],
    });
  }

  async setPermissionMode(mode: string): Promise<void> {
    this.permissionMode = mode;
  }
}
