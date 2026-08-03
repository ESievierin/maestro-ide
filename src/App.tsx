import { useState } from "react";
import { EventLog } from "./components/EventLog";
import { useWorktrees } from "./state/worktrees";
import { DiffViewer } from "./views/DiffViewer";
import { SessionPanel } from "./views/SessionPanel";
import { WorktreeList } from "./views/WorktreeList";

function MainPanel() {
  const selected = useWorktrees((s) => s.selected);
  const worktree = useWorktrees((s) => s.worktrees.find((w) => w.branch === s.selected));
  const [tab, setTab] = useState<"chat" | "diff">("chat");

  if (!selected || !worktree || !worktree.branch) {
    return (
      <div className="main-empty">
        <p>Select a worktree to see its details.</p>
        <p className="hint">Sessions and diffs are per worktree.</p>
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
            >
              Chat
            </button>
            <button
              className={`small ${tab === "diff" ? "selected" : ""}`}
              onClick={() => setTab("diff")}
            >
              Diff
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
  return (
    <div className="app">
      <header className="app-header">
        <h1>MaestroIDE</h1>
      </header>
      <div className="app-body">
        <WorktreeList />
        <main className="main-panel">
          <MainPanel />
        </main>
      </div>
      <EventLog />
    </div>
  );
}
