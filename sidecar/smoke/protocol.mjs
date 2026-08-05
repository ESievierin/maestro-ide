// Protocol smoke tests: drive the real sidecar process in mock mode and assert that each
// path still round-trips. Unit tests cover the pieces; this covers the wire.
//
//   node smoke/protocol.mjs        (from the sidecar directory, after npm run build)
//
// Every scenario spawns `dist/main.js` with MAESTRO_SIDECAR_MOCK=1, sends requests, and
// checks the events that come back. No SDK, no tokens, no network.

import { spawn } from "node:child_process";
import readline from "node:readline";
import { fileURLToPath } from "node:url";
import path from "node:path";

const MAIN = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "dist", "main.js");
const TIMEOUT_MS = 30_000;

/**
 * Run one scenario. `onEvent(event, ctx)` handles each event; the scenario ends when it
 * calls `ctx.finish()`, and fails on timeout or a thrown assertion.
 */
async function scenario(name, prompt, onEvent, { spawnExtra = {}, allowNack = false } = {}) {
  const child = spawn(process.execPath, [MAIN], {
    env: { ...process.env, MAESTRO_SIDECAR_MOCK: "1" },
    stdio: ["pipe", "pipe", "inherit"],
  });
  const rl = readline.createInterface({ input: child.stdout });
  let nextId = 1;
  const seen = [];

  try {
    await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error(`timed out; events seen: ${seen.join(", ")}`)),
        TIMEOUT_MS,
      );
      const ctx = {
        send: (request) => child.stdin.write(JSON.stringify({ id: nextId++, ...request }) + "\n"),
        finish: () => {
          clearTimeout(timer);
          resolve();
        },
        fail: (message) => {
          clearTimeout(timer);
          reject(new Error(message));
        },
        seen,
      };
      rl.on("line", (line) => {
        let event;
        try {
          event = JSON.parse(line);
        } catch {
          return;
        }
        seen.push(event.type);
        if (event.type === "ready") {
          ctx.send({
            type: "spawn",
            session_id: "s1",
            cwd: ".",
            prompt,
            session_type: "manual",
            ...spawnExtra,
          });
          // Scenarios see `ready` too — the version check is one of them.
        }
        // A nack is a bug in every scenario but the one that asks for one.
        if (event.type === "ack" && event.ok === false && !allowNack) {
          ctx.fail(`request ${event.id} was nacked: ${event.error}`);
          return;
        }
        try {
          onEvent(event, ctx);
        } catch (err) {
          ctx.fail(String(err));
        }
      });
    });
    console.log(`ok   ${name}`);
    return true;
  } catch (err) {
    console.log(`FAIL ${name}: ${err.message}`);
    return false;
  } finally {
    child.kill();
  }
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const scenarios = [
  // The protocol version both sides agree on. A mismatch here means a stale dist.
  () =>
    scenario("ready reports the protocol version", "hello", (event, ctx) => {
      if (event.type === "ready") {
        assert(typeof event.protocol_version === "number", "no protocol version");
        assert(event.protocol_version >= 5, `expected v5+, got v${event.protocol_version}`);
        ctx.finish();
      }
    }),

  // S3 Tier 1: the question dialog and its answer.
  () => {
    let answered = false;
    return scenario("ask_user_question round trip", "please ASK me", (event, ctx) => {
      if (event.type === "user_dialog_request") {
        assert(event.dialog_kind === "ask_user_question", `kind was ${event.dialog_kind}`);
        assert(event.payload.questions.length === 2, "expected two questions");
        answered = true;
        ctx.send({
          type: "user_dialog_response",
          request_id: event.request_id,
          behavior: "completed",
          result: { answers: { [event.payload.questions[0].question]: "Patch" } },
        });
      }
      if (event.type === "result") {
        assert(answered, "the dialog never arrived");
        ctx.finish();
      }
    });
  },

  // S3 Tier 1: runtime switches reach a live session.
  () => {
    let turn = 0;
    let echo = "";
    return scenario("runtime model/effort/thinking/permissions", "hello", (event, ctx) => {
      if (event.type === "stream_delta" && turn === 1) echo += event.text;
      if (event.type !== "result") return;
      turn += 1;
      if (turn === 1) {
        ctx.send({ type: "set_model", session_id: "s1", model: "claude-opus-5" });
        ctx.send({ type: "set_effort", session_id: "s1", effort: "high" });
        ctx.send({ type: "set_thinking", session_id: "s1", thinking: "16000" });
        ctx.send({ type: "set_permission_mode", session_id: "s1", permission_mode: "plan" });
        ctx.send({ type: "send", session_id: "s1", prompt: "again" });
        return;
      }
      for (const expected of [
        "model=claude-opus-5",
        "effort=high",
        "thinking=16000",
        "permissions=plan",
      ]) {
        assert(echo.includes(expected), `missing ${expected} in: ${echo}`);
      }
      ctx.finish();
    });
  },

  // S3 Tier 2: everything the transcript needs.
  () => {
    const seenTypes = new Set();
    let toolResultFor = null;
    let nested = 0;
    return scenario(
      "transcript events (thinking, tools, subagents, todos, usage, limits)",
      "THINK TOOLS SUBAGENT TODO DENY LIMIT",
      (event, ctx) => {
        seenTypes.add(event.type);
        if (event.type === "tool_result") toolResultFor = event.tool_use_id;
        if (event.type === "tool_use" && event.parent_tool_use_id) nested += 1;
        if (event.type === "stream_delta" && event.parent_tool_use_id) nested += 1;
        if (event.type !== "result") return;
        for (const type of [
          "thinking_delta",
          "tool_use",
          "tool_result",
          "todos",
          "usage",
          "rate_limit",
          "permission_denied",
        ]) {
          assert(seenTypes.has(type), `no ${type} event`);
        }
        assert(toolResultFor, "a tool result must name its call");
        assert(nested >= 2, `expected nested subagent activity, saw ${nested}`);
        ctx.finish();
      },
    );
  },

  // S3 Tier 3: plan review and MCP elicitation, both answered as dialogs.
  () => {
    const kinds = [];
    return scenario("plan review and elicitation dialogs", "make a PLAN and AUTH", (event, ctx) => {
      if (event.type === "user_dialog_request") {
        kinds.push(event.dialog_kind);
        ctx.send({
          type: "user_dialog_response",
          request_id: event.request_id,
          behavior: "completed",
          result: { approved: true },
        });
      }
      if (event.type === "result") {
        assert(kinds.includes("plan_approval"), `kinds: ${kinds}`);
        assert(kinds.includes("elicitation"), `kinds: ${kinds}`);
        ctx.finish();
      }
    });
  },

  // S3 Tier 3: agents and MCP servers are discovered, and an action reports back.
  () => {
    let agents = 0;
    let acted = false;
    let after = null;
    return scenario("agents, MCP status and an MCP action", "hello", (event, ctx) => {
      if (event.type === "agents") agents = event.agents.length;
      if (event.type === "mcp_servers") {
        if (!acted) {
          acted = true;
          ctx.send({
            type: "mcp_action",
            session_id: "s1",
            server: "broken-mcp",
            action: "reconnect",
          });
        } else {
          after = event.servers[0];
        }
      }
      if (event.type === "result" && after) {
        assert(agents >= 2, `expected agent profiles, got ${agents}`);
        assert(after.detail === "after reconnect", `unexpected state: ${JSON.stringify(after)}`);
        ctx.finish();
      }
    });
  },

  // S2-T2: the escalation tool's request/response pair.
  () => {
    let question = null;
    let echo = "";
    return scenario("ask_original_agent round trip", "please ESCALATE this", (event, ctx) => {
      if (event.type === "escalation_request") {
        question = event.question;
        ctx.send({
          type: "escalation_response",
          request_id: event.request_id,
          result: "Three retries matched the upstream timeout budget.",
        });
      }
      if (event.type === "stream_delta") echo += event.text;
      if (event.type === "result") {
        assert(question, "no escalation request");
        assert(echo.includes("Three retries"), `answer missing from: ${echo}`);
        ctx.finish();
      }
    });
  },

  // The gate's own channel: a verdict before the call runs, in any permission mode.
  () => {
    let checked = null;
    let echo = "";
    return scenario("PreToolUse gate decision", "HOOKCHECK now", (event, ctx) => {
      if (event.type === "gate_check") {
        checked = event.args.command;
        ctx.send({
          type: "gate_decision",
          request_id: event.request_id,
          decision: "deny",
          message: "Pushes are gated.",
        });
      }
      if (event.type === "stream_delta") echo += event.text;
      if (event.type === "result") {
        assert(checked && checked.includes("git push"), `gate saw: ${checked}`);
        assert(echo.includes("deny"), `verdict missing from: ${echo}`);
        ctx.finish();
      }
    });
  },

  // Attachments ride along with a prompt.
  () => {
    let turn = 0;
    let echo = "";
    return scenario("image attachments", "hello", (event, ctx) => {
      if (event.type === "stream_delta" && turn === 1) echo += event.text;
      if (event.type !== "result") return;
      turn += 1;
      if (turn === 1) {
        ctx.send({
          type: "send",
          session_id: "s1",
          prompt: "what is in this screenshot?",
          attachments: [{ media_type: "image/png", data: "iVBORw0KGgo=" }],
        });
        return;
      }
      assert(echo.includes("+1 image"), `attachment not carried: ${echo}`);
      ctx.finish();
    });
  },

  // An unknown request type must be nacked, not ignored (the compatibility rule).
  () =>
    scenario(
      "unknown requests are nacked",
      "hello",
      (event, ctx) => {
        if (event.type === "ready") {
          // `scenario` already sent the spawn; follow it with nonsense.
          ctx.send({ type: "no_such_request" });
          return;
        }
        if (event.type === "ack" && event.ok === false) {
          assert(/unknown/i.test(event.error ?? ""), `unexpected error: ${event.error}`);
          ctx.finish();
        }
      },
      { allowNack: true },
    ),
];

let failures = 0;
for (const run of scenarios) {
  // Sequential on purpose: each scenario owns a process and the output stays readable.
  const ok = await run();
  if (!ok) failures += 1;
}

console.log(failures === 0 ? "\nall protocol smoke scenarios passed" : `\n${failures} failed`);
process.exit(failures === 0 ? 0 : 1);
