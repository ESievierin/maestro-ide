import { useState } from "react";
import { EventLog } from "./components/EventLog";
import { Icon } from "./components/Icon";
import { Toasts } from "./components/Toasts";
import { useHotkeys } from "./hooks/useHotkeys";
import { useWorktrees } from "./state/worktrees";
import { DiffViewer } from "./views/DiffViewer";
import { SessionPanel } from "./views/SessionPanel";
import { WorktreeList } from "./views/WorktreeList";
import { selectAttentionCount, useAttention } from "./state/attention";
import { AttentionPanel } from "./views/AttentionPanel";
import { GateDialog } from "./views/GateDialog";
import { PromptEditor } from "./views/PromptEditor";

function MainPanel() {
  const selected = useWorktrees((s) => s.selected);
  const worktree = useWorktrees((s) => s.worktrees.find((w) => w.branch === s.selected));
  const tab = useWorktrees((s) => s.tab);
  const setTab = useWorktrees((s) => s.setTab);

  if (!selected || !worktree || !worktree.branch) {
    return (
      <div className="main-empty">
        <p>Select a worktree to see its details.</p>
        <p className="hint">
          Sessions and diffs are per worktree. Alt+1…9 jumps between them, Alt+↑/↓ cycles, Alt+C /
          Alt+D switch chat and diff.
        </p>
      </div>
    );
  }

  return (
    <div className="main-detail">
      <div className="worktree-summary">
        <div className="worktree-summary-row">
          <h2>{worktree.branch}</h2>
          <div className="main-tabs">
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
          </div>
        </div>
        <span className="repo-line" title={worktree.path}>
          {worktree.path}
          {worktree.task_id ? ` · ${worktree.task_id}` : ""}
          {worktree.base_branch ? ` · base: ${worktree.base_branch}` : ""}
        </span>
      </div>
      {tab === "chat" ? <SessionPanel worktree={worktree} /> : <DiffViewer worktree={worktree} />}
    </div>
  );
}

export default function App() {
  const [showPrompts, setShowPrompts] = useState(false);
  useHotkeys();
  const [showAttention, setShowAttention] = useState(false);
  const attentionCount = useAttention(selectAttentionCount);

  return (
    <div className="app">
      <header className="app-header">
        <h1>MaestroIDE</h1>
        <div className="actions">
          <button
            className={`small ${attentionCount > 0 ? "attention-alert" : ""}`}
            onClick={() => setShowAttention((open) => !open)}
            title="Everything waiting on you"
          >
            <Icon name="bell" /> Needs you
            {attentionCount > 0 && <span className="count-pill">{attentionCount}</span>}
          </button>
          <button className="small" onClick={() => setShowPrompts(true)} title="Prompt templates">
            <Icon name="file-text" /> Prompts
          </button>
        </div>
      </header>
      {showAttention && <AttentionPanel onClose={() => setShowAttention(false)} />}
      <div className="app-body">
        <WorktreeList />
        <main className="main-panel">
          <MainPanel />
        </main>
      </div>
      <EventLog />
      <Toasts />
      <GateDialog />
      {showPrompts && <PromptEditor onClose={() => setShowPrompts(false)} />}
    </div>
  );
}
