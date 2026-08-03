import { useEffect, useState } from "react";
import { activeSessionCount, useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import type { WorktreeInfo } from "../types/worktrees";
import { CreateWorktreeDialog } from "./CreateWorktreeDialog";

function StatusBadges({ wt }: { wt: WorktreeInfo }) {
  const active = useSessions((s) => (wt.branch ? activeSessionCount(s.byBranch[wt.branch]) : 0));
  return (
    <span className="badges">
      {active > 0 && <span className="badge badge-active">⚡{active}</span>}
      {wt.is_primary && <span className="badge badge-muted">primary</span>}
      {wt.status?.dirty && <span className="badge badge-warn">dirty</span>}
      {wt.status && wt.status.ahead > 0 && (
        <span className="badge badge-info">↑{wt.status.ahead}</span>
      )}
      {wt.status && wt.status.behind > 0 && (
        <span className="badge badge-info">↓{wt.status.behind}</span>
      )}
    </span>
  );
}

function RepoPicker() {
  const setRepo = useWorktrees((s) => s.setRepo);
  const [path, setPath] = useState("");

  return (
    <div className="repo-picker">
      <p className="hint">Select the git repository to orchestrate:</p>
      <input
        type="text"
        placeholder="C:\path\to\repo"
        value={path}
        onChange={(e) => setPath(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && path.trim()) void setRepo(path.trim());
        }}
      />
      <button disabled={!path.trim()} onClick={() => void setRepo(path.trim())}>
        Open repository
      </button>
    </div>
  );
}

export function WorktreeList() {
  const { repo, worktrees, selected, error, refresh, remove, select, clearError } = useWorktrees();
  const [showCreate, setShowCreate] = useState(false);

  useEffect(() => {
    // refresh is a stable zustand action; run once on mount.
    void refresh();
  }, [refresh]);

  const onRemove = async (branch: string) => {
    const outcome = await remove(branch, false);
    if (outcome?.outcome === "dirty_confirmation_required") {
      const forceIt = window.confirm(
        `Worktree "${branch}" has uncommitted changes.\nRemove anyway and discard them?`,
      );
      if (forceIt) await remove(branch, true);
    }
  };

  return (
    <aside className="worktree-list">
      <div className="panel-header">
        <h2>Worktrees</h2>
        {repo && (
          <button className="small" onClick={() => setShowCreate(true)}>
            + New
          </button>
        )}
      </div>

      {error && (
        <div className="error-banner" onClick={clearError} title="Click to dismiss">
          {error}
        </div>
      )}

      {!repo ? (
        <RepoPicker />
      ) : (
        <>
          <div className="repo-line" title={repo.path}>
            {repo.path}
          </div>
          <ul className="worktree-items">
            {worktrees.map((wt) => (
              <li
                key={wt.path}
                className={wt.branch === selected ? "selected" : ""}
                onClick={() => select(wt.branch)}
              >
                <div className="wt-row">
                  <span className="wt-branch">{wt.branch ?? "(detached)"}</span>
                  <StatusBadges wt={wt} />
                </div>
                <div className="wt-meta">
                  {wt.task_id && <span className="wt-task">{wt.task_id}</span>}
                  {!wt.is_primary && wt.branch && (
                    <button
                      className="small danger"
                      onClick={(e) => {
                        e.stopPropagation();
                        void onRemove(wt.branch as string);
                      }}
                    >
                      remove
                    </button>
                  )}
                </div>
              </li>
            ))}
          </ul>
        </>
      )}

      {showCreate && repo && (
        <CreateWorktreeDialog repo={repo} onClose={() => setShowCreate(false)} />
      )}
    </aside>
  );
}
