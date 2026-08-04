// Mock session runner (MAESTRO_SIDECAR_MOCK=1): scripted events, no SDK, no API usage.
// Used by Rust integration tests and for manual UI testing of the whole pipeline.
//
// Prompt keywords: "PERMISSION" pauses on a chat permission request; "GATE" asks to run
// a push+PR command (exercises the T7 approval dialog); "ASK" raises a question dialog
// (AskUserQuestion path); "CRASH" kills the whole sidecar process (supervisor recovery
// testing).

import type { DialogAnswer, SessionHandle, SidecarEvent, SpawnRequest } from "./protocol.js";

type Emit = (event: SidecarEvent) => void;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export class MockSession implements SessionHandle {
  private interrupted = false;
  private closed = false;
  private permissionCounter = 0;
  private dialogCounter = 0;
  private model = "";
  private effort = "";
  private permissionMode = "";
  private readonly pending = new Map<string, (allow: boolean) => void>();
  private readonly pendingDialogs = new Map<string, (answer: string) => void>();

  constructor(
    private readonly sessionId: string,
    private readonly emit: Emit,
    private readonly onEnd: (sessionId: string) => void,
  ) {}

  async spawn(req: SpawnRequest): Promise<void> {
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
    if (req.prompt.trim().length > 0) {
      this.send(req.prompt);
    } else {
      this.emit({ type: "status", session_id: this.sessionId, status: "awaiting_input" });
    }
  }

  send(prompt: string): void {
    void this.runTurn(prompt);
  }

  private async runTurn(prompt: string): Promise<void> {
    this.interrupted = false;
    this.emit({ type: "status", session_id: this.sessionId, status: "streaming" });

    if (prompt.includes("CRASH")) {
      await sleep(20);
      process.exit(13);
    }

    if (prompt.includes("PERMISSION") || prompt.includes("GATE")) {
      const requestId = `mock-perm-${this.sessionId}-${++this.permissionCounter}`;
      // "GATE" exercises the T7 approval path: a push+PR command the gate must
      // intercept with editable PR fields. Plain "PERMISSION" stays a chat prompt.
      const command = prompt.includes("GATE")
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
        name: "Bash",
        summary: allowed ? JSON.stringify({ command }) : "(denied)",
      });
    }

    if (prompt.includes("ASK")) {
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

    const settings = [
      this.model && `model=${this.model}`,
      this.effort && `effort=${this.effort}`,
      this.permissionMode && `permissions=${this.permissionMode}`,
    ]
      .filter(Boolean)
      .join(" ");
    const words = `Mock reply${settings ? ` (${settings})` : ""} to: ${prompt}`.split(" ");
    for (const word of words) {
      if (this.interrupted || this.closed) break;
      this.emit({ type: "stream_delta", session_id: this.sessionId, text: word + " " });
      await sleep(60);
    }

    if (this.closed) return;
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

  async setPermissionMode(mode: string): Promise<void> {
    this.permissionMode = mode;
  }
}
