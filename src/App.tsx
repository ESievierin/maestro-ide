import { useEffect, useState } from "react";
import { flushTranscripts, useSessions } from "./state/sessions";
import { EventLog } from "./components/EventLog";
import { Icon } from "./components/Icon";
import { Toasts } from "./components/Toasts";
import { useHotkeys } from "./hooks/useHotkeys";
import { useUI } from "./state/ui";
import { useWorktrees } from "./state/worktrees";
import { openWorktree, copyPath } from "./utils/actions";
import { DiffViewer } from "./views/DiffViewer";
import { NotesPanel } from "./views/NotesPanel";
import { SessionPanel } from "./views/SessionPanel";
import { WorktreeList } from "./views/WorktreeList";
import { selectAttentionCount, useAttention } from "./state/attention";
import { AttentionPanel } from "./views/AttentionPanel";
import { GateDialog } from "./views/GateDialog";
import { PromptEditor } from "./views/PromptEditor";
import { SnapshotsDialog } from "./views/SnapshotsDialog";
import { CheckDialog } from "./views/CheckDialog";
import { PushDialog } from "./views/PushDialog";
import { BranchLogDialog } from "./views/BranchLogDialog";
import { HotkeysDialog } from "./views/HotkeysDialog";
import { MergeDialog } from "./views/MergeDialog";
import { CommandPalette } from "./views/CommandPalette";
import { CreatePrDialog } from "./views/CreatePrDialog";
import { DaemonPanel } from "./views/DaemonPanel";
import { PrRepliesDialog } from "./views/PrRepliesDialog";
import { SettingsDialog } from "./views/SettingsDialog";
import { useChecks } from "./state/checks";
import { useDaemon } from "./state/daemon";

function MainPanel() {
  const selected = useWorktrees((s) => s.selected);
  const worktree = useWorktrees((s) => s.worktrees.find((w) => w.branch === s.selected));
  const worktrees = useWorktrees((s) => s.worktrees);
  const repo = useWorktrees((s) => s.repo);
  const tab = useWorktrees((s) => s.tab);
  const setTab = useWorktrees((s) => s.setTab);
  const dialog = useUI((s) => s.dialog);
  const openDialog = useUI((s) => s.openDialog);
  const closeDialog = useUI((s) => s.closeDialog);
  const checkCommand = useChecks((s) => s.command);
  const fetchCheckCommand = useChecks((s) => s.fetchCommand);
  // Total spend across this worktree's sessions (live ones report usage events).
  // A primitive return keeps the zustand snapshot stable when nothing changed.
  const branchCost = useSessions((s) => {
    const list = selected ? (s.byBranch[selected] ?? []) : [];
    let total = 0;
    for (const session of list) {
      total += s.usage[session.id]?.costUsd ?? 0;
    }
    return total;
  });

  useEffect(() => {
    void fetchCheckCommand();
  }, [fetchCheckCommand]);

  if (!selected || !worktree || !worktree.branch) {
    return (
      <div className="main-empty">
        <p>Select a worktree to see its details.</p>
        <p className="hint">
          Sessions, diffs and notes are per worktree. Ctrl+K opens the command palette; Alt+1…9
          jumps between worktrees, Alt+↑/↓ cycles, Alt+C / Alt+D / Alt+N switch chat, diff and
          notes.
        </p>
      </div>
    );
  }

  return (
    <div className="main-detail">
      <div className="worktree-summary">
        <div className="worktree-summary-row">
          <h2>{worktree.branch}</h2>
          <div className="main-tabs btn-group">
            <button
              className={`small ${tab === "chat" ? "selected" : ""}`}
              onClick={() => setTab("chat")}
              title="Chat (Alt+C)"
            >
              <Icon name="chat" /> Chat
            </button>
            <button
              className={`small ${tab === "diff" ? "selected" : ""}`}
              onClick={() => setTab("diff")}
              title="Diff (Alt+D)"
            >
              <Icon name="diff" /> Diff
            </button>
            <button
              className={`small ${tab === "notes" ? "selected" : ""}`}
              onClick={() => setTab("notes")}
              title="Task notes (Alt+N)"
            >
              <Icon name="file-text" /> Notes
            </button>
          </div>
        </div>
        <div className="worktree-path-row">
          <span className="repo-line" title={worktree.path}>
            {worktree.path}
            {worktree.task_id ? ` · ${worktree.task_id}` : ""}
            {worktree.base_branch ? ` · base: ${worktree.base_branch}` : ""}
            {branchCost > 0 ? ` · $${branchCost.toFixed(2)} spent` : ""}
          </span>
          <span className="wt-open-actions">
            <button
              className="small icon-only ghost"
              title="Commits on this branch (vs its base)"
              onClick={() => openDialog("log")}
            >
              <Icon name="log" size={13} />
            </button>
            {checkCommand && (
              <button
                className="small icon-only ghost"
                title={`Checks: ${checkCommand}`}
                onClick={() => openDialog("checks")}
              >
                <Icon name="check" size={13} />
              </button>
            )}
            <button
              className="small icon-only ghost"
              title="Snapshots (checkpoint / roll back the uncommitted state)"
              onClick={() => openDialog("snapshots")}
            >
              <Icon name="history" size={13} />
            </button>
            <button
              className="small icon-only ghost"
              title="Push this branch to the remote…"
              onClick={() => openDialog("push")}
            >
              <Icon name="upload" size={13} />
            </button>
            <button
              className="small icon-only ghost"
              title="Create a pull request (commit + push + gh pr create)…"
              onClick={() => openDialog("createpr")}
            >
              <Icon name="pr" size={13} />
            </button>
            <button
              className="small icon-only ghost"
              title="PR review comments — draft, edit and post replies…"
              onClick={() => openDialog("replies")}
            >
              <Icon name="reply" size={13} />
            </button>
            <button
              className="small icon-only ghost"
              title="Copy worktree path"
              onClick={() => void copyPath(worktree.path)}
            >
              <Icon name="copy" size={13} />
            </button>
            <button
              className="small icon-only ghost"
              title="Open in file explorer"
              onClick={() => openWorktree(worktree.branch as string, "explorer")}
            >
              <Icon name="folder" size={13} />
            </button>
            <button
              className="small icon-only ghost"
              title="Open in editor (Rider)"
              onClick={() => openWorktree(worktree.branch as string, "editor")}
            >
              <Icon name="external-link" size={13} />
            </button>
          </span>
        </div>
      </div>
      {tab === "chat" && <SessionPanel worktree={worktree} />}
      {tab === "diff" && <DiffViewer worktree={worktree} />}
      {tab === "notes" && <NotesPanel worktree={worktree} />}
      {dialog === "snapshots" && <SnapshotsDialog branch={worktree.branch} onClose={closeDialog} />}
      {dialog === "checks" && <CheckDialog branch={worktree.branch} onClose={closeDialog} />}
      {dialog === "push" && <PushDialog branch={worktree.branch} onClose={closeDialog} />}
      {dialog === "log" && <BranchLogDialog branch={worktree.branch} onClose={closeDialog} />}
      {dialog === "merge" && (
        <MergeDialog source={worktree} worktrees={worktrees} repo={repo} onClose={closeDialog} />
      )}
      {dialog === "createpr" && (
        <CreatePrDialog worktree={worktree} repo={repo} onClose={closeDialog} />
      )}
      {dialog === "replies" && <PrRepliesDialog worktree={worktree} onClose={closeDialog} />}
    </div>
  );
}

export default function App() {
  useHotkeys();
  const dialog = useUI((s) => s.dialog);
  const paletteOpen = useUI((s) => s.paletteOpen);
  const eventLogOpen = useUI((s) => s.eventLogOpen);
  const openDialog = useUI((s) => s.openDialog);
  const closeDialog = useUI((s) => s.closeDialog);
  const setPalette = useUI((s) => s.setPalette);
  const toggleEventLog = useUI((s) => s.toggleEventLog);
  const attentionCount = useAttention(selectAttentionCount);

  // Ctrl+K from anywhere, inputs included — the palette is never in the way.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && (e.key === "k" || e.key === "K")) {
        e.preventDefault();
        useUI.getState().setPalette(!useUI.getState().paletteOpen);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  // Closing the window mid-turn deserves one honest question — an interrupted
  // agent leaves a half-done worktree with no notes. A session just sitting at
  // `awaiting_input` isn't doing anything, so it doesn't need to hold up the
  // close: the backend closes it as `done`, exactly like closing it by hand.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void (async () => {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      unlisten = await getCurrentWindow().onCloseRequested(async (event) => {
        const byBranch = useSessions.getState().byBranch;
        const active = Object.values(byBranch)
          .flat()
          .filter((s) => ["spawning", "streaming"].includes(s.status)).length;
        if (active > 0) {
          // A native dialog, not window.confirm() — the browser-synchronous
          // version can hang the whole webview instead of showing a prompt.
          const { confirm } = await import("@tauri-apps/plugin-dialog");
          const ok = await confirm(
            `${active} session${active > 1 ? "s are" : " is"} still working. Quit anyway?`,
            { title: "MaestroIDE", kind: "warning" },
          );
          if (!ok) {
            event.preventDefault();
            return;
          }
        }
        // Any debounced transcript save still pending loses its race with process
        // exit otherwise — flush before the window is allowed to actually close.
        await flushTranscripts();
      });
    })();
    return () => unlisten?.();
  }, []);

  return (
    <div className="app">
      <header className="app-header">
        <h1>
          <Icon name="branch" size={16} className="brand-mark" /> MaestroIDE
        </h1>
        <div className="actions">
          <DaemonChip />
          <AttentionArea count={attentionCount} />
          <button
            className="small ghost"
            onClick={() => setPalette(true)}
            title="Command palette (Ctrl+K)"
          >
            <Icon name="sliders" /> Ctrl+K
          </button>
          <button
            className="small ghost"
            onClick={() => openDialog("prompts")}
            title="Prompt templates"
          >
            <Icon name="file-text" /> Prompts
          </button>
          <button
            className="small icon-only ghost"
            onClick={() => openDialog("hotkeys")}
            title="Keyboard shortcuts"
          >
            <Icon name="question" />
          </button>
          <button
            className="small icon-only ghost"
            onClick={toggleEventLog}
            title="Event log (debug)"
          >
            <Icon name="sliders" />
          </button>
          <button
            className="small icon-only ghost"
            onClick={() => openDialog("settings")}
            title="Settings"
          >
            <Icon name="settings" />
          </button>
        </div>
      </header>
      <div className="app-body">
        <WorktreeList />
        <main className="main-panel">
          <MainPanel />
        </main>
      </div>
      {eventLogOpen && <EventLog onClose={toggleEventLog} />}
      <Toasts />
      <GateDialog />
      {dialog === "prompts" && <PromptEditor onClose={closeDialog} />}
      {dialog === "hotkeys" && <HotkeysDialog onClose={closeDialog} />}
      {dialog === "daemon" && <DaemonPanel onClose={closeDialog} />}
      {dialog === "settings" && <SettingsDialog onClose={closeDialog} />}
      {paletteOpen && <CommandPalette onClose={() => setPalette(false)} />}
    </div>
  );
}

/** Daemon status in the header: hidden until first fetch, then shows on/off,
 * queue depth and a spinning icon while a task runs. Opens the daemon panel. */
function DaemonChip() {
  const status = useDaemon((s) => s.status);
  const fetchStatus = useDaemon((s) => s.fetchStatus);
  const openDialog = useUI((s) => s.openDialog);

  useEffect(() => {
    void fetchStatus();
  }, [fetchStatus]);

  if (!status) return null;
  const busy = status.running !== null;
  return (
    <button
      className={`small ghost ${status.last_error ? "attention-alert" : ""}`}
      onClick={() => openDialog("daemon")}
      title={
        status.enabled
          ? `Daemon on — acting as ${status.account || "?"}${status.repo ? ` on ${status.repo}` : ""}`
          : "GitHub daemon (off)"
      }
    >
      <Icon name="bot" spin={busy} /> Daemon
      {status.enabled && (status.queued > 0 || busy) && (
        <span className="count-pill">{status.queued + (busy ? 1 : 0)}</span>
      )}
    </button>
  );
}

/** "Needs you" with its own local open state (a drawer, not a routed dialog). */
function AttentionArea({ count }: { count: number }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        className={`small ghost ${count > 0 ? "attention-alert" : ""}`}
        onClick={() => setOpen((o) => !o)}
        title="Everything waiting on you"
      >
        <Icon name="bell" /> Needs you
        {count > 0 && <span className="count-pill">{count}</span>}
      </button>
      {open && <AttentionPanel onClose={() => setOpen(false)} />}
    </>
  );
}
