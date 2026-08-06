import { useMemo, useState } from "react";
import { Icon } from "../components/Icon";
import { SelectMenu, type SelectMenuOption } from "../components/SelectMenu";
import { useWorktrees } from "../state/worktrees";
import type { MergeOutcome, WorktreeInfo } from "../types/worktrees";

/**
 * Lands a worktree's branch into another worktree's checked-out branch — a
 * local `git merge --no-ff` run in the target's own working directory. Never
 * pushes, and never touches or removes the source worktree; that stays a
 * separate, explicit action if the user wants it.
 */
export function MergeDialog({
  source,
  worktrees,
  onClose,
}: {
  source: WorktreeInfo;
  worktrees: WorktreeInfo[];
  onClose: () => void;
}) {
  const merge = useWorktrees((s) => s.merge);

  const targets = useMemo(
    () => worktrees.filter((w) => w.branch && w.branch !== source.branch),
    [worktrees, source.branch],
  );
  const [targetBranch, setTargetBranch] = useState<string>(() => {
    const primary = targets.find((w) => w.is_primary);
    return (primary ?? targets[0])?.branch ?? "";
  });
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<MergeOutcome | null>(null);

  const targetOptions: SelectMenuOption[] = targets.map((w) => ({
    value: w.branch as string,
    label: w.branch as string,
    description: w.is_primary
      ? "Primary worktree"
      : w.status?.dirty
        ? "Has uncommitted changes"
        : undefined,
  }));

  const target = targets.find((w) => w.branch === targetBranch);
  const targetDirty = target?.status?.dirty ?? false;
  const sourceDirty = source.status?.dirty ?? false;

  const submit = async () => {
    if (!targetBranch || targetDirty) return;
    setBusy(true);
    const outcome = await merge(source.branch as string, targetBranch);
    setBusy(false);
    if (outcome) setResult(outcome);
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="arrow-up" /> Merge into…
        </h3>
        <p className="hint">
          Merges <code>{source.branch}</code> into whichever branch you pick below — a regular merge
          commit run in that worktree, local only. It never pushes, and never touches this worktree.
        </p>

        {targets.length === 0 ? (
          <p className="hint warn">No other worktree to merge into.</p>
        ) : !result ? (
          <div className="form-grid">
            <label>
              Target
              <SelectMenu value={targetBranch} onChange={setTargetBranch} options={targetOptions} />
            </label>

            {targetDirty && (
              <p className="hint warn">
                <Icon name="alert" size={12} /> <code>{targetBranch}</code> has uncommitted changes
                — commit or discard them there before merging.
              </p>
            )}
            {sourceDirty && (
              <p className="hint warn">
                <Icon name="alert" size={12} /> <code>{source.branch}</code> has uncommitted changes
                of its own — only what's committed will be merged.
              </p>
            )}
          </div>
        ) : result.merged ? (
          <p className="hint success">
            <Icon name="check" /> Merged <code>{source.branch}</code> into{" "}
            <code>{targetBranch}</code>.
          </p>
        ) : (
          <div className="merge-result">
            {result.conflicts.length > 0 ? (
              <>
                <p className="merge-result-title">
                  <Icon name="alert" /> Merge stopped — conflicts in:
                </p>
                <ul className="merge-conflicts">
                  {result.conflicts.map((path) => (
                    <li key={path}>
                      <code>{path}</code>
                    </li>
                  ))}
                </ul>
                <p className="hint">
                  Resolve them in <code>{targetBranch}</code>'s worktree with your usual git tools
                  and commit — or run <code>git merge --abort</code> there to back out.
                </p>
              </>
            ) : (
              <p className="merge-result-title">
                <Icon name="alert" /> {result.message || "Merge failed."}
              </p>
            )}
          </div>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            {result ? "Close" : "Cancel"}
          </button>
          {targets.length > 0 && !result && (
            <button
              className="btn-primary"
              disabled={busy || !targetBranch || targetDirty}
              onClick={() => void submit()}
            >
              {busy ? "Merging…" : "Merge"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
