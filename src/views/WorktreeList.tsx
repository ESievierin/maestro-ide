import { useEffect, useState } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { Icon, StatusDot } from "../components/Icon";
import { useChecks } from "../state/checks";
import { useDiffs } from "../state/diffs";
import { useUI } from "../state/ui";
import { activeSessionCount, useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import type { WorktreeInfo } from "../types/worktrees";
import { removeWorktree, syncAllWorktrees } from "../utils/actions";
import { CreateWorktreeDialog } from "./CreateWorktreeDialog";
import { MergeDialog } from "./MergeDialog";

const STARTUP_TIPS = [
  "Tip: each worktree runs its own agent, so you can work several tasks in parallel.",
  "Tip: use the command palette to jump to any worktree without touching the mouse.",
  "Tip: a failed check shows up as a red badge right on the worktree row.",
  "Tip: you can merge a worktree's branch back the moment its diff looks good.",
];

function useStartupTip() {
  const [tip] = useState(() => STARTUP_TIPS[Math.floor(Math.random() * STARTUP_TIPS.length)]);
  return tip;
}

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
  const check = useChecks((s) => (wt.branch ? s.results[wt.branch] : undefined));

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
      {check?.status === "running" && (
        <span className="badge badge-info">
          <Icon name="spinner" size={11} spin /> checks
        </span>
      )}
      {check?.status === "passed" && (
        <span className="badge badge-active" title="Latest check run passed">
          <Icon name="check" size={11} /> checks
        </span>
      )}
      {check?.status === "failed" && (
        <span className="badge badge-failed" title="Latest check run failed">
          <Icon name="close" size={11} /> checks
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

/**
 * Repository chooser. Shown when nothing is selected yet and whenever the user asks to
 * switch — one repository is open at a time, and its path is persisted in settings.
 */
function RepoPicker({ current, onDone }: { current: string | null; onDone: () => void }) {
  const setRepo = useWorktrees((s) => s.setRepo);
  const [path, setPath] = useState(current ?? "");
  const [busy, setBusy] = useState(false);

  const open = async (candidate: string) => {
    const trimmed = candidate.trim();
    if (!trimmed) return;
    setBusy(true);
    const ok = await setRepo(trimmed);
    setBusy(false);
    if (ok) onDone();
  };

  const browse = async () => {
    const picked = await openDialog({
      directory: true,
      multiple: false,
      title: "Pick a git repository",
      defaultPath: path || undefined,
    });
    if (typeof picked === "string") {
      setPath(picked);
      await open(picked);
    }
  };

  return (
    <div className="repo-picker">
      <p className="hint">Git repository to orchestrate:</p>
      <input
        type="text"
        placeholder="C:\path\to\repo"
        value={path}
        onChange={(e) => setPath(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") void open(path);
        }}
      />
      <div className="actions">
        <button disabled={busy} onClick={() => void browse()}>
          <Icon name="folder" /> Browse…
        </button>
        <button disabled={busy || !path.trim()} onClick={() => void open(path)}>
          {busy ? "Opening…" : "Open"}
        </button>
        {current && (
          <button className="small" onClick={onDone}>
            Cancel
          </button>
        )}
      </div>
    </div>
  );
}

export function WorktreeList() {
  const {
    repo,
    worktrees,
    selected,
    loading,
    error,
    refresh,
    sync,
    setPinned,
    select,
    clearError,
  } = useWorktrees();
  const showCreate = useUI((s) => s.dialog === "create");
  const openDialog = useUI((s) => s.openDialog);
  const closeDialog = useUI((s) => s.closeDialog);
  const [switchingRepo, setSwitchingRepo] = useState(false);
  const [mergeSource, setMergeSource] = useState<WorktreeInfo | null>(null);
  const [syncing, setSyncing] = useState<string | null>(null);
  const [syncingAll, setSyncingAll] = useState(false);
  const startupTip = useStartupTip();
  const [filter, setFilter] = useState("");

  useEffect(() => {
    // refresh is a stable zustand action; run once on mount.
    void refresh();
  }, [refresh]);

  const onSync = async (branch: string) => {
    setSyncing(branch);
    const outcome = await sync(branch);
    setSyncing(null);
    if (!outcome) return; // the failure is already on the error banner / toast
    const { useToasts } = await import("../state/toasts");
    if (outcome.merged) {
      useToasts.getState().push({
        severity: "info",
        code: "synced",
        message: `'${branch}' is up to date with its base.`,
      });
    } else if (outcome.conflicts.length > 0) {
      useToasts.getState().push({
        severity: "warning",
        code: "sync-conflicts",
        message: `Sync of '${branch}' stopped on ${outcome.conflicts.length} conflicting file${outcome.conflicts.length > 1 ? "s" : ""} — resolve in its worktree and commit, or run git merge --abort there.`,
      });
    } else {
      useToasts.getState().push({
        severity: "warning",
        code: "sync-failed",
        message: outcome.message || `Sync of '${branch}' failed.`,
      });
    }
  };

  const syncableCount = worktrees.filter((w) => !w.is_primary && w.branch).length;
  const pushableCount = worktrees.filter(
    (w) => !w.is_primary && w.branch && (w.status?.ahead ?? 0) > 0,
  ).length;

  const onSyncAll = async () => {
    setSyncingAll(true);
    await syncAllWorktrees();
    setSyncingAll(false);
  };

  const normalizedFilter = filter.trim().toLowerCase();
  const filteredWorktrees = normalizedFilter
    ? worktrees.filter(
        (wt) =>
          (wt.branch ?? "").toLowerCase().includes(normalizedFilter) ||
          (wt.task_id ?? "").toLowerCase().includes(normalizedFilter),
      )
    : worktrees;
  // Pinned worktrees float to the top as a group; a stable sort keeps every
  // other relative ordering (creation order within each group) untouched.
  const sortedWorktrees = [...filteredWorktrees].sort(
    (a, b) => Number(b.pinned) - Number(a.pinned),
  );

  return (
    <aside className="worktree-list">
      <div className="panel-header">
        <h2>Worktrees</h2>
        {repo && (
          <button className="small" onClick={() => openDialog("create")} title="New worktree">
            <Icon name="plus" /> New
          </button>
        )}
      </div>

      {error && (
        <div className="error-banner" onClick={clearError} title="Click to dismiss">
          {error}
        </div>
      )}

      {!repo || switchingRepo ? (
        <RepoPicker current={repo?.path ?? null} onDone={() => setSwitchingRepo(false)} />
      ) : (
        <>
          <button
            className="repo-line repo-switch"
            title={`${repo.path}\nClick to open a different repository`}
            onClick={() => setSwitchingRepo(true)}
          >
            <Icon name="folder" size={12} />
            <span className="repo-path">{repo.path}</span>
            <Icon name="chevron-down" size={12} />
          </button>
          {worktrees.length === 0 && !loading && (
            <>
              <p className="hint">
                No worktrees yet. Use <strong>+ New</strong> to create one per task.
              </p>
              <p className="hint startup-tip">{startupTip}</p>
            </>
          )}
          {worktrees.length > 1 && (
            <div className="wt-filter">
              <Icon name="search" size={12} className="wt-filter-icon" />
              <input
                type="text"
                placeholder="Filter by branch or task…"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape" && filter) {
                    e.stopPropagation();
                    setFilter("");
                  }
                }}
              />
              {filter && (
                <button
                  className="small icon-only ghost wt-filter-clear"
                  title="Clear filter"
                  onClick={() => setFilter("")}
                >
                  <Icon name="close" size={11} />
                </button>
              )}
            </div>
          )}
          {syncableCount > 1 && (
            <button
              className="small ghost wt-sync-all"
              disabled={syncingAll}
              title={`Sync all ${syncableCount} worktrees with their base branch`}
              onClick={() => void onSyncAll()}
            >
              <Icon name={syncingAll ? "spinner" : "arrow-down"} spin={syncingAll} /> Sync all
            </button>
          )}
          {pushableCount > 1 && (
            <button
              className="small ghost wt-push-all"
              title={`Push all ${pushableCount} worktrees with unpushed commits…`}
              onClick={() => openDialog("pushall")}
            >
              <Icon name="upload" /> Push all
            </button>
          )}
          {normalizedFilter && filteredWorktrees.length === 0 && (
            <p className="hint">No worktree matches "{filter.trim()}".</p>
          )}
          <ul className="worktree-items">
            {sortedWorktrees.map((wt) => (
              <li
                key={wt.path}
                className={wt.branch === selected ? "selected" : ""}
                onClick={() => select(wt.branch)}
              >
                <div className="wt-row">
                  <span className="wt-branch">
                    <Icon name="branch" size={12} /> {wt.branch ?? "(detached)"}
                    {wt.pinned && <Icon name="star" size={11} className="wt-pin-indicator" />}
                  </span>
                  <StatusBadges wt={wt} />
                </div>
                {(wt.task_id || wt.branch) && (
                  <div className="wt-meta">
                    {wt.task_id && <span className="wt-task">{wt.task_id}</span>}
                    <span className="wt-actions">
                      {wt.branch && (
                        <button
                          className={`small icon-only ghost wt-pin${wt.pinned ? " wt-pin-active" : ""}`}
                          title={wt.pinned ? "Unpin" : "Pin to the top of the list"}
                          onClick={(e) => {
                            e.stopPropagation();
                            void setPinned(wt.branch as string, !wt.pinned);
                          }}
                        >
                          <Icon name="star" />
                        </button>
                      )}
                      {!wt.is_primary && wt.branch && (
                        <button
                          className="small icon-only ghost wt-merge"
                          title="Sync with base (merge the base branch in)"
                          disabled={syncing === wt.branch}
                          onClick={(e) => {
                            e.stopPropagation();
                            void onSync(wt.branch as string);
                          }}
                        >
                          <Icon
                            name={syncing === wt.branch ? "spinner" : "arrow-down"}
                            spin={syncing === wt.branch}
                          />
                        </button>
                      )}
                      {wt.branch && worktrees.length > 1 && (
                        <button
                          className="small icon-only ghost wt-merge"
                          title="Merge into…"
                          onClick={(e) => {
                            e.stopPropagation();
                            setMergeSource(wt);
                          }}
                        >
                          <Icon name="arrow-up" />
                        </button>
                      )}
                      {!wt.is_primary && wt.branch && (
                        <button
                          className="small danger icon-only ghost wt-delete"
                          title="Remove worktree"
                          onClick={(e) => {
                            e.stopPropagation();
                            void removeWorktree(wt.branch as string);
                          }}
                        >
                          <Icon name="trash" />
                        </button>
                      )}
                    </span>
                  </div>
                )}
              </li>
            ))}
          </ul>
        </>
      )}

      {showCreate && repo && <CreateWorktreeDialog repo={repo} onClose={closeDialog} />}
      {mergeSource && (
        <MergeDialog
          source={mergeSource}
          worktrees={worktrees}
          repo={repo}
          onClose={() => setMergeSource(null)}
        />
      )}
    </aside>
  );
}
