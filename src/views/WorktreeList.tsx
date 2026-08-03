import { useEffect, useState } from "react";
import { Icon, StatusDot } from "../components/Icon";
import { useDiffs } from "../state/diffs";
import { activeSessionCount, useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import type { WorktreeInfo } from "../types/worktrees";
import { CreateWorktreeDialog } from "./CreateWorktreeDialog";

/**
 * At-a-glance state of one worktree, in the order that matters when four agents run:
 * who is blocked (failed / awaiting_input), who is busy (working), and whose diff is
 * ready to review.
 */
function StatusBadges({ wt }: { wt: WorktreeInfo }) {
  const sessions = useSessions((s) => (wt.branch ? s.byBranch[wt.branch] : undefined));
  const diffFiles = useDiffs((s) =>
    wt.branch ? (s.snapshots[`${wt.branch}|worktree`]?.files.length ?? null) : null,
  );

  const list = sessions ?? [];
  const working = list.some((s) => s.status === "streaming" || s.status === "spawning");
  const awaiting = list.some((s) => s.status === "awaiting_input");
  const failed = list.some((s) => s.status === "failed");
  const active = activeSessionCount(sessions);
  // "diff ready" is only interesting once nobody is still writing to the branch.
  const diffReady = !working && (diffFiles ?? 0) > 0;

  return (
    <span className="badges">
      {failed && (
        <span className="badge badge-failed">
          <Icon name="alert" size={11} /> failed
        </span>
      )}
      {awaiting && (
        <span className="badge badge-awaiting">
          <Icon name="question" size={11} /> awaiting
        </span>
      )}
      {working && (
        <span className="badge badge-active">
          <StatusDot tone="active" pulse /> working{active > 1 ? ` ${active}` : ""}
        </span>
      )}
      {diffReady && (
        <span className="badge badge-info">
          <Icon name="diff" size={11} /> {diffFiles}
        </span>
      )}
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
  const { repo, worktrees, selected, loading, error, refresh, remove, select, clearError } =
    useWorktrees();
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
          <button className="small" onClick={() => setShowCreate(true)} title="New worktree">
            <Icon name="plus" /> New
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
          {worktrees.length === 0 && !loading && (
            <p className="hint">
              No worktrees yet. Use <strong>+ New</strong> to create one per task.
            </p>
          )}
          <ul className="worktree-items">
            {worktrees.map((wt) => (
              <li
                key={wt.path}
                className={wt.branch === selected ? "selected" : ""}
                onClick={() => select(wt.branch)}
              >
                <div className="wt-row">
                  <span className="wt-branch">
                    <Icon name="branch" size={12} /> {wt.branch ?? "(detached)"}
                  </span>
                  <StatusBadges wt={wt} />
                </div>
                <div className="wt-meta">
                  {wt.task_id && <span className="wt-task">{wt.task_id}</span>}
                  {!wt.is_primary && wt.branch && (
                    <button
                      className="small danger icon-only"
                      title="Remove worktree"
                      onClick={(e) => {
                        e.stopPropagation();
                        void onRemove(wt.branch as string);
                      }}
                    >
                      <Icon name="trash" />
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
