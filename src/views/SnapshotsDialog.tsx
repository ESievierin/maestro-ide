import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useToasts } from "../state/toasts";
import { useWorktrees } from "../state/worktrees";
import type { RestoreOutcome, Snapshot } from "../types/worktrees";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

/**
 * Checkpoints of a worktree's uncommitted state, stash-backed. Take one before
 * letting an agent attempt something risky; restore it when the attempt made
 * things worse. Restoring replaces the current uncommitted state (with an
 * explicit confirmation when that state isn't empty) and keeps the snapshot,
 * so one good checkpoint survives several bad attempts.
 */
export function SnapshotsDialog({ branch, onClose }: { branch: string; onClose: () => void }) {
  const [snapshots, setSnapshots] = useState<Snapshot[] | null>(null);
  const [label, setLabel] = useState("");
  const [busy, setBusy] = useState(false);
  useEscapeToClose(onClose);

  const reload = useCallback(async () => {
    try {
      setSnapshots(await invoke<Snapshot[]>("list_snapshots", { branch }));
    } catch {
      setSnapshots([]); // the failure is already an error toast via error.raised
    }
  }, [branch]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const take = async () => {
    setBusy(true);
    try {
      await invoke("take_snapshot", { branch, label: label.trim() });
      setLabel("");
      useToasts.getState().push({
        severity: "info",
        code: "snapshot",
        message: "Snapshot taken — the worktree itself is unchanged.",
      });
      await reload();
    } catch {
      // error.raised toast already shown
    } finally {
      setBusy(false);
    }
  };

  const restore = async (snap: Snapshot) => {
    setBusy(true);
    try {
      let result = await invoke<RestoreOutcome>("restore_snapshot", {
        branch,
        id: snap.id,
        confirmed: false,
      });
      if (result.outcome === "dirty_confirmation_required") {
        const ok = window.confirm(
          `Restoring "${snap.label}" replaces the current uncommitted changes in "${branch}".\nDiscard them and restore the snapshot?`,
        );
        if (!ok) return;
        result = await invoke<RestoreOutcome>("restore_snapshot", {
          branch,
          id: snap.id,
          confirmed: true,
        });
      }
      if (result.outcome === "restored") {
        useToasts.getState().push({
          severity: "info",
          code: "snapshot",
          message: `Restored "${snap.label}". The snapshot is kept for another round.`,
        });
        const { useDiffs } = await import("../state/diffs");
        void useDiffs.getState().refresh(branch, "worktree");
        void useWorktrees.getState().refresh();
        await reload();
      }
    } catch {
      // error.raised toast already shown
    } finally {
      setBusy(false);
    }
  };

  const drop = async (snap: Snapshot) => {
    if (!window.confirm(`Delete snapshot "${snap.label}"? This cannot be undone.`)) return;
    setBusy(true);
    try {
      await invoke("drop_snapshot", { branch, id: snap.id });
      await reload();
    } catch {
      // error.raised toast already shown
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="history" /> Snapshots · {branch}
        </h3>
        <p className="hint">
          A snapshot records the worktree's uncommitted state (untracked files included) without
          changing anything. Restore rolls the working tree back to it. Stash-backed — also visible
          to <code>git stash list</code>.
        </p>

        <div className="snapshot-take">
          <input
            type="text"
            placeholder="Label (e.g. before retry with different approach)…"
            value={label}
            disabled={busy}
            onChange={(e) => setLabel(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void take();
            }}
          />
          <button className="btn-primary" disabled={busy} onClick={() => void take()}>
            <Icon name="plus" /> Take snapshot
          </button>
        </div>

        {snapshots === null ? (
          <p className="empty">Loading…</p>
        ) : snapshots.length === 0 ? (
          <p className="empty">No snapshots yet. Take one before a risky agent attempt.</p>
        ) : (
          <ul className="snapshot-list">
            {snapshots.map((snap) => (
              <li key={snap.id}>
                <div className="snapshot-info">
                  <span className="snapshot-label">{snap.label}</span>
                  <span className="ac-desc">
                    {snap.id} · {snap.created_at}
                  </span>
                </div>
                <span className="snapshot-actions">
                  <button className="small" disabled={busy} onClick={() => void restore(snap)}>
                    <Icon name="history" /> Restore
                  </button>
                  <button
                    className="small danger icon-only ghost"
                    title="Delete snapshot"
                    disabled={busy}
                    onClick={() => void drop(snap)}
                  >
                    <Icon name="trash" />
                  </button>
                </span>
              </li>
            ))}
          </ul>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
