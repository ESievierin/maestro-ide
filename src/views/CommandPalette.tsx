import { useEffect, useMemo, useRef, useState } from "react";
import { Icon, type IconName } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useChecks } from "../state/checks";
import { useSessions } from "../state/sessions";
import { useUI } from "../state/ui";
import { useWorktrees } from "../state/worktrees";
import { isTerminalStatus } from "../types/sessions";
import { clearFinishedSessions, copyPath, openWorktree, removeWorktree } from "../utils/actions";

interface PaletteItem {
  id: string;
  icon: IconName;
  label: string;
  hint?: string;
  run: () => void;
}

/**
 * Ctrl+K: every navigation target and worktree action behind one fuzzy filter.
 * Deterministic matching — case-insensitive substring, no scoring surprises.
 */
export function CommandPalette({ onClose }: { onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [highlight, setHighlight] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  useEscapeToClose(onClose);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const items = useMemo<PaletteItem[]>(() => {
    const { worktrees, selected, select, setTab, sync } = useWorktrees.getState();
    const { openDialog, toggleEventLog } = useUI.getState();
    const checkCommand = useChecks.getState().command;
    const current = worktrees.find((w) => w.branch === selected);
    const result: PaletteItem[] = [];

    for (const w of worktrees) {
      if (!w.branch) continue;
      const branch = w.branch;
      result.push({
        id: `go:${branch}`,
        icon: "branch",
        label: `Go to: ${branch}`,
        hint: w.is_primary ? "primary" : (w.task_id ?? undefined),
        run: () => select(branch),
      });
    }

    result.push(
      {
        id: "tab:chat",
        icon: "chat",
        label: "Tab: Chat",
        hint: "Alt+C",
        run: () => setTab("chat"),
      },
      {
        id: "tab:diff",
        icon: "diff",
        label: "Tab: Diff",
        hint: "Alt+D",
        run: () => setTab("diff"),
      },
      {
        id: "tab:notes",
        icon: "file-text",
        label: "Tab: Notes",
        hint: "Alt+N",
        run: () => setTab("notes"),
      },
    );

    if (current?.branch) {
      const branch = current.branch;
      result.push(
        {
          id: "wt:editor",
          icon: "external-link",
          label: "Open in editor (Rider)",
          hint: branch,
          run: () => openWorktree(branch, "editor"),
        },
        {
          id: "wt:explorer",
          icon: "folder",
          label: "Open in file explorer",
          hint: branch,
          run: () => openWorktree(branch, "explorer"),
        },
        {
          id: "wt:copy",
          icon: "copy",
          label: "Copy worktree path",
          hint: branch,
          run: () => void copyPath(current.path),
        },
        {
          id: "wt:log",
          icon: "log",
          label: "Branch commits (vs base)…",
          hint: branch,
          run: () => openDialog("log"),
        },
        {
          id: "wt:merge",
          icon: "arrow-up",
          label: "Merge into…",
          hint: branch,
          run: () => openDialog("merge"),
        },
        {
          id: "wt:push",
          icon: "upload",
          label: "Push branch…",
          hint: branch,
          run: () => openDialog("push"),
        },
        {
          id: "wt:createpr",
          icon: "pr",
          label: "Create PR…",
          hint: branch,
          run: () => openDialog("createpr"),
        },
        {
          id: "wt:replies",
          icon: "reply",
          label: "PR review comments — reply…",
          hint: branch,
          run: () => openDialog("replies"),
        },
        {
          id: "wt:snapshots",
          icon: "history",
          label: "Snapshots…",
          hint: branch,
          run: () => openDialog("snapshots"),
        },
      );
      if (!current.is_primary) {
        result.push({
          id: "wt:sync",
          icon: "arrow-down",
          label: "Sync with base",
          hint: branch,
          run: () => void sync(branch),
        });
      }
      if (checkCommand) {
        result.push({
          id: "wt:checks",
          icon: "check",
          label: "Checks…",
          hint: checkCommand,
          run: () => openDialog("checks"),
        });
      }
      const finishedCount = (useSessions.getState().byBranch[branch] ?? []).filter((s) =>
        isTerminalStatus(s.status),
      ).length;
      if (finishedCount > 0) {
        result.push({
          id: "wt:clear-finished",
          icon: "trash",
          label: `Clear ${finishedCount} finished session${finishedCount === 1 ? "" : "s"}`,
          hint: branch,
          run: () => void clearFinishedSessions(branch, finishedCount),
        });
      }
      if (!current.is_primary) {
        result.push({
          id: "wt:remove",
          icon: "trash",
          label: "Remove worktree…",
          hint: branch,
          run: () => void removeWorktree(branch),
        });
      }
    }

    const streaming = Object.values(useSessions.getState().byBranch)
      .flat()
      .filter((s) => s.status === "streaming");
    if (streaming.length > 0) {
      result.push({
        id: "app:interrupt-all",
        icon: "stop",
        label: `Interrupt all running sessions (${streaming.length})`,
        run: () => {
          const interrupt = useSessions.getState().interrupt;
          for (const s of streaming) void interrupt(s.id);
        },
      });
    }

    result.push(
      {
        id: "app:new-worktree",
        icon: "plus",
        label: "New worktree…",
        run: () => openDialog("create"),
      },
      {
        id: "app:prompts",
        icon: "file-text",
        label: "Prompt templates…",
        run: () => openDialog("prompts"),
      },
      {
        id: "app:daemon",
        icon: "bot",
        label: "GitHub daemon…",
        run: () => openDialog("daemon"),
      },
      {
        id: "app:hotkeys",
        icon: "question",
        label: "Keyboard shortcuts",
        run: () => openDialog("hotkeys"),
      },
      {
        id: "app:settings",
        icon: "settings",
        label: "Settings…",
        run: () => openDialog("settings"),
      },
      {
        id: "app:search-sessions",
        icon: "search",
        label: "Search session history (all branches)…",
        run: () => openDialog("search-sessions"),
      },
      {
        id: "app:eventlog",
        icon: "sliders",
        label: "Toggle event log",
        run: () => toggleEventLog(),
      },
    );
    return result;
  }, []);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return items;
    return items.filter(
      (item) => item.label.toLowerCase().includes(q) || item.hint?.toLowerCase().includes(q),
    );
  }, [items, query]);

  useEffect(() => {
    setHighlight(0);
  }, [query]);

  useEffect(() => {
    listRef.current?.children[highlight]?.scrollIntoView({ block: "nearest" });
  }, [highlight]);

  const execute = (item: PaletteItem | undefined) => {
    if (!item) return;
    onClose();
    item.run();
  };

  return (
    <div className="modal-backdrop palette-backdrop" onClick={onClose}>
      <div className="palette" onClick={(e) => e.stopPropagation()}>
        <input
          ref={inputRef}
          type="text"
          placeholder="Type a command or worktree name…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setHighlight((h) => Math.min(h + 1, filtered.length - 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setHighlight((h) => Math.max(h - 1, 0));
            } else if (e.key === "Enter") {
              e.preventDefault();
              execute(filtered[highlight]);
            }
          }}
        />
        <ul className="palette-list" ref={listRef}>
          {filtered.map((item, i) => (
            <li
              key={item.id}
              className={`palette-item ${i === highlight ? "highlight" : ""}`}
              onMouseEnter={() => setHighlight(i)}
              onClick={() => execute(item)}
            >
              <Icon name={item.icon} size={13} />
              <span className="palette-label">{item.label}</span>
              {item.hint && <span className="palette-hint">{item.hint}</span>}
            </li>
          ))}
          {filtered.length === 0 && <li className="palette-empty">No matches.</li>}
        </ul>
      </div>
    </div>
  );
}
