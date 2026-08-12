import { useEffect } from "react";
import { Icon } from "../components/Icon";
import { StatusDot } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useChecks } from "../state/checks";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

/**
 * The check run for one worktree: what command, the verdict, and the output
 * tail — plus the button to (re)run it. One place answers "does this branch
 * actually build/pass tests?" without leaving the app.
 */
export function CheckDialog({ branch, onClose }: { branch: string; onClose: () => void }) {
  const command = useChecks((s) => s.command);
  const result = useChecks((s) => s.results[branch]);
  const run = useChecks((s) => s.run);
  const fetchResult = useChecks((s) => s.fetchResult);
  useEscapeToClose(onClose);

  useEffect(() => {
    void fetchResult(branch);
  }, [branch, fetchResult]);

  const running = result?.status === "running";

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal check-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="check" /> Checks · {branch}
        </h3>
        <p className="hint">
          Runs <code>{command ?? "(not configured)"}</code> inside this worktree. Configure it via{" "}
          <code>check_command</code> in <code>~/.maestro/config.toml</code>;{" "}
          <code>check_auto = true</code> also runs it after every finished session.
        </p>

        {result ? (
          <div className="check-result">
            <p className="check-status">
              {result.status === "running" ? (
                <>
                  <Icon name="spinner" spin /> Running…
                </>
              ) : result.status === "passed" ? (
                <span className="check-passed">
                  <StatusDot tone="streaming" /> Passed
                </span>
              ) : (
                <span className="check-failed">
                  <StatusDot tone="failed" /> Failed
                  {result.exit_code !== null ? ` (exit ${result.exit_code})` : ""}
                </span>
              )}
              <span className="ac-desc">
                started {new Date(result.started_at).toLocaleTimeString()}
                {result.finished_at &&
                  ` · finished ${new Date(result.finished_at).toLocaleTimeString()}`}
              </span>
            </p>
            {result.output_tail && <pre className="check-output">{result.output_tail}</pre>}
          </div>
        ) : (
          <p className="empty">No check has run for this worktree yet.</p>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Close
          </button>
          <button
            className="btn-primary"
            disabled={!command || running}
            onClick={() => void run(branch)}
          >
            {running ? "Running…" : result ? "Run again" : "Run checks"}
          </button>
        </div>
      </div>
    </div>
  );
}
