import { useEffect, useState } from "react";
import { Icon, StatusDot } from "../components/Icon";
import { SelectMenu } from "../components/SelectMenu";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useDaemon, type DaemonTask } from "../state/daemon";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

/**
 * The background daemon: review requests on PRs (→ REVIEW.md), review comments
 * on the user's own PR branches (→ REVIEW_PLAN.md), and assigned Jira issues
 * (→ RESEARCH.md). Everything read-only; it never posts and never commits —
 * every result waits for the human.
 */
export function DaemonPanel({ onClose }: { onClose: () => void }) {
  const status = useDaemon((s) => s.status);
  const tasks = useDaemon((s) => s.tasks);
  const fetchStatus = useDaemon((s) => s.fetchStatus);
  const fetchTasks = useDaemon((s) => s.fetchTasks);
  const setEnabled = useDaemon((s) => s.setEnabled);
  const setAccount = useDaemon((s) => s.setAccount);
  const setWatchedAccounts = useDaemon((s) => s.setWatchedAccounts);
  const setSkipLabels = useDaemon((s) => s.setSkipLabels);
  const pollNow = useDaemon((s) => s.pollNow);
  const dismiss = useDaemon((s) => s.dismiss);
  const dismissFinished = useDaemon((s) => s.dismissFinished);
  const [clearingFinished, setClearingFinished] = useState(false);
  const [polling, setPolling] = useState(false);
  useEscapeToClose(onClose);

  useEffect(() => {
    void fetchStatus();
    void fetchTasks();
  }, [fetchStatus, fetchTasks]);

  const visible = tasks.filter((t) => t.state !== "dismissed");
  const finishedCount = visible.filter((t) => t.state === "done" || t.state === "failed").length;

  const clearFinished = async () => {
    setClearingFinished(true);
    try {
      await dismissFinished();
    } finally {
      setClearingFinished(false);
    }
  };

  const doPollNow = async () => {
    setPolling(true);
    try {
      await pollNow();
    } finally {
      setPolling(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal daemon-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="bot" /> GitHub daemon
        </h3>
        <p className="hint">
          Watches for PRs where the selected account is asked to review (→ REVIEW.md), new review
          comments on PRs whose branch has a worktree here (→ REVIEW_PLAN.md), and Jira issues
          assigned to you (→ RESEARCH.md, needs <code>jira_*</code> in config.toml). Everything runs
          read-only; it never posts to GitHub/Jira and never commits.
        </p>

        {status ? (
          <>
            <div className="daemon-controls">
              <button
                className={status.enabled ? "btn-primary" : ""}
                onClick={() => void setEnabled(!status.enabled)}
              >
                {status.enabled ? (
                  <>
                    <StatusDot tone="streaming" pulse /> Running — turn off
                  </>
                ) : (
                  <>
                    <Icon name="play" /> Turn on
                  </>
                )}
              </button>
              <button
                className="small ghost"
                disabled={polling}
                title="Run one polling pass now instead of waiting for the next scheduled tick — works whether or not the daemon is turned on"
                onClick={() => void doPollNow()}
              >
                <Icon name={polling ? "spinner" : "refresh"} spin={polling} /> Poll now
              </button>
              <div className="segmented">
                <SelectMenu
                  icon="bot"
                  title="Which gh account the daemon acts as (per-call token; gh's active account is never switched)"
                  value={status.account}
                  placeholder="gh account…"
                  options={status.accounts.map((a) => ({
                    value: a.login,
                    label: a.login,
                    description: a.active ? "gh active account" : undefined,
                  }))}
                  onChange={(login) => void setAccount(login)}
                />
              </div>
            </div>

            {status.accounts.length > 1 && (
              <div className="daemon-watch-accounts">
                <span className="hint">Also detect review requests / your own PR comments as:</span>
                {status.accounts.map((a) => {
                  const checked = status.watched_accounts.includes(a.login);
                  return (
                    <label key={a.login} className="settings-toggle">
                      <input
                        type="checkbox"
                        checked={checked}
                        onChange={(e) => {
                          const next = e.target.checked
                            ? [...status.watched_accounts, a.login]
                            : status.watched_accounts.filter((login) => login !== a.login);
                          void setWatchedAccounts(next);
                        }}
                      />
                      {a.login}
                    </label>
                  );
                })}
              </div>
            )}

            <label className="settings-field daemon-skip-labels">
              Skip PRs carrying any of these labels (comma-separated, case-insensitive)
              <input
                type="text"
                placeholder="wip, draft, do-not-review"
                defaultValue={status.skip_labels.join(", ")}
                onBlur={(e) => {
                  const next = e.target.value
                    .split(",")
                    .map((s) => s.trim())
                    .filter(Boolean);
                  void setSkipLabels(next);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") e.currentTarget.blur();
                }}
              />
              <span className="hint">
                No review-request or comment-reply task is ever queued for a matching PR. Empty = no
                filter.
              </span>
            </label>

            <p className="daemon-facts hint">
              Watching: <code>{status.repo ?? "(derived from the open repository's origin)"}</code>
              {` · Jira ${status.jira_configured ? "connected" : "not configured"}`}
              {status.last_poll &&
                ` · last poll ${new Date(status.last_poll).toLocaleTimeString()}`}
              {status.utilization !== null && ` · usage ${Math.round(status.utilization)}%`}
            </p>
            {status.last_error && (
              <p className="daemon-error">
                <Icon name="alert" size={12} /> {status.last_error}
              </p>
            )}
          </>
        ) : (
          <p className="empty">
            <Icon name="spinner" spin /> Loading status…
          </p>
        )}

        <div className="daemon-tasks">
          {finishedCount > 0 && (
            <div className="daemon-tasks-header">
              <button
                className="small ghost"
                disabled={clearingFinished}
                title={`Dismiss all ${finishedCount} finished (done/failed) task${finishedCount === 1 ? "" : "s"}`}
                onClick={() => void clearFinished()}
              >
                {clearingFinished ? <Icon name="spinner" spin /> : <Icon name="trash" size={12} />}{" "}
                Clear finished
              </button>
            </div>
          )}
          {visible.length === 0 ? (
            <p className="empty">No daemon tasks yet.</p>
          ) : (
            <ul className="daemon-task-list">
              {visible.map((t) => (
                <TaskRow key={t.key} task={t} onDismiss={(key) => void dismiss(key)} />
              ))}
            </ul>
          )}
        </div>

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}

function TaskRow({ task, onDismiss }: { task: DaemonTask; onDismiss: (key: string) => void }) {
  const tone =
    task.state === "running"
      ? "streaming"
      : task.state === "done"
        ? "done"
        : task.state === "failed"
          ? "failed"
          : "awaiting_input";
  return (
    <li className="daemon-task">
      <StatusDot tone={tone} pulse={task.state === "running"} />
      <span className="daemon-task-main">
        <span className="daemon-task-title" title={task.key}>
          {task.title}
        </span>
        <span className="ac-desc">
          {task.kind === "pr_review"
            ? "review request"
            : task.kind === "pr_comment"
              ? "PR comment"
              : task.kind === "jira"
                ? "Jira"
                : task.kind}{" "}
          · {task.state}
          {task.state === "queued" && task.attempts > 0
            ? ` (retrying, attempt ${task.attempts + 1})`
            : ""}
          {task.branch ? ` · ${task.branch}` : ""}
        </span>
      </span>
      {task.state !== "running" && (
        <button
          className="small icon-only ghost"
          title="Dismiss (does not touch GitHub)"
          onClick={() => onDismiss(task.key)}
        >
          <Icon name="close" size={12} />
        </button>
      )}
    </li>
  );
}
