import { useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import { Icon, StatusDot } from "../components/Icon";
import remarkGfm from "remark-gfm";
import { activeSessionCount, useSessions } from "../state/sessions";
import type { CommandInfo, ModelOption, Session, TranscriptItem } from "../types/sessions";
import {
  EFFORTS,
  GATE_UNSAFE_MODES,
  isTerminalStatus,
  PERMISSION_MODE_LABELS,
  PERMISSION_MODES,
  READ_ONLY_MODE,
} from "../types/sessions";
import type { WorktreeInfo } from "../types/worktrees";
import { QuestionDialog } from "./QuestionDialog";

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

function ToolUseEntry({ name, summary }: { name: string; summary: string }) {
  return (
    <details className="t-tool">
      <summary>
        <span className="t-tool-name">{name}</span>
        <span className="t-tool-preview">{summary.slice(0, 80)}</span>
      </summary>
      <code>{summary}</code>
    </details>
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

function TranscriptView({ sessionId, items }: { sessionId: string; items: TranscriptItem[] }) {
  const bottomRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "instant", block: "end" });
  }, [items]);

  if (items.length === 0) {
    return (
      <div className="transcript">
        <p className="empty">No output yet for this session (live transcript).</p>
      </div>
    );
  }

  return (
    <div className="transcript">
      {items.map((item, i) => {
        switch (item.kind) {
          case "user":
            return (
              <div key={i} className="t-user">
                <Markdown text={item.text} />
              </div>
            );
          case "text":
            return <Markdown key={i} text={item.text} />;
          case "tool_use":
            return <ToolUseEntry key={i} name={item.name} summary={item.summary} />;
          case "status":
            return (
              <div key={i} className="t-status">
                — {item.status.replace("_", " ")} —
              </div>
            );
          case "permission_request":
            return <PermissionEntry key={i} sessionId={sessionId} item={item} />;
          case "dialog":
            return (
              <div key={i} className="t-dialog">
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
              <div key={i} className="t-status">
                <Icon name="sliders" /> {item.text}
              </div>
            );
        }
      })}
      <div ref={bottomRef} />
    </div>
  );
}

type SuggestedCommand = CommandInfo & { local?: boolean };

/** One entry in the autocomplete list: either a command or a value for its argument. */
type Suggestion =
  | { kind: "command"; command: SuggestedCommand }
  | { kind: "value"; command: string; value: string; label: string };

/**
 * Chat input with slash-command autocomplete. Command names come from the session
 * (the CLI reports them) plus Maestro's own; for Maestro's runtime commands the argument
 * is completed too, which is where the model *ids* become discoverable.
 */
function ChatInput({
  disabled,
  commands,
  models,
  onSend,
  onResume,
  onLocal,
}: {
  disabled: boolean;
  commands: CommandInfo[];
  models: ModelOption[];
  onSend: (text: string) => void;
  onResume: () => void;
  onLocal: (command: string, argument: string) => void;
}) {
  const [value, setValue] = useState("");
  const [highlight, setHighlight] = useState(0);

  const suggestions = useMemo<Suggestion[]>(() => {
    if (!value.startsWith("/")) return [];
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
  }, [value, commands, models]);

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
    if (text.length === 0) return;
    if (text.startsWith("/")) {
      const [head, ...rest] = text.slice(1).split(" ");
      if (LOCAL_COMMANDS.some((c) => c.name === head)) {
        runLocal(head, rest.join(" ").trim());
        return;
      }
    }
    onSend(text);
    setValue("");
    setHighlight(0);
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
      <div className="follow-up">
        <input
          type="text"
          placeholder={disabled ? "Session is finished" : "Message the agent… ( / for commands)"}
          value={value}
          disabled={disabled}
          onChange={(e) => {
            setValue(e.target.value);
            setHighlight(0);
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
              if (e.key === "Tab" || e.key === "Enter") {
                e.preventDefault();
                accept(suggestions[highlight]);
                return;
              }
              if (e.key === "Escape") {
                setValue(value + " ");
                return;
              }
            }
            if (e.key === "Enter") submit();
          }}
        />
        <button disabled={disabled || value.trim().length === 0} onClick={submit}>
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
  const [prompt, setPrompt] = useState("");
  const [model, setModel] = useState<string>("");
  const [effort, setEffort] = useState<string>("");
  const [permissionMode, setPermissionMode] = useState<string>("");
  const [busy, setBusy] = useState(false);

  // The cached list can be stale (e.g. left over from mock mode); ask the CLI once.
  useEffect(() => {
    void refreshModels();
  }, [refreshModels]);

  const submit = async () => {
    setBusy(true);
    const session = await spawn({
      branch,
      prompt,
      model: model || undefined,
      effort: effort || undefined,
      permission_mode: permissionMode || undefined,
    });
    setBusy(false);
    if (session) {
      setPrompt("");
      onSpawned(session);
    }
  };

  return (
    <div className="new-session">
      <div className="new-session-row">
        <select value={model} onChange={(e) => setModel(e.target.value)}>
          <option value="">model: default</option>
          {models
            .filter((m) => m.id !== "default")
            .map((m) => (
              <option key={m.id} value={m.id}>
                {m.display_name}
              </option>
            ))}
        </select>
        <select value={effort} onChange={(e) => setEffort(e.target.value)}>
          <option value="">effort: default</option>
          {EFFORTS.map((e) => (
            <option key={e} value={e}>
              {e}
            </option>
          ))}
        </select>
        <select value={permissionMode} onChange={(e) => setPermissionMode(e.target.value)}>
          <option value="">permissions: default</option>
          {PERMISSION_MODES.map((m) => (
            <option key={m} value={m}>
              {PERMISSION_MODE_LABELS[m] ?? m}
            </option>
          ))}
        </select>
      </div>
      <textarea
        rows={3}
        placeholder="Initial prompt for the agent…"
        value={prompt}
        onChange={(e) => setPrompt(e.target.value)}
      />
      <button disabled={busy || prompt.trim().length === 0} onClick={() => void submit()}>
        {busy ? "Starting…" : "Start session"}
      </button>
      {GATE_UNSAFE_MODES.includes(permissionMode) && (
        <p className="hint warn">
          <Icon name="alert" /> In <code>auto</code> a classifier answers permission prompts, and
          what it approves never reaches the commit/push/PR gate — a push can happen without the
          approval dialog. Avoid it where that matters.
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
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>Resume a session</h3>
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
          <button onClick={onClose}>Cancel</button>
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

  useEffect(() => {
    void refreshModels();
  }, [refreshModels]);

  // The CLI may report a model this list doesn't have (an alias, or a newer build).
  const known = models.some((m) => m.id === session.model);

  return (
    <div className="runtime-controls">
      <label title="Model used for the next turn">
        <Icon name="sliders" />
        <select
          value={known ? (session.model ?? "") : ""}
          onChange={(e) => void setModel(session.id, e.target.value)}
        >
          <option value="">default{!known && session.model ? ` (${session.model})` : ""}</option>
          {models
            .filter((m) => m.id !== "default")
            .map((m) => (
              <option key={m.id} value={m.id}>
                {m.display_name} — {m.id}
              </option>
            ))}
        </select>
      </label>
      <label title="Reasoning effort">
        <select
          value={session.effort ?? ""}
          onChange={(e) => void setEffort(session.id, e.target.value)}
        >
          <option value="">effort: default</option>
          {EFFORTS.map((e) => (
            <option key={e} value={e}>
              {e}
            </option>
          ))}
        </select>
      </label>
      <label title="Permission mode">
        <select
          value={session.permission_mode ?? ""}
          onChange={(e) => void setPermissionMode(session.id, e.target.value)}
        >
          {session.permission_mode === null && <option value="">permissions: default</option>}
          {PERMISSION_MODES.map((m) => (
            <option key={m} value={m}>
              {PERMISSION_MODE_LABELS[m] ?? m}
            </option>
          ))}
        </select>
      </label>
      {GATE_UNSAFE_MODES.includes(session.permission_mode ?? "") && (
        <span className="pill pill-warn" title="A classifier may approve gated commands">
          <Icon name="alert" /> gate not guaranteed
        </span>
      )}
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
    setModel,
    setEffort,
    setPermissionMode,
    error,
    clearError,
  } = useSessions();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [showResumePicker, setShowResumePicker] = useState(false);

  useEffect(() => {
    // fetch is a stable zustand action.
    void fetch(branch);
  }, [branch, fetch]);

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
    }
  };

  const resume = async (source: Session) => {
    const session = await spawn({
      branch,
      prompt: "",
      resume_from: source.id,
      permission_mode: source.permission_mode ?? undefined,
    });
    if (session) setSelectedId(session.id);
  };

  return (
    <div className="session-panel">
      <div className="panel-header">
        <h2>
          Sessions <span className="count">({activeCount} active)</span>
        </h2>
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
          >
            {s.session_type} · {s.id.slice(0, 8)}
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
            <div className="actions">
              {isTerminalStatus(selected.status) ? (
                <>
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
          <ChatInput
            disabled={isTerminalStatus(selected.status)}
            commands={commands[selected.id] ?? []}
            models={models}
            onSend={(text) => void send(selected.id, text)}
            onResume={() => setShowResumePicker(true)}
            onLocal={(command, argument) => runLocalCommand(selected, command, argument)}
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
