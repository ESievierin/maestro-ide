import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import { Icon, StatusDot } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { SelectMenu, type SelectMenuOption } from "../components/SelectMenu";
import remarkGfm from "remark-gfm";
import { activeSessionCount, useSessions } from "../state/sessions";
import type {
  AgentInfo,
  Attachment,
  CommandInfo,
  McpServerInfo,
  ModelOption,
  RateLimitInfo,
  Session,
  SessionUsage,
  TodoItem,
  ToolChild,
  TranscriptItem,
} from "../types/sessions";
import {
  EFFORTS,
  isTerminalStatus,
  PERMISSION_MODE_LABELS,
  PERMISSION_MODES,
  MAX_ATTACHMENT_BYTES,
  READ_ONLY_MODE,
  SESSION_TYPES,
  THINKING_LABELS,
  THINKING_OPTIONS,
  TODO_STATUS_ORDER,
} from "../types/sessions";
import type { WorktreeInfo } from "../types/worktrees";
import { QuestionDialog } from "./QuestionDialog";

const DEFAULT_OPTION: SelectMenuOption = { value: "", label: "Default" };

const SESSION_TYPE_SHORT_LABELS: Record<string, string> = {
  manual: "Manual",
  research: "Research",
  implementation: "Implementation",
  review_fix: "Review fix",
};
const SESSION_TYPE_DESCRIPTIONS: Record<string, string> = {
  manual: "No extra behaviour",
  research: "Read-only work",
  implementation: "Writes TASK_NOTES.md on close",
  review_fix: "Can ask the original agent",
};
const SESSION_TYPE_MENU_OPTIONS: SelectMenuOption[] = SESSION_TYPES.map((t) => ({
  value: t,
  label: SESSION_TYPE_SHORT_LABELS[t] ?? t,
  description: SESSION_TYPE_DESCRIPTIONS[t],
}));

const EFFORT_DESCRIPTIONS: Record<string, string> = {
  low: "Fast, spends little on reasoning",
  medium: "Balanced — the usual choice",
  high: "Slower, more thorough",
  xhigh: "Extended reasoning for hard problems",
  max: "Maximum depth, highest cost",
};
const EFFORT_MENU_OPTIONS: SelectMenuOption[] = EFFORTS.map((e) => ({
  value: e,
  label: e,
  description: EFFORT_DESCRIPTIONS[e],
}));

const PERMISSION_MODE_SHORT_LABELS: Record<string, string> = {
  default: "Default",
  acceptEdits: "Accept edits",
  auto: "Auto",
  plan: "Plan",
};
const PERMISSION_MODE_DESCRIPTIONS: Record<string, string> = {
  default: "Asks before every risky action",
  acceptEdits: "Edits auto-approved, commands still ask",
  auto: "A classifier answers ordinary prompts — the gate still applies",
  plan: "Read-only: plans first, writes nothing",
};
const PERMISSION_MENU_OPTIONS: SelectMenuOption[] = PERMISSION_MODES.map((m) => ({
  value: m,
  label: PERMISSION_MODE_SHORT_LABELS[m] ?? m,
  description: PERMISSION_MODE_DESCRIPTIONS[m],
}));

const THINKING_SHORT_LABELS: Record<string, string> = {
  default: "CLI default",
  off: "Off",
  "4000": "4k budget",
  "16000": "16k budget",
  "32000": "32k budget",
};
const THINKING_MENU_OPTIONS: SelectMenuOption[] = THINKING_OPTIONS.map((t) => ({
  value: t,
  label: THINKING_SHORT_LABELS[t] ?? t,
}));

/** Commands handled by Maestro itself, merged into the autocomplete list. */
const LOCAL_COMMANDS: (CommandInfo & { local: true })[] = [
  {
    name: "resume",
    description: "Maestro: continue a finished session of this worktree",
    argument_hint: "",
    local: true,
  },
  {
    name: "model",
    description: "Maestro: switch this session's model (blank = default)",
    argument_hint: "<model-id>",
    local: true,
  },
  {
    name: "effort",
    description: "Maestro: switch this session's reasoning effort",
    argument_hint: "<low|medium|high|xhigh|max>",
    local: true,
  },
  {
    name: "permissions",
    description: "Maestro: switch this session's permission mode",
    argument_hint: "<default|acceptEdits|auto|plan>",
    local: true,
  },
  {
    name: "thinking",
    description: "Maestro: how much this session may think (a budget makes it visible)",
    argument_hint: "<default|off|4000|16000|32000>",
    local: true,
  },
];

/** A local command whose argument Maestro can complete, with the values to offer. */
function localArgumentValues(
  command: string,
  models: ModelOption[],
): { value: string; label: string }[] {
  switch (command) {
    case "model":
      return models.map((m) => ({ value: m.id, label: m.display_name }));
    case "effort":
      return EFFORTS.map((e) => ({ value: e, label: e }));
    case "permissions":
      return PERMISSION_MODES.map((m) => ({ value: m, label: PERMISSION_MODE_LABELS[m] ?? m }));
    case "thinking":
      return THINKING_OPTIONS.map((t) => ({ value: t, label: THINKING_LABELS[t] ?? t }));
    default:
      return [];
  }
}

function StatusPill({ status }: { status: Session["status"] }) {
  const live = status === "streaming" || status === "spawning";
  return (
    <span className={`pill pill-${status}`}>
      <StatusDot tone={status} pulse={live} />
      {status.replace("_", " ")}
    </span>
  );
}

function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
    </div>
  );
}

/** A block streaming in must never sit mid-reveal for longer than this. */
const STREAM_REVEAL_CAP_MS = 3000;
/** Reveal speed floor — keeps a trickle of new text feeling like typing, not a stall. */
const STREAM_REVEAL_MIN_CHARS_PER_SEC = 60;

/**
 * Smooths a growing (streamed) string into a typewriter-style reveal, sped up
 * for big bursts so any backlog clears within `STREAM_REVEAL_CAP_MS` — a huge
 * chunk that lands in one event never sits half-shown for seconds, and a
 * trickle of tokens still reads as a pleasant, steady type-in rather than
 * popping in all at once. Text that arrives already complete (a resumed
 * session's history, a tab switch) renders instantly — only *new* growth on an
 * already-mounted block animates.
 */
function useStreamedReveal(target: string): string {
  const [visible, setVisible] = useState(target);
  const state = useRef({ visibleLen: target.length, deadline: 0 });

  useEffect(() => {
    const s = state.current;
    if (target.length <= s.visibleLen) {
      // Nothing new (or the block reset, e.g. a fresh session) — show it as-is.
      s.visibleLen = target.length;
      s.deadline = 0;
      setVisible(target);
      return;
    }
    if (s.deadline === 0) {
      s.deadline = performance.now() + STREAM_REVEAL_CAP_MS;
    }

    let raf = 0;
    let last = performance.now();
    const tick = (now: number) => {
      const dt = (now - last) / 1000;
      last = now;
      const backlog = target.length - s.visibleLen;
      if (backlog <= 0) {
        s.deadline = 0;
        return;
      }
      const secondsLeft = Math.max(0.05, (s.deadline - now) / 1000);
      const rate = Math.max(STREAM_REVEAL_MIN_CHARS_PER_SEC, backlog / secondsLeft);
      s.visibleLen = Math.min(target.length, s.visibleLen + rate * dt);
      setVisible(target.slice(0, Math.floor(s.visibleLen)));
      if (s.visibleLen < target.length) {
        raf = requestAnimationFrame(tick);
      } else {
        s.deadline = 0;
      }
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [target]);

  return visible;
}

/** The agent's reply text, revealed with {@link useStreamedReveal}. */
function StreamedMarkdown({ text }: { text: string }) {
  return <Markdown text={useStreamedReveal(text)} />;
}

function SubagentChild({ child }: { child: ToolChild }) {
  if (child.kind === "tool_use") {
    return (
      <div className="t-sub-tool">
        <span className="t-tool-name">{child.name}</span>
        <span className="t-tool-preview">{child.summary.slice(0, 70)}</span>
      </div>
    );
  }
  if (child.kind === "thinking") {
    return <div className="t-sub-thinking">{child.text}</div>;
  }
  return (
    <div className="t-sub-text">
      <Markdown text={child.text} />
    </div>
  );
}

/**
 * A tool call, its result, and — for `Task` — everything the subagent did inside it.
 * Folded by default: the answer is what the user reads, this is the evidence behind it.
 */
function ToolUseEntry({ item }: { item: Extract<TranscriptItem, { kind: "tool_use" }> }) {
  const { name, summary, result, children } = item;
  const state = result ? (result.isError ? "error" : "done") : "running";
  return (
    <details className={`t-tool t-tool-${state}`}>
      <summary>
        <span className="t-tool-name">{name}</span>
        <span className="t-tool-preview">{summary.slice(0, 80)}</span>
        {state === "running" && <Icon name="spinner" spin />}
        {state === "error" && <Icon name="alert" />}
        {children.length > 0 && <span className="t-tool-badge">{children.length}</span>}
      </summary>
      <code>{summary}</code>
      {result && (
        <pre className={`t-tool-result ${result.isError ? "error" : ""}`}>
          {result.text || "(no output)"}
        </pre>
      )}
      {children.length > 0 && (
        <div className="t-subagent">
          {children.map((child, i) => (
            <SubagentChild key={i} child={child} />
          ))}
        </div>
      )}
    </details>
  );
}

/** The agent's reasoning. Folded away, because it is not the answer. */
function ThinkingEntry({ text }: { text: string }) {
  return (
    <details className="t-thinking">
      <summary>
        <Icon name="spinner" /> thinking
        <span className="t-tool-preview">{text.slice(0, 70)}</span>
      </summary>
      <div className="t-thinking-body">{text}</div>
    </details>
  );
}

/** A tool call refused before the user ever saw it (classifier, deny rule, `dontAsk`). */
function DeniedEntry({ item }: { item: Extract<TranscriptItem, { kind: "denied" }> }) {
  return (
    <div className="t-denied">
      <div className="t-denied-title">
        <Icon name="shield" /> {item.tool} denied automatically ({item.reason})
      </div>
      <div className="t-denied-message">{item.message}</div>
    </div>
  );
}

/** The agent's checklist. Shown above the input, where the next step belongs. */
function TodoList({ items }: { items: TodoItem[] }) {
  const sorted = useMemo(
    () =>
      items
        .map((t, i) => ({ t, i }))
        .sort(
          (a, b) =>
            (TODO_STATUS_ORDER[a.t.status] ?? 1) - (TODO_STATUS_ORDER[b.t.status] ?? 1) ||
            a.i - b.i,
        )
        .map((e) => e.t),
    [items],
  );
  const done = items.filter((t) => t.status === "completed").length;

  return (
    <details className="todo-list" open>
      <summary>
        <Icon name="check" /> Plan
        <span className="count">
          {done}/{items.length}
        </span>
      </summary>
      <ul>
        {sorted.map((todo, i) => (
          <li key={i} className={`todo todo-${todo.status}`}>
            <Icon
              name={
                todo.status === "completed"
                  ? "check"
                  : todo.status === "in_progress"
                    ? "play"
                    : "circle"
              }
            />
            {todo.content}
          </li>
        ))}
      </ul>
    </details>
  );
}

/**
 * What this session can reach: the subagent profiles it may delegate to and its MCP
 * servers. A single icon button that pops the detail open, rather than a permanent
 * full-width bar — a failed or unauthenticated server still needs to be impossible
 * to miss, so it keeps a badge on the trigger even while collapsed.
 */
function SessionCapabilities({
  session,
  agents,
  servers,
}: {
  session: Session;
  agents: AgentInfo[];
  servers: McpServerInfo[];
}) {
  const mcpAction = useSessions((s) => s.mcpAction);
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const broken = servers.filter((s) => s.status !== "connected" && s.status !== "disabled");

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onPointerDown);
    return () => document.removeEventListener("mousedown", onPointerDown);
  }, [open]);

  if (agents.length === 0 && servers.length === 0) return null;

  return (
    <div className={`capabilities-popover ${open ? "open" : ""}`} ref={rootRef}>
      <button
        type="button"
        className={`small ghost ${broken.length > 0 ? "attention-alert" : ""}`}
        onClick={() => setOpen((o) => !o)}
        title="Capabilities: agents and MCP servers this session can reach"
      >
        <Icon name="shield" />
        {agents.length} · {servers.length}
        {broken.length > 0 && <span className="count-pill">{broken.length}</span>}
      </button>
      {open && (
        <div className="capabilities-panel">
          {servers.length > 0 && (
            <ul className="cap-list">
              {servers.map((server) => (
                <li key={server.name}>
                  <span className="cap-name">
                    <StatusDot tone={server.status === "connected" ? "streaming" : "failed"} />
                    {server.name}
                  </span>
                  <span className="ac-desc">
                    {server.status}
                    {server.tool_count > 0 && ` · ${server.tool_count} tools`}
                    {server.detail && ` · ${server.detail}`}
                  </span>
                  <span className="cap-actions">
                    <button
                      className="small"
                      onClick={() => void mcpAction(session.id, server.name, "reconnect")}
                    >
                      <Icon name="refresh" /> Reconnect
                    </button>
                    <button
                      className="small"
                      onClick={() =>
                        void mcpAction(
                          session.id,
                          server.name,
                          server.status === "disabled" ? "enable" : "disable",
                        )
                      }
                    >
                      {server.status === "disabled" ? "Enable" : "Disable"}
                    </button>
                  </span>
                </li>
              ))}
            </ul>
          )}

          {agents.length > 0 && (
            <ul className="cap-list">
              {agents.map((agent) => (
                <li key={agent.name}>
                  <span className="cap-name">{agent.name}</span>
                  <span className="ac-desc">
                    {agent.description}
                    {agent.model && ` · ${agent.model}`}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

/** Cost and context pressure of the selected session. */
function UsageMeter({ usage }: { usage: SessionUsage }) {
  const percent = usage.contextPercent;
  const tooltip = [
    usage.turns !== undefined && `${usage.turns} turns`,
    usage.inputTokens !== undefined && `${usage.inputTokens.toLocaleString()} in`,
    usage.outputTokens !== undefined && `${usage.outputTokens.toLocaleString()} out`,
    usage.contextTokens !== undefined &&
      usage.contextMaxTokens !== undefined &&
      `context ${usage.contextTokens.toLocaleString()}/${usage.contextMaxTokens.toLocaleString()}`,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <span className="usage-meter" title={tooltip || "no usage reported yet"}>
      {usage.costUsd !== undefined && (
        <span className="usage-cost">${usage.costUsd.toFixed(3)}</span>
      )}
      {percent !== undefined && (
        <span className={`usage-context ${percent >= 80 ? "warn" : ""}`}>
          <span className="usage-bar">
            <span className="usage-fill" style={{ width: `${Math.min(100, percent)}%` }} />
          </span>
          {Math.round(percent)}%
        </span>
      )}
    </span>
  );
}

/** Account-wide quota state. Only rendered once the CLI has something to say. */
function RateLimitPill({ info }: { info: RateLimitInfo }) {
  if (info.status === "allowed") return null;
  const resets = info.resetsAt ? new Date(info.resetsAt).toLocaleTimeString() : null;
  return (
    <span
      className={`pill ${info.status === "rejected" ? "pill-failed" : "pill-warn"}`}
      title={`${info.limitType ?? "quota"}${resets ? ` · resets ${resets}` : ""}`}
    >
      <Icon name="alert" />
      {info.status === "rejected" ? "rate limited" : "quota"}
      {info.utilization !== undefined && ` ${Math.round(info.utilization)}%`}
    </span>
  );
}

function PermissionEntry({
  sessionId,
  item,
}: {
  sessionId: string;
  item: Extract<TranscriptItem, { kind: "permission_request" }>;
}) {
  const respondPermission = useSessions((s) => s.respondPermission);
  const prettyArgs = useMemo(() => {
    try {
      return JSON.stringify(item.args, null, 2);
    } catch {
      return String(item.args);
    }
  }, [item.args]);

  return (
    <div className="t-permission">
      <div className="t-permission-title">{item.title ?? `Permission requested: ${item.tool}`}</div>
      <pre className="t-permission-args">{prettyArgs}</pre>
      {item.resolved === "pending" ? (
        <div className="t-permission-actions">
          <button
            className="small"
            onClick={() => void respondPermission(sessionId, item.requestId, true)}
          >
            Allow
          </button>
          <button
            className="small danger"
            onClick={() => void respondPermission(sessionId, item.requestId, false)}
          >
            Deny
          </button>
        </div>
      ) : (
        <div className="t-permission-resolved">{item.resolved}</div>
      )}
    </div>
  );
}

/**
 * One transcript entry, memoized. During streaming only the tail item's object
 * identity changes (deltas merge into it), so every settled entry above it —
 * including its parsed markdown — is skipped entirely on re-render. Long
 * sessions stay smooth this way; without it the whole transcript re-rendered
 * per streamed token.
 */
const TranscriptEntry = memo(function TranscriptEntry({
  item,
  sessionId,
}: {
  item: TranscriptItem;
  sessionId: string;
}) {
  switch (item.kind) {
    case "user":
      return (
        <div className="t-user">
          <Markdown text={item.text} />
        </div>
      );
    case "text":
      return <StreamedMarkdown text={item.text} />;
    case "tool_use":
      return <ToolUseEntry item={item} />;
    case "thinking":
      return <ThinkingEntry text={item.text} />;
    case "denied":
      return <DeniedEntry item={item} />;
    case "status":
      return <div className="t-status">— {item.status.replace("_", " ")} —</div>;
    case "permission_request":
      return <PermissionEntry sessionId={sessionId} item={item} />;
    case "dialog":
      return (
        <div className="t-dialog">
          <div className="t-dialog-title">
            <Icon name="question" /> {item.title}
          </div>
          {item.lines.map((line, j) => (
            <div key={j} className="t-dialog-line">
              {line}
            </div>
          ))}
        </div>
      );
    case "settings":
      return (
        <div className="t-status">
          <Icon name="sliders" /> {item.text}
        </div>
      );
  }
});

function TranscriptView({ sessionId, items }: { sessionId: string; items: TranscriptItem[] }) {
  const containerRef = useRef<HTMLDivElement>(null);
  // Auto-follow is a *state of the scrollbar*, not a mode: pinned to the bottom
  // means follow new output; scrolled up to read means stay put and offer a
  // "latest" jump instead of yanking the user back down mid-read.
  const followingRef = useRef(true);
  const [following, setFollowing] = useState(true);

  const scrollToBottom = useCallback(() => {
    const el = containerRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  useEffect(() => {
    if (followingRef.current) scrollToBottom();
  }, [items, scrollToBottom]);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    // The smoothed reveal grows the tail block between `items` updates; watch
    // the subtree so that growth is followed too (while pinned).
    const observer = new MutationObserver(() => {
      if (followingRef.current) scrollToBottom();
    });
    observer.observe(el, { childList: true, subtree: true, characterData: true });
    return () => observer.disconnect();
  }, [scrollToBottom]);

  const onScroll = () => {
    const el = containerRef.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
    if (nearBottom !== followingRef.current) {
      followingRef.current = nearBottom;
      setFollowing(nearBottom);
    }
  };

  if (items.length === 0) {
    return (
      <div className="transcript">
        <p className="empty">No output yet for this session (live transcript).</p>
      </div>
    );
  }

  return (
    <div className="transcript-wrap">
      <div className="transcript" ref={containerRef} onScroll={onScroll}>
        {items.map((item, i) => (
          <TranscriptEntry key={i} item={item} sessionId={sessionId} />
        ))}
      </div>
      {!following && (
        <button
          className="jump-latest"
          onClick={() => {
            followingRef.current = true;
            setFollowing(true);
            scrollToBottom();
          }}
        >
          <Icon name="arrow-down" size={12} /> Latest
        </button>
      )}
    </div>
  );
}

type SuggestedCommand = CommandInfo & { local?: boolean };

/** One entry in the autocomplete list: either a command or a value for its argument. */
type Suggestion =
  | { kind: "command"; command: SuggestedCommand }
  | { kind: "value"; command: string; value: string; label: string };

/** How tall the follow-up box grows before it scrolls instead. */
const MAX_INPUT_HEIGHT = 160;

/**
 * Chat input with slash-command autocomplete. Command names come from the session
 * (the CLI reports them) plus Maestro's own; for Maestro's runtime commands the argument
 * is completed too, which is where the model *ids* become discoverable.
 *
 * Enter sends; Shift+Enter (or Ctrl/Cmd+Enter) inserts a newline / force-sends, and
 * ArrowUp/Down at the edges of the box recall previously sent messages, like a shell.
 */
function ChatInput({
  disabled,
  commands,
  models,
  onSend,
  onResume,
  onLocal,
  onError,
}: {
  disabled: boolean;
  commands: CommandInfo[];
  models: ModelOption[];
  onSend: (text: string, attachments: Attachment[]) => void;
  onResume: () => void;
  onLocal: (command: string, argument: string) => void;
  onError: (message: string) => void;
}) {
  const [value, setValue] = useState("");
  const [highlight, setHighlight] = useState(0);
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [history, setHistory] = useState<string[]>([]);
  const [historyIndex, setHistoryIndex] = useState<number | null>(null);
  const [suppressAutocompleteFor, setSuppressAutocompleteFor] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_INPUT_HEIGHT)}px`;
  }, [value]);

  const suggestions = useMemo<Suggestion[]>(() => {
    if (!value.startsWith("/") || value === suppressAutocompleteFor) return [];
    const [head, ...rest] = value.slice(1).split(" ");
    if (rest.length === 0) {
      const query = head.toLowerCase();
      const all: SuggestedCommand[] = [...LOCAL_COMMANDS, ...commands];
      return all
        .filter((c) => c.name.toLowerCase().startsWith(query))
        .slice(0, 8)
        .map((command) => ({ kind: "command", command }));
    }
    const partial = rest.join(" ").toLowerCase();
    return localArgumentValues(head, models)
      .filter(
        (v) => v.value.toLowerCase().includes(partial) || v.label.toLowerCase().includes(partial),
      )
      .slice(0, 8)
      .map((v) => ({ kind: "value", command: head, value: v.value, label: v.label }));
  }, [value, commands, models, suppressAutocompleteFor]);

  const runLocal = (name: string, argument: string) => {
    setValue("");
    setHighlight(0);
    if (name === "resume") {
      onResume();
      return;
    }
    onLocal(name, argument);
  };

  const accept = (suggestion: Suggestion) => {
    if (suggestion.kind === "value") {
      // Picking the value *is* the action — filling it in would leave Enter with
      // nothing to do, since the suggestion list still matches the completed text.
      runLocal(suggestion.command, suggestion.value);
      return;
    }
    const command = suggestion.command;
    // A local command with no argument runs immediately; the rest just get filled in.
    if (command.local && command.argument_hint === "") {
      runLocal(command.name, "");
      return;
    }
    setValue(`/${command.name} `);
    setHighlight(0);
  };

  const submit = () => {
    const text = value.trim();
    if (text.length === 0 && attachments.length === 0) return;
    if (text.startsWith("/")) {
      const [head, ...rest] = text.slice(1).split(" ");
      if (LOCAL_COMMANDS.some((c) => c.name === head)) {
        runLocal(head, rest.join(" ").trim());
        return;
      }
    }
    onSend(text, attachments);
    setHistory((h) => (h[h.length - 1] === text ? h : [...h, text]));
    setHistoryIndex(null);
    setValue("");
    setAttachments([]);
    setHighlight(0);
  };

  /** Screenshots go straight from the clipboard to the agent. */
  const paste = async (event: React.ClipboardEvent<HTMLTextAreaElement>) => {
    const images = [...event.clipboardData.items].filter((i) => i.type.startsWith("image/"));
    if (images.length === 0) return;
    event.preventDefault();
    for (const item of images) {
      const file = item.getAsFile();
      if (!file) continue;
      if (file.size > MAX_ATTACHMENT_BYTES) {
        onError(`image is too large (${Math.round(file.size / 1024)} KB); 5 MB is the limit`);
        continue;
      }
      const buffer = new Uint8Array(await file.arrayBuffer());
      let binary = "";
      for (const byte of buffer) binary += String.fromCharCode(byte);
      setAttachments((current) => [...current, { media_type: file.type, data: btoa(binary) }]);
    }
  };

  return (
    <div className="chat-input">
      {suggestions.length > 0 && (
        <div className="autocomplete">
          {suggestions.map((s, i) => (
            <button
              key={s.kind === "command" ? s.command.name : s.value}
              className={`autocomplete-item ${i === highlight ? "highlight" : ""}`}
              onMouseEnter={() => setHighlight(i)}
              onClick={() => accept(s)}
            >
              {s.kind === "command" ? (
                <>
                  <span className="ac-name">
                    /{s.command.name}{" "}
                    {s.command.argument_hint && <em>{s.command.argument_hint}</em>}
                    {s.command.local && <span className="ac-local">maestro</span>}
                  </span>
                  <span className="ac-desc">{s.command.description}</span>
                </>
              ) : (
                <>
                  <span className="ac-name">{s.value}</span>
                  <span className="ac-desc">{s.label}</span>
                </>
              )}
            </button>
          ))}
        </div>
      )}
      {attachments.length > 0 && (
        <div className="attachments">
          {attachments.map((a, i) => (
            <button
              key={i}
              className="attachment"
              title="Remove"
              onClick={() => setAttachments((c) => c.filter((_, j) => j !== i))}
            >
              <Icon name="file-text" /> {a.media_type.replace("image/", "")}{" "}
              {Math.round((a.data.length * 3) / 4 / 1024)} KB
              <Icon name="close" />
            </button>
          ))}
        </div>
      )}
      <div className="follow-up">
        <textarea
          ref={textareaRef}
          rows={1}
          placeholder={
            disabled
              ? "Session is finished"
              : "Message the agent… (Enter to send, Shift+Enter for a new line)"
          }
          value={value}
          disabled={disabled}
          onPaste={(e) => void paste(e)}
          onChange={(e) => {
            setValue(e.target.value);
            setHighlight(0);
            setHistoryIndex(null);
          }}
          onKeyDown={(e) => {
            if (suggestions.length > 0) {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setHighlight((h) => (h + 1) % suggestions.length);
                return;
              }
              if (e.key === "ArrowUp") {
                e.preventDefault();
                setHighlight((h) => (h - 1 + suggestions.length) % suggestions.length);
                return;
              }
              if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
                e.preventDefault();
                accept(suggestions[highlight]);
                return;
              }
              if (e.key === "Escape") {
                e.preventDefault();
                setSuppressAutocompleteFor(value);
                return;
              }
            }
            // Enter sends; Shift+Enter (or Ctrl/Cmd+Enter) inserts a newline / force-sends.
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              submit();
              return;
            }
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
              e.preventDefault();
              submit();
              return;
            }
            // History recall (like a shell): only when the caret is already at the
            // edge the arrow key would otherwise do nothing useful from, so normal
            // multi-line cursor movement is never hijacked.
            if (history.length > 0) {
              const el = e.currentTarget;
              const atStart = el.selectionStart === 0 && el.selectionEnd === 0;
              const atEnd = el.selectionStart === value.length && el.selectionEnd === value.length;
              if (e.key === "ArrowUp" && atStart) {
                e.preventDefault();
                const next =
                  historyIndex === null ? history.length - 1 : Math.max(0, historyIndex - 1);
                setHistoryIndex(next);
                setValue(history[next]);
                return;
              }
              if (e.key === "ArrowDown" && atEnd && historyIndex !== null) {
                e.preventDefault();
                const next = historyIndex + 1;
                if (next >= history.length) {
                  setHistoryIndex(null);
                  setValue("");
                } else {
                  setHistoryIndex(next);
                  setValue(history[next]);
                }
              }
            }
          }}
        />
        <button
          className="btn-primary"
          disabled={disabled || (value.trim().length === 0 && attachments.length === 0)}
          onClick={submit}
        >
          Send
        </button>
      </div>
    </div>
  );
}

function NewSessionForm({
  branch,
  onSpawned,
}: {
  branch: string;
  onSpawned: (s: Session) => void;
}) {
  const spawn = useSessions((s) => s.spawn);
  const models = useSessions((s) => s.models);
  const refreshModels = useSessions((s) => s.refreshModels);
  const presets = useSessions((s) => s.presets);
  const fetchPresets = useSessions((s) => s.fetchPresets);
  const savePreset = useSessions((s) => s.savePreset);
  const deletePreset = useSessions((s) => s.deletePreset);
  const [prompt, setPrompt] = useState("");
  const [model, setModel] = useState<string>("");
  const [effort, setEffort] = useState<string>("");
  const [permissionMode, setPermissionMode] = useState<string>("");
  const [thinking, setThinking] = useState<string>("");
  const [sessionType, setSessionType] = useState<string>("manual");
  const [busy, setBusy] = useState(false);
  const [presetName, setPresetName] = useState<string | null>(null);
  const [presetBusy, setPresetBusy] = useState(false);

  // The cached list can be stale (e.g. left over from mock mode); ask the CLI once.
  useEffect(() => {
    void refreshModels();
    void fetchPresets();
  }, [refreshModels, fetchPresets]);

  const presetMenuOptions = useMemo<SelectMenuOption[]>(
    () => [
      { value: "", label: "No preset" },
      ...presets.map((p) => ({
        value: p.id,
        label: p.name,
        description: [p.session_type, p.model, p.effort, p.permission_mode]
          .filter(Boolean)
          .join(" · "),
      })),
    ],
    [presets],
  );

  const applyPreset = (id: string) => {
    const preset = presets.find((p) => p.id === id);
    if (!preset) return;
    setSessionType(preset.session_type ?? "manual");
    setModel(preset.model ?? "");
    setEffort(preset.effort ?? "");
    setPermissionMode(preset.permission_mode ?? "");
  };

  const confirmSavePreset = async () => {
    const name = presetName?.trim();
    if (!name) return;
    setPresetBusy(true);
    try {
      await savePreset({
        name,
        session_type: sessionType || null,
        model: model || null,
        effort: effort || null,
        permission_mode: permissionMode || null,
        tools_profile: null,
      });
      setPresetName(null);
    } finally {
      setPresetBusy(false);
    }
  };

  const [presetPicker, setPresetPicker] = useState("");
  const handlePresetChange = (id: string) => {
    setPresetPicker(id);
    if (id) applyPreset(id);
  };
  const deleteCurrentPreset = async () => {
    if (!presetPicker) return;
    await deletePreset(presetPicker);
    setPresetPicker("");
  };

  const modelMenuOptions = useMemo<SelectMenuOption[]>(
    () => [
      DEFAULT_OPTION,
      ...models
        .filter((m) => m.id !== "default")
        .map((m) => ({ value: m.id, label: m.display_name })),
    ],
    [models],
  );

  const submit = async () => {
    setBusy(true);
    const session = await spawn({
      branch,
      prompt,
      session_type: sessionType,
      model: model || undefined,
      effort: effort || undefined,
      permission_mode: permissionMode || undefined,
      thinking: thinking || undefined,
    });
    setBusy(false);
    if (session) {
      setPrompt("");
      onSpawned(session);
    }
  };

  return (
    <div className="new-session">
      <div className="new-session-row preset-row">
        <SelectMenu
          icon="bot"
          title="Apply a saved preset (model, effort, permission mode, type)"
          value={presetPicker}
          onChange={handlePresetChange}
          options={presetMenuOptions}
        />
        {presetPicker && (
          <button
            className="small icon-only ghost"
            title="Delete this preset"
            onClick={() => void deleteCurrentPreset()}
          >
            <Icon name="trash" size={12} />
          </button>
        )}
        {presetName === null ? (
          <button className="small ghost" onClick={() => setPresetName("")}>
            <Icon name="plus" size={12} /> Save as preset
          </button>
        ) : (
          <>
            <input
              type="text"
              className="preset-name-input"
              placeholder="Preset name…"
              autoFocus
              value={presetName}
              onChange={(e) => setPresetName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void confirmSavePreset();
                if (e.key === "Escape") setPresetName(null);
              }}
            />
            <button
              className="small ghost"
              disabled={presetBusy || !presetName.trim()}
              onClick={() => void confirmSavePreset()}
            >
              {presetBusy ? <Icon name="spinner" spin /> : <Icon name="check" size={12} />}
            </button>
            <button className="small icon-only ghost" onClick={() => setPresetName(null)}>
              <Icon name="close" size={12} />
            </button>
          </>
        )}
      </div>
      <div className="new-session-row">
        <SelectMenu
          title="What kind of work this session is — it decides notes and tools"
          value={sessionType}
          onChange={setSessionType}
          options={SESSION_TYPE_MENU_OPTIONS}
        />
        <div className="segmented">
          <SelectMenu
            icon="sliders"
            title="Model for this session"
            value={model}
            onChange={setModel}
            options={modelMenuOptions}
          />
          <SelectMenu
            title="Reasoning effort"
            value={effort}
            onChange={setEffort}
            options={[DEFAULT_OPTION, ...EFFORT_MENU_OPTIONS]}
          />
          <SelectMenu
            icon="shield"
            title="Permission mode"
            value={permissionMode}
            onChange={setPermissionMode}
            options={[
              DEFAULT_OPTION,
              ...PERMISSION_MENU_OPTIONS.filter((o) => o.value !== "default"),
            ]}
          />
          <SelectMenu
            title="Thinking budget"
            value={thinking}
            onChange={setThinking}
            options={[
              { value: "", label: "CLI default" },
              ...THINKING_MENU_OPTIONS.filter((o) => o.value !== "default"),
            ]}
          />
        </div>
      </div>
      <textarea
        rows={3}
        placeholder={
          sessionType === "review_fix"
            ? "Paste the review comments…"
            : "Initial prompt for the agent…"
        }
        value={prompt}
        onChange={(e) => setPrompt(e.target.value)}
      />
      <button
        className="btn-primary"
        disabled={busy || prompt.trim().length === 0}
        onClick={() => void submit()}
      >
        {busy ? "Starting…" : "Start session"}
      </button>
      {permissionMode === "auto" && (
        <p className="hint">
          In <code>auto</code> a classifier answers ordinary permission prompts. Commits, pushes and
          PRs still stop at the approval dialog — the gate runs before every tool call, in every
          mode.
        </p>
      )}
      {sessionType === "implementation" && (
        <p className="hint">
          On close this session gets one last turn to write <code>TASK_NOTES.md</code> — the record
          the next agent reads.
        </p>
      )}
      {sessionType === "review_fix" && (
        <p className="hint">
          This session can call <code>ask_original_agent</code> to ask the implementing agent about
          its reasoning (twice per turn, read-only).
        </p>
      )}
      <p className="hint">
        One writer per worktree: if a writer session is already running, this one starts read-only.
      </p>
    </div>
  );
}

/** Modal listing resumable (finished, with an SDK id) sessions of the branch. */
function ResumePicker({
  sessions,
  onPick,
  onClose,
}: {
  sessions: Session[];
  onPick: (s: Session) => void;
  onClose: () => void;
}) {
  const resumable = sessions.filter((s) => isTerminalStatus(s.status) && s.sdk_session_id);
  useEscapeToClose(onClose);
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="play" /> Resume a session
        </h3>
        {resumable.length === 0 ? (
          <p className="hint">No finished sessions with a resumable context on this worktree.</p>
        ) : (
          <ul className="resume-list">
            {resumable.map((s) => (
              <li key={s.id}>
                <button className="resume-item" onClick={() => onPick(s)}>
                  <span>
                    {s.session_type} · {s.id.slice(0, 8)} · {s.status}
                  </span>
                  <span className="ac-desc">
                    {s.model ?? "default model"} · {new Date(s.updated_at).toLocaleString()}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Cancel
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Live model / effort / permission switches. These apply to the running session — the
 * core forwards them to the CLI, persists them, and reports back through
 * `session.settings_changed`, so the selectors always show what the agent is really on.
 */
function RuntimeControls({ session }: { session: Session }) {
  const models = useSessions((s) => s.models);
  const refreshModels = useSessions((s) => s.refreshModels);
  const setModel = useSessions((s) => s.setModel);
  const setEffort = useSessions((s) => s.setEffort);
  const setPermissionMode = useSessions((s) => s.setPermissionMode);
  const setThinking = useSessions((s) => s.setThinking);

  useEffect(() => {
    void refreshModels();
  }, [refreshModels]);

  // The CLI may report a model this list doesn't have (an alias, or a newer build).
  const known = models.some((m) => m.id === session.model);

  const modelMenuOptions = useMemo<SelectMenuOption[]>(
    () => [
      { value: "", label: !known && session.model ? `Default (${session.model})` : "Default" },
      ...models
        .filter((m) => m.id !== "default")
        .map((m) => ({ value: m.id, label: m.display_name, description: m.id })),
    ],
    [models, known, session.model],
  );
  const permissionMenuOptions = useMemo<SelectMenuOption[]>(
    () =>
      session.permission_mode === null
        ? [DEFAULT_OPTION, ...PERMISSION_MENU_OPTIONS.filter((o) => o.value !== "default")]
        : PERMISSION_MENU_OPTIONS,
    [session.permission_mode],
  );
  const thinkingMenuOptions = useMemo<SelectMenuOption[]>(
    () => [
      ...(session.thinking === null ? [{ value: "", label: "CLI default" }] : []),
      ...THINKING_MENU_OPTIONS,
    ],
    [session.thinking],
  );

  return (
    <div className="runtime-controls segmented">
      <SelectMenu
        icon="sliders"
        title="Model used for the next turn"
        value={known ? (session.model ?? "") : ""}
        onChange={(v) => void setModel(session.id, v)}
        options={modelMenuOptions}
      />
      <SelectMenu
        title="Reasoning effort"
        value={session.effort ?? ""}
        onChange={(v) => void setEffort(session.id, v)}
        options={[DEFAULT_OPTION, ...EFFORT_MENU_OPTIONS]}
      />
      <SelectMenu
        icon="shield"
        title="Permission mode"
        value={session.permission_mode ?? ""}
        onChange={(v) => void setPermissionMode(session.id, v)}
        options={permissionMenuOptions}
      />
      <SelectMenu
        title="Thinking budget — the CLI default often produces none at all"
        value={session.thinking ?? ""}
        onChange={(v) => void setThinking(session.id, v || "default")}
        options={thinkingMenuOptions}
      />
    </div>
  );
}

export function SessionPanel({ worktree }: { worktree: WorktreeInfo }) {
  const branch = worktree.branch as string;
  const sessions = useSessions((s) => s.byBranch[branch]);
  const transcripts = useSessions((s) => s.transcripts);
  const commands = useSessions((s) => s.commands);
  const {
    fetch,
    send,
    interrupt,
    close,
    remove,
    spawn,
    models,
    dialogs,
    todos,
    usage,
    agents,
    mcpServers,
    rateLimit,
    setModel,
    setEffort,
    setPermissionMode,
    setThinking,
    loadTranscript,
    seedTranscript,
    error,
    clearError,
  } = useSessions();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [showResumePicker, setShowResumePicker] = useState(false);

  useEffect(() => {
    // fetch is a stable zustand action.
    void fetch(branch);
  }, [branch, fetch]);

  useEffect(() => {
    // A session opened after a restart has nothing live in memory yet — hydrate
    // its transcript from the last autosave. A no-op once it's already loaded.
    if (selectedId) void loadTranscript(selectedId);
  }, [selectedId, loadTranscript]);

  useEffect(() => {
    // Tab labels read the first prompt out of the transcript — load every tab's
    // history, not just the selected one, so restarted sessions aren't all
    // stuck showing the generic "manual · 3f9c2a1b" fallback.
    for (const session of sessions ?? []) void loadTranscript(session.id);
  }, [sessions, loadTranscript]);

  const list = sessions ?? [];
  const selected = list.find((s) => s.id === selectedId) ?? null;
  const activeCount = activeSessionCount(list);

  // Any session of this worktree can be blocked on a dialog, not just the visible one.
  const pendingDialog =
    (selected && dialogs[selected.id]) || list.map((s) => dialogs[s.id]).find(Boolean) || null;

  const runLocalCommand = (session: Session, command: string, argument: string) => {
    switch (command) {
      case "model":
        void setModel(session.id, argument);
        break;
      case "effort":
        void setEffort(session.id, argument);
        break;
      case "permissions":
        void setPermissionMode(session.id, argument);
        break;
      case "thinking":
        void setThinking(session.id, argument || "default");
        break;
    }
  };

  const resume = async (source: Session) => {
    // Load before spawning, not after — the source is a finished session, so
    // nothing about its history can still change in between.
    await loadTranscript(source.id);
    const priorHistory = useSessions.getState().transcripts[source.id] ?? [];
    const session = await spawn({
      branch,
      prompt: "",
      resume_from: source.id,
      permission_mode: source.permission_mode ?? undefined,
    });
    if (session) {
      // The new session is a fresh row (that's how resume-with-context works),
      // but visually it should read as the same conversation continuing, not a
      // second one starting from blank — so carry the old transcript forward.
      seedTranscript(session.id, [
        ...priorHistory,
        { kind: "settings", text: "Resumed — continuing this session" },
      ]);
      setSelectedId(session.id);
    }
  };

  /** The first thing the user asked a session — for tab labels and retries. */
  const firstPrompt = (sessionId: string): string | null => {
    const item = (transcripts[sessionId] ?? []).find((t) => t.kind === "user");
    return item && item.kind === "user" ? item.text : null;
  };

  /** A fresh session with the failed one's settings and opening prompt. */
  const retry = async (source: Session) => {
    const prompt = firstPrompt(source.id);
    if (!prompt) return;
    const session = await spawn({
      branch,
      prompt,
      session_type: source.session_type,
      model: source.model ?? undefined,
      effort: source.effort ?? undefined,
      permission_mode: source.permission_mode ?? undefined,
      thinking: source.thinking ?? undefined,
    });
    if (session) setSelectedId(session.id);
  };

  /** Human tab label: the prompt's first words beat `manual · 3f9c2a1b`. */
  const tabLabel = (s: Session): string => {
    const prompt = firstPrompt(s.id);
    if (!prompt) return `${s.session_type} · ${s.id.slice(0, 8)}`;
    const oneLine = prompt.replace(/\s+/g, " ").trim();
    return oneLine.length > 26 ? `${oneLine.slice(0, 26)}…` : oneLine;
  };

  const removeAllFinished = useSessions((s) => s.removeAllFinished);
  const finishedCount = list.filter((s) => isTerminalStatus(s.status)).length;
  const [clearingFinished, setClearingFinished] = useState(false);

  const clearFinished = async () => {
    // A single stray row is not worth interrupting the user for; a pile of
    // them is exactly what this button exists to clean up in one go.
    if (finishedCount > 2) {
      const { confirm } = await import("@tauri-apps/plugin-dialog");
      const ok = await confirm(
        `Delete ${finishedCount} finished session${finishedCount === 1 ? "" : "s"} on this branch? This cannot be undone.`,
        { title: "MaestroIDE", kind: "warning" },
      );
      if (!ok) return;
    }
    setClearingFinished(true);
    try {
      await removeAllFinished(branch);
    } finally {
      setClearingFinished(false);
    }
  };

  return (
    <div className="session-panel">
      <div className="panel-header">
        <h2>
          Sessions <span className="count">({activeCount} active)</span>
        </h2>
        {finishedCount > 0 && (
          <button
            className="small ghost"
            disabled={clearingFinished}
            title={`Delete all ${finishedCount} finished (done/failed/cancelled) session${finishedCount === 1 ? "" : "s"} on this branch`}
            onClick={() => void clearFinished()}
          >
            {clearingFinished ? <Icon name="spinner" spin /> : <Icon name="trash" size={12} />}{" "}
            Clear finished
          </button>
        )}
        {rateLimit && <RateLimitPill info={rateLimit} />}
      </div>

      {error && (
        <div className="error-banner" onClick={clearError} title="Click to dismiss">
          {error}
        </div>
      )}

      <div className="session-tabs">
        {list.map((s) => (
          <button
            key={s.id}
            className={`session-tab ${s.id === selectedId ? "selected" : ""}`}
            onClick={() => setSelectedId(s.id)}
            title={`${s.session_type} · ${s.id.slice(0, 8)}`}
          >
            {tabLabel(s)}
            {s.permission_mode === READ_ONLY_MODE && <span className="pill">read-only</span>}
            <StatusPill status={s.status} />
          </button>
        ))}
        <button
          className={`session-tab ${selectedId === null ? "selected" : ""}`}
          onClick={() => setSelectedId(null)}
        >
          + new
        </button>
      </div>

      {selected ? (
        <>
          <div className="session-toolbar">
            {isTerminalStatus(selected.status) ? (
              <span className="session-meta">
                {selected.model ?? "default model"}
                {selected.effort ? ` · ${selected.effort}` : ""}
                {selected.permission_mode ? ` · ${selected.permission_mode}` : ""}
              </span>
            ) : (
              <RuntimeControls session={selected} />
            )}
            {usage[selected.id] && <UsageMeter usage={usage[selected.id]} />}
            {!isTerminalStatus(selected.status) && (
              <SessionCapabilities
                session={selected}
                agents={agents[selected.id] ?? []}
                servers={mcpServers[selected.id] ?? []}
              />
            )}
            <div className="actions">
              {isTerminalStatus(selected.status) ? (
                <>
                  {selected.status === "failed" && firstPrompt(selected.id) && (
                    <button
                      className="small"
                      onClick={() => void retry(selected)}
                      title="Fresh session, same settings and opening prompt"
                    >
                      <Icon name="refresh" /> Retry
                    </button>
                  )}
                  {selected.sdk_session_id && (
                    <button
                      className="small"
                      onClick={() => void resume(selected)}
                      title="Continue this session's context"
                    >
                      <Icon name="play" /> Resume
                    </button>
                  )}
                  <button
                    className="small danger"
                    onClick={() => {
                      void remove(selected.id, branch).then((ok) => {
                        if (ok) setSelectedId(null);
                      });
                    }}
                  >
                    Remove
                  </button>
                </>
              ) : (
                <>
                  <button
                    className="small icon-only ghost"
                    title="Collapse / expand every tool and thinking block"
                    onClick={() => {
                      const details =
                        document.querySelectorAll<HTMLDetailsElement>(".transcript details");
                      const anyOpen = [...details].some((d) => d.open);
                      details.forEach((d) => (d.open = !anyOpen));
                    }}
                  >
                    <Icon name="log" size={13} />
                  </button>
                  <button
                    className="small"
                    disabled={selected.status !== "streaming"}
                    onClick={() => void interrupt(selected.id)}
                    title="Stop the current turn"
                  >
                    <Icon name="stop" /> Interrupt
                  </button>
                  <button className="small danger" onClick={() => void close(selected.id)}>
                    <Icon name="close" /> Close
                  </button>
                </>
              )}
            </div>
          </div>
          <TranscriptView sessionId={selected.id} items={transcripts[selected.id] ?? []} />
          {(todos[selected.id]?.length ?? 0) > 0 && <TodoList items={todos[selected.id]} />}
          <ChatInput
            disabled={isTerminalStatus(selected.status)}
            commands={commands[selected.id] ?? []}
            models={models}
            onSend={(text, attachments) => void send(selected.id, text, attachments)}
            onResume={() => setShowResumePicker(true)}
            onLocal={(command, argument) => runLocalCommand(selected, command, argument)}
            onError={(message) => useSessions.setState({ error: message })}
          />
        </>
      ) : (
        <NewSessionForm branch={branch} onSpawned={(s) => setSelectedId(s.id)} />
      )}

      {/* A dialog blocks the agent, so it is modal even if the user switched tabs. */}
      {pendingDialog && <QuestionDialog dialog={pendingDialog} />}

      {showResumePicker && (
        <ResumePicker
          sessions={list}
          onPick={(s) => {
            setShowResumePicker(false);
            void resume(s);
          }}
          onClose={() => setShowResumePicker(false)}
        />
      )}
    </div>
  );
}
