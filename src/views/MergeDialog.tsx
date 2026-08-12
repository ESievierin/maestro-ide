import { useMemo, useState } from "react";
import { Icon } from "../components/Icon";
import { SelectMenu, type SelectMenuOption } from "../components/SelectMenu";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useWorktrees } from "../state/worktrees";
import { openWorktree } from "../utils/actions";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";
import type { MergeOutcome, RepoInfo, WorktreeInfo } from "../types/worktrees";

/**
 * Lands a worktree's branch into any other branch — a local `git merge --no-ff`.
 * Never pushes, never rewrites history, never touches the source worktree.
 *
 * Built for the Rider↔Maestro round-trip:
 * - a dirty target no longer blocks: its uncommitted state is parked in a
 *   snapshot, the merge runs clean, and the state is re-applied on top —
 *   whatever is open in Rider just gains the merge underneath it;
 * - a branch without a worktree is hosted by the primary, which is switched
 *   there and — after a clean merge — switched straight back, so an editor
 *   open on the primary never sees the branch change.
 */
export function MergeDialog({
  source,
  worktrees,
  repo,
  onClose,
}: {
  source: WorktreeInfo;
  worktrees: WorktreeInfo[];
  repo: RepoInfo | null;
  onClose: () => void;
}) {
  const merge = useWorktrees((s) => s.merge);
  useEscapeToClose(onClose);

  const checkedOut = useMemo(
    () => worktrees.filter((w) => w.branch && w.branch !== source.branch),
    [worktrees, source.branch],
  );

  /** Every branch that can host a merge, deduped: checked-out ones first (they
   * merge in place), then other local branches, then remote-only ones (both of
   * the latter route through the primary worktree). */
  const targetOptions = useMemo<SelectMenuOption[]>(() => {
    const seen = new Set<string>();
    const options: SelectMenuOption[] = [];

    for (const w of checkedOut) {
      const branch = w.branch as string;
      seen.add(branch);
      options.push({
        value: branch,
        label: branch,
        description: w.is_primary
          ? "Primary worktree"
          : w.status?.dirty
            ? "Its own worktree — has uncommitted changes"
            : "Its own worktree",
      });
    }
    for (const branch of repo?.branches ?? []) {
      if (branch === source.branch || seen.has(branch)) continue;
      seen.add(branch);
      options.push({
        value: branch,
        label: branch,
        description: "Via primary — will switch it to this branch",
      });
    }
    for (const remote of repo?.remote_branches ?? []) {
      // "origin/feature/x" → "feature/x"; git switch DWIM creates the local branch.
      const short = remote.slice(remote.indexOf("/") + 1);
      if (!short || short === source.branch || seen.has(short)) continue;
      seen.add(short);
      options.push({
        value: short,
        label: short,
        description: `From ${remote} — via primary, will switch it`,
      });
    }
    return options;
  }, [checkedOut, repo, source.branch]);

  const [targetBranch, setTargetBranch] = useState<string>(() => {
    // The branch this work forked from is what you almost always merge back into.
    const base = source.base_branch;
    if (base) {
      const short = base.includes("/") ? base.slice(base.indexOf("/") + 1) : base;
      for (const candidate of [base, short]) {
        if (
          candidate !== source.branch &&
          (checkedOut.some((w) => w.branch === candidate) ||
            repo?.branches.includes(candidate) ||
            repo?.remote_branches.some((r) => r.slice(r.indexOf("/") + 1) === candidate))
        ) {
          return candidate;
        }
      }
    }
    const primary = checkedOut.find((w) => w.is_primary);
    return (primary ?? checkedOut[0])?.branch ?? "";
  });
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<MergeOutcome | null>(null);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const target = checkedOut.find((w) => w.branch === targetBranch);
  const viaPrimary = !target;
  const primary = worktrees.find((w) => w.is_primary);
  const hostDirty = viaPrimary
    ? (primary?.status?.dirty ?? false)
    : (target?.status?.dirty ?? false);
  const sourceDirty = source.status?.dirty ?? false;

  const submit = async () => {
    if (!targetBranch || (viaPrimary && hostDirty)) return;
    setBusy(true);
    setSubmitError(null);
    const outcome = await merge(source.branch as string, targetBranch);
    setBusy(false);
    if (outcome) {
      setResult(outcome);
    } else {
      // merge() stored the failure on the sidebar banner; show it here too —
      // the modal covers that banner, and the user is looking at this dialog.
      setSubmitError(
        useWorktrees.getState().error ?? "Merge failed — see the sidebar for details.",
      );
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="arrow-up" /> Merge into…
        </h3>
        <p className="hint">
          Merges <code>{source.branch}</code> into the branch you pick — a regular merge commit,
          local only, never pushed. A branch without its own worktree is handled in the{" "}
          <strong>primary</strong> worktree: it switches to that branch first, so the result is
          right there when you open the project in Rider.
        </p>

        {targetOptions.length === 0 ? (
          <p className="hint warn">No other branch to merge into.</p>
        ) : !result ? (
          <div className="form-grid">
            <label>
              Target
              <SelectMenu
                value={targetBranch}
                onChange={(v) => {
                  setTargetBranch(v);
                  setSubmitError(null);
                }}
                options={targetOptions}
              />
            </label>

            {viaPrimary && targetBranch && (
              <p className="hint">
                <Icon name="branch" size={12} /> The primary worktree will host this merge on{" "}
                <code>{targetBranch}</code> and switch back to its own branch right after.
              </p>
            )}
            {viaPrimary && hostDirty && (
              <p className="hint warn">
                <Icon name="alert" size={12} /> The primary worktree has uncommitted changes —
                commit or discard them there before it can host a merge for another branch.
              </p>
            )}
            {!viaPrimary && hostDirty && (
              <p className="hint">
                <Icon name="history" size={12} /> <code>{targetBranch}</code> has uncommitted
                changes — they'll be parked in a snapshot for the merge and put back on top of the
                result automatically.
              </p>
            )}
            {sourceDirty && (
              <p className="hint warn">
                <Icon name="alert" size={12} /> <code>{source.branch}</code> has uncommitted changes
                of its own — only what's committed will be merged.
              </p>
            )}
            {submitError && <p className="hint warn">{submitError}</p>}
          </div>
        ) : result.merged ? (
          <>
            <p className="hint success">
              <Icon name="check" /> Merged <code>{source.branch}</code> into{" "}
              <code>{targetBranch}</code>.
              {result.restored && <> Uncommitted changes in the target were preserved on top.</>}
              {result.switched_back && (
                <>
                  {" "}
                  The primary hosted the merge and is already back on its own branch — nothing moved
                  under your editor.
                </>
              )}
              {result.switched_primary && !result.switched_back && (
                <>
                  {" "}
                  The primary worktree is now on <code>{targetBranch}</code>.
                </>
              )}
            </p>
            {result.parked_changes && (
              <p className="hint warn">
                <Icon name="alert" size={12} /> The target's uncommitted changes clashed with the
                merge result — they are safe in snapshot <code>{result.parked_changes}</code>{" "}
                (Snapshots dialog → restore when ready).
              </p>
            )}
            {!result.switched_back && (
              <p className="hint">
                <button
                  className="small"
                  onClick={() => openWorktree(targetBranch, "editor")}
                  title="Open the merged result in the editor"
                >
                  <Icon name="external-link" /> Open {targetBranch} in Rider
                </button>
              </p>
            )}
            {!source.is_primary && (
              <p className="hint">
                Task landed? The source worktree can go —{" "}
                <button
                  className="small danger"
                  onClick={() => {
                    void (async () => {
                      const remove = useWorktrees.getState().remove;
                      const outcome = await remove(source.branch as string, false);
                      if (outcome?.outcome === "dirty_confirmation_required") {
                        const ok = window.confirm(
                          `Worktree "${source.branch}" has uncommitted changes.\nRemove anyway and discard them?`,
                        );
                        if (ok) await remove(source.branch as string, true);
                        else return;
                      }
                      onClose();
                    })();
                  }}
                >
                  <Icon name="trash" /> Remove worktree
                </button>{" "}
                (the branch itself is kept).
              </p>
            )}
          </>
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
                  {result.switched_primary ? (
                    <>
                      The primary worktree is on <code>{targetBranch}</code>, mid-merge — resolve
                      right in Rider and commit, or run <code>git merge --abort</code> there to back
                      out.
                    </>
                  ) : (
                    <>
                      Resolve them in <code>{targetBranch}</code>'s worktree with your usual git
                      tools and commit — or run <code>git merge --abort</code> there to back out.
                    </>
                  )}
                </p>
                {result.parked_changes && (
                  <p className="hint warn">
                    <Icon name="alert" size={12} /> The target's uncommitted changes are parked in
                    snapshot <code>{result.parked_changes}</code> — restore them from the Snapshots
                    dialog after the conflict is settled.
                  </p>
                )}
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
          {targetOptions.length > 0 && !result && (
            <button
              className="btn-primary"
              disabled={busy || !targetBranch || (viaPrimary && hostDirty)}
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
