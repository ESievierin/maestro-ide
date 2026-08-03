import { EventLog } from "./components/EventLog";
import { useWorktrees } from "./state/worktrees";
import { SessionPanel } from "./views/SessionPanel";
import { WorktreeList } from "./views/WorktreeList";

function MainPanel() {
  const selected = useWorktrees((s) => s.selected);
  const worktree = useWorktrees((s) => s.worktrees.find((w) => w.branch === s.selected));

  if (!selected || !worktree || !worktree.branch) {
    return (
      <div className="main-empty">
        <p>Select a worktree to see its details.</p>
        <p className="hint">Sessions run per worktree; diff viewer arrives in T5.</p>
      </div>
    );
  }

  return (
    <div className="main-detail">
      <div className="worktree-summary">
        <h2>{worktree.branch}</h2>
        <span className="repo-line" title={worktree.path}>
          {worktree.path}
          {worktree.task_id ? ` · ${worktree.task_id}` : ""}
          {worktree.base_branch ? ` · base: ${worktree.base_branch}` : ""}
        </span>
      </div>
      <SessionPanel worktree={worktree} />
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
