import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useWorktrees } from "../state/worktrees";
import type { WorktreeInfo } from "../types/worktrees";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

interface PushOutcome {
  branch: string;
  ok: boolean;
  message: string;
}

/**
 * The bulk sibling of PushDialog, same contract: this lists exactly which
 * branches will be pushed before anything runs, and clicking the button here
 * *is* the authorization for all of them — no silent one-click loop the way
 * "Sync all" gets away with, because pushing publishes commits to a shared
 * remote instead of just updating local git state.
 */
export function PushAllDialog({
  worktrees,
  onClose,
}: {
  worktrees: WorktreeInfo[];
  onClose: () => void;
}) {
  useEscapeToClose(onClose);
  const targets = worktrees.filter((w) => !w.is_primary && w.branch && (w.status?.ahead ?? 0) > 0);
  const [busy, setBusy] = useState(false);
  const [results, setResults] = useState<PushOutcome[] | null>(null);

  const pushAll = async () => {
    setBusy(true);
    const outcomes: PushOutcome[] = [];
    for (const w of targets) {
      const branch = w.branch as string;
      try {
        const message = await invoke<string>("push_worktree", { branch });
        outcomes.push({ branch, ok: true, message: message || "Pushed." });
      } catch (e) {
        outcomes.push({ branch, ok: false, message: String(e) });
      }
    }
    setResults(outcomes);
    setBusy(false);
    void useWorktrees.getState().refresh();
  };

  const failedCount = results?.filter((r) => !r.ok).length ?? 0;

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="upload" /> Push all
        </h3>

        {!results ? (
          targets.length === 0 ? (
            <p className="hint">No worktree has unpushed commits.</p>
          ) : (
            <>
              <p className="hint">
                Runs <code>git push -u origin &lt;branch&gt;</code> in each worktree below, one at a
                time — a failure on one does not stop the rest.
              </p>
              <ul className="push-all-list">
                {targets.map((w) => (
                  <li key={w.branch}>
                    <code>{w.branch}</code>{" "}
                    <span className="badge badge-info">↑{w.status?.ahead}</span>
                  </li>
                ))}
              </ul>
            </>
          )
        ) : (
          <>
            <p className={failedCount > 0 ? "hint warn" : "hint success"}>
              <Icon name={failedCount > 0 ? "alert" : "check"} /> {results.length - failedCount} of{" "}
              {results.length} pushed
              {failedCount > 0 ? `, ${failedCount} failed` : ""}.
            </p>
            <ul className="push-all-list">
              {results.map((r) => (
                <li key={r.branch}>
                  <Icon name={r.ok ? "check" : "alert"} size={12} /> <code>{r.branch}</code>
                  {!r.ok && <span className="hint warn"> — {r.message}</span>}
                </li>
              ))}
            </ul>
          </>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            {results ? "Close" : "Cancel"}
          </button>
          {targets.length > 0 && !results && (
            <button className="btn-primary" disabled={busy} onClick={() => void pushAll()}>
              {busy
                ? "Pushing…"
                : `Push ${targets.length} branch${targets.length === 1 ? "" : "es"}`}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
