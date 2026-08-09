import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon, StatusDot } from "../components/Icon";
import { SelectMenu } from "../components/SelectMenu";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useDaemon, type DaemonTask } from "../state/daemon";

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
  const dismiss = useDaemon((s) => s.dismiss);
  useEscapeToClose(onClose);

  const [telemetryEnabled, setTelemetryEnabledState] = useState<boolean | null>(null);

  useEffect(() => {
    void fetchStatus();
    void fetchTasks();
    void invoke<boolean>("get_telemetry_enabled").then(setTelemetryEnabledState);
  }, [fetchStatus, fetchTasks]);

  const toggleTelemetry = async () => {
    if (telemetryEnabled === null) return;
    const next = !telemetryEnabled;
    setTelemetryEnabledState(next);
    try {
      await invoke("set_telemetry_enabled", { enabled: next });
    } catch {
      setTelemetryEnabledState(!next); // revert; the error toast is already up
    }
  };

  const visible = tasks.filter((t) => t.state !== "dismissed");

  return (
    <div className="modal-backdrop" onClick={onClose}>
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

        <label
          className="daemon-telemetry-toggle notify-toggle"
          title="Records every prompt and reply (and reasoning, when there is any) to ~/.maestro/telemetry for later analysis. Turning this off does not touch anything already written."
        >
          <input
            type="checkbox"
            checked={telemetryEnabled ?? true}
            disabled={telemetryEnabled === null}
            onChange={() => void toggleTelemetry()}
          />
          Record conversation telemetry
        </label>

        <div className="daemon-tasks">
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
