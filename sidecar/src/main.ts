// MaestroIDE sidecar — executes Claude Agent SDK sessions on behalf of the Rust core.
//
// NDJSON over stdio, protocol v1 (see protocol.ts). The Rust core owns all state;
// this process only runs sessions and streams events back.

import * as readline from "node:readline";

import { AgentSession } from "./engine.js";
import { reportModels } from "./models.js";
import { MockSession } from "./mock.js";
import {
  PROTOCOL_VERSION,
  writeEvent,
  type SessionHandle,
  type SidecarRequest,
} from "./protocol.js";

const useMock = process.env.MAESTRO_SIDECAR_MOCK === "1";
const sessions = new Map<string, SessionHandle>();

function createSession(sessionId: string): SessionHandle {
  const onEnd = (id: string) => {
    sessions.delete(id);
  };
  return useMock
    ? new MockSession(sessionId, writeEvent, onEnd)
    : new AgentSession(sessionId, writeEvent, onEnd);
}

function ack(id: number, ok: boolean, error?: string): void {
  writeEvent(error !== undefined ? { type: "ack", id, ok, error } : { type: "ack", id, ok });
}

async function dispatch(request: SidecarRequest): Promise<void> {
  switch (request.type) {
    case "spawn": {
      if (sessions.has(request.session_id)) {
        ack(request.id, false, `session already exists: ${request.session_id}`);
        return;
      }
      const session = createSession(request.session_id);
      sessions.set(request.session_id, session);
      try {
        await session.spawn(request);
        ack(request.id, true);
      } catch (err) {
        sessions.delete(request.session_id);
        ack(request.id, false, String(err));
      }
      return;
    }
    case "send": {
      const session = sessions.get(request.session_id);
      if (!session) {
        ack(request.id, false, `unknown session: ${request.session_id}`);
        return;
      }
      session.send(request.prompt, request.attachments);
      ack(request.id, true);
      return;
    }
    case "interrupt": {
      const session = sessions.get(request.session_id);
      if (!session) {
        ack(request.id, false, `unknown session: ${request.session_id}`);
        return;
      }
      await session.interrupt();
      ack(request.id, true);
      return;
    }
    case "close": {
      const session = sessions.get(request.session_id);
      if (!session) {
        ack(request.id, false, `unknown session: ${request.session_id}`);
        return;
      }
      session.close();
      sessions.delete(request.session_id);
      ack(request.id, true);
      return;
    }
    case "permission_response": {
      for (const session of sessions.values()) {
        if (
          session.respondPermission(
            request.request_id,
            request.allow,
            request.updated_args,
            request.message,
          )
        ) {
          ack(request.id, true);
          return;
        }
      }
      ack(request.id, false, `unknown permission request: ${request.request_id}`);
      return;
    }
    case "set_model":
    case "set_effort":
    case "set_thinking":
    case "set_permission_mode": {
      const session = sessions.get(request.session_id);
      if (!session) {
        ack(request.id, false, `unknown session: ${request.session_id}`);
        return;
      }
      try {
        if (request.type === "set_model") await session.setModel(request.model);
        else if (request.type === "set_effort") await session.setEffort(request.effort);
        else if (request.type === "set_thinking") await session.setThinking(request.thinking);
        else await session.setPermissionMode(request.permission_mode);
        ack(request.id, true);
      } catch (err) {
        ack(request.id, false, String(err));
      }
      return;
    }
    case "mcp_action": {
      const session = sessions.get(request.session_id);
      if (!session) {
        ack(request.id, false, `unknown session: ${request.session_id}`);
        return;
      }
      try {
        await session.mcpAction(request.server, request.action);
        ack(request.id, true);
      } catch (err) {
        ack(request.id, false, String(err));
      }
      return;
    }
    case "user_dialog_response": {
      for (const session of sessions.values()) {
        if (session.respondUserDialog(request.request_id, request.behavior, request.result)) {
          ack(request.id, true);
          return;
        }
      }
      ack(request.id, false, `unknown dialog request: ${request.request_id}`);
      return;
    }
    case "list_models": {
      if (useMock) {
        writeEvent({
          type: "models",
          session_id: "",
          models: [
            { id: "mock-model", display_name: "Mock Model" },
            { id: "claude-fable-5", display_name: "Fable (mock)" },
          ],
        });
      } else {
        await reportModels(request.cwd);
      }
      ack(request.id, true);
      return;
    }
    case "shutdown": {
      for (const session of sessions.values()) {
        session.close();
      }
      sessions.clear();
      ack(request.id, true);
      process.exit(0);
    }
  }
}

const rl = readline.createInterface({ input: process.stdin, terminal: false });

/** In-flight dispatches, so a closing stdin does not kill work mid-answer. */
let inFlight = 0;
let stdinClosed = false;

function exitWhenIdle(): void {
  if (stdinClosed && inFlight === 0) {
    for (const session of sessions.values()) {
      session.close();
    }
    process.exit(0);
  }
}

rl.on("line", (line) => {
  const trimmed = line.trim();
  if (trimmed.length === 0) return;

  let request: SidecarRequest;
  try {
    request = JSON.parse(trimmed) as SidecarRequest;
  } catch {
    writeEvent({ type: "error", message: `invalid JSON request: ${trimmed.slice(0, 200)}` });
    return;
  }

  inFlight += 1;
  dispatch(request)
    .catch((err) => {
      writeEvent({ type: "error", message: `dispatch failed: ${String(err)}` });
    })
    .finally(() => {
      inFlight -= 1;
      exitWhenIdle();
    });
});

rl.on("close", () => {
  // Core closed our stdin: finish what is in flight, then shut down cleanly.
  stdinClosed = true;
  exitWhenIdle();
});

writeEvent({ type: "ready", protocol_version: PROTOCOL_VERSION });
