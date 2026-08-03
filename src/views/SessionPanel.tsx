import { useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { activeSessionCount, useSessions } from "../state/sessions";
import type { CommandInfo, Session, TranscriptItem } from "../types/sessions";
import { EFFORTS, isTerminalStatus, PERMISSION_MODES, READ_ONLY_MODE } from "../types/sessions";
import type { WorktreeInfo } from "../types/worktrees";

/** Commands handled by Maestro itself, merged into the autocomplete list. */
const LOCAL_COMMANDS: (CommandInfo & { local: true })[] = [
  {
    name: "resume",
    description: "Maestro: continue a finished session of this worktree",
    argument_hint: "",
    local: true,
  },
];

function StatusPill({ status }: { status: Session["status"] }) {
  return <span className={`pill pill-${status}`}>{status.replace("_", " ")}</span>;
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
        }
      })}
      <div ref={bottomRef} />
    </div>
  );
}

type SuggestedCommand = CommandInfo & { local?: boolean };

/** Chat input with slash-command autocomplete fed by the session's command list. */
function ChatInput({
  disabled,
  commands,
  onSend,
  onResume,
}: {
  disabled: boolean;
  commands: CommandInfo[];
  onSend: (text: string) => void;
  onResume: () => void;
}) {
  const [value, setValue] = useState("");
  const [highlight, setHighlight] = useState(0);

  const suggestions = useMemo<SuggestedCommand[]>(() => {
    if (!value.startsWith("/") || value.includes(" ")) return [];
    const query = value.slice(1).toLowerCase();
    const all: SuggestedCommand[] = [...LOCAL_COMMANDS, ...commands];
    return all.filter((c) => c.name.toLowerCase().startsWith(query)).slice(0, 8);
  }, [value, commands]);

  const accept = (command: SuggestedCommand) => {
    if (command.local && command.name === "resume") {
      setValue("");
      setHighlight(0);
      onResume();
      return;
    }
    setValue(`/${command.name} `);
    setHighlight(0);
  };

  const submit = () => {
    const text = value.trim();
    if (text.length === 0) return;
    onSend(text);
    setValue("");
    setHighlight(0);
  };

  return (
    <div className="chat-input">
      {suggestions.length > 0 && (
        <div className="autocomplete">
          {suggestions.map((c, i) => (
            <button
              key={c.name}
              className={`autocomplete-item ${i === highlight ? "highlight" : ""}`}
              onMouseEnter={() => setHighlight(i)}
              onClick={() => accept(c)}
            >
              <span className="ac-name">
                /{c.name} {c.argument_hint && <em>{c.argument_hint}</em>}
                {c.local && <span className="ac-local">maestro</span>}
              </span>
              <span className="ac-desc">{c.description}</span>
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
              if (e.key === "Tab") {
                e.preventDefault();
                accept(suggestions[highlight]);
                return;
              }
              if (e.key === "Enter") {
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
  const [prompt, setPrompt] = useState("");
  const [model, setModel] = useState<string>("");
  const [effort, setEffort] = useState<string>("");
  const [permissionMode, setPermissionMode] = useState<string>("");
  const [busy, setBusy] = useState(false);

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
              {m === READ_ONLY_MODE ? "plan (read-only)" : m}
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

export function SessionPanel({ worktree }: { worktree: WorktreeInfo }) {
  const branch = worktree.branch as string;
  const sessions = useSessions((s) => s.byBranch[branch]);
  const transcripts = useSessions((s) => s.transcripts);
  const commands = useSessions((s) => s.commands);
  const { fetch, send, interrupt, close, remove, spawn, error, clearError } = useSessions();
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [showResumePicker, setShowResumePicker] = useState(false);

  useEffect(() => {
    // fetch is a stable zustand action.
    void fetch(branch);
  }, [branch, fetch]);

  const list = sessions ?? [];
  const selected = list.find((s) => s.id === selectedId) ?? null;
  const activeCount = activeSessionCount(list);

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
            <span className="session-meta">
              {selected.model ?? "default model"}
              {selected.effort ? ` · ${selected.effort}` : ""}
              {selected.permission_mode ? ` · ${selected.permission_mode}` : ""}
            </span>
            <div className="actions">
              {isTerminalStatus(selected.status) ? (
                <>
                  {selected.sdk_session_id && (
                    <button className="small" onClick={() => void resume(selected)}>
                      Resume
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
                  >
                    Interrupt
                  </button>
                  <button className="small danger" onClick={() => void close(selected.id)}>
                    Close
                  </button>
                </>
              )}
            </div>
          </div>
          <TranscriptView sessionId={selected.id} items={transcripts[selected.id] ?? []} />
          <ChatInput
            disabled={isTerminalStatus(selected.status)}
            commands={commands[selected.id] ?? []}
            onSend={(text) => void send(selected.id, text)}
            onResume={() => setShowResumePicker(true)}
          />
        </>
      ) : (
        <NewSessionForm branch={branch} onSpawned={(s) => setSelectedId(s.id)} />
      )}

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
