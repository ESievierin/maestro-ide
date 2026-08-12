import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useWorktrees } from "../state/worktrees";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

/**
 * Explicit, confirmed push of one branch. The dialog states the exact command
 * before anything runs — clicking Push here *is* the authorization, the same
 * way the approval gate is for agent-initiated pushes.
 */
export function PushDialog({ branch, onClose }: { branch: string; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<string | null>(null);
  useEscapeToClose(onClose);

  const push = async () => {
    setBusy(true);
    try {
      const result = await invoke<string>("push_worktree", { branch });
      setReport(result || "Pushed.");
      void useWorktrees.getState().refresh();
    } catch {
      // the failure is already an error toast via error.raised
      onClose();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="upload" /> Push · {branch}
        </h3>

        {report === null ? (
          <>
            <p className="hint">
              Runs <code>git push -u origin {branch}</code> in this worktree — publishing the
              branch's commits to the remote. Nothing else is touched.
            </p>
            <div className="modal-actions">
              <button className="ghost" disabled={busy} onClick={onClose}>
                Cancel
              </button>
              <button className="btn-primary" disabled={busy} onClick={() => void push()}>
                {busy ? "Pushing…" : "Push"}
              </button>
            </div>
          </>
        ) : (
          <>
            <p className="hint success">
              <Icon name="check" /> Pushed.
            </p>
            <pre className="check-output">{report}</pre>
            <div className="modal-actions">
              <button className="ghost" onClick={onClose}>
                Close
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
