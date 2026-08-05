import { useState } from "react";
import { Icon } from "../components/Icon";
import { useWorktrees } from "../state/worktrees";
import type { RepoInfo } from "../types/worktrees";

interface Props {
  repo: RepoInfo;
  onClose: () => void;
}

export function CreateWorktreeDialog({ repo, onClose }: Props) {
  const createWorktree = useWorktrees((s) => s.create);

  const [mode, setMode] = useState<"new" | "attach">("new");
  const [kind, setKind] = useState("impl");
  const [taskId, setTaskId] = useState("");
  const [slug, setSlug] = useState("");
  const [base, setBase] = useState(repo.default_branch);
  const [existing, setExisting] = useState("");
  const [busy, setBusy] = useState(false);

  const baseOptions = [...repo.branches, ...repo.remote_branches];
  const repoIsEmpty = repo.branches.length === 0;

  const canSubmit =
    mode === "new"
      ? taskId.trim().length > 0 && slug.trim().length > 0 && base.trim().length > 0
      : existing.length > 0;

  const submit = async () => {
    setBusy(true);
    const ok = await createWorktree(
      mode === "new"
        ? {
            kind: kind.trim() || "impl",
            task_id: taskId.trim(),
            slug: slug.trim(),
            base: base.trim(),
          }
        : { existing_branch: existing },
    );
    setBusy(false);
    if (ok) onClose();
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="plus" /> New worktree
        </h3>

        {repoIsEmpty && (
          <p className="hint warn">
            This repository has no local branches yet (no commits?). Create an initial commit first
            — worktrees need an existing base.
          </p>
        )}

        <div className="mode-toggle">
          <label>
            <input type="radio" checked={mode === "new"} onChange={() => setMode("new")} />
            New branch
          </label>
          <label>
            <input type="radio" checked={mode === "attach"} onChange={() => setMode("attach")} />
            Existing branch
          </label>
        </div>

        {mode === "new" ? (
          <div className="form-grid">
            <label>
              Type
              <input value={kind} onChange={(e) => setKind(e.target.value)} placeholder="impl" />
            </label>
            <label>
              Task ID
              <input
                value={taskId}
                onChange={(e) => setTaskId(e.target.value)}
                placeholder="T-42"
              />
            </label>
            <label>
              Slug
              <input
                value={slug}
                onChange={(e) => setSlug(e.target.value)}
                placeholder="diff viewer"
              />
            </label>
            <label>
              Base branch
              <input
                list="base-branch-options"
                value={base}
                onChange={(e) => setBase(e.target.value)}
                placeholder="start typing to search…"
              />
              <datalist id="base-branch-options">
                {baseOptions.map((b) => (
                  <option key={b} value={b} />
                ))}
              </datalist>
            </label>
            <p className="hint">
              Branch:{" "}
              <code>{`${kind || "impl"}/${taskId || "…"}-${slug ? slug.toLowerCase().replace(/[^a-z0-9]+/g, "-") : "…"}`}</code>
            </p>
          </div>
        ) : (
          <div className="form-grid">
            <label>
              Branch
              <input
                list="attach-branch-options"
                value={existing}
                onChange={(e) => setExisting(e.target.value)}
                placeholder="start typing to search…"
              />
              <datalist id="attach-branch-options">
                {repo.branches.map((b) => (
                  <option key={b} value={b} />
                ))}
              </datalist>
            </label>
            <p className="hint">
              Local branches only. Reattaches stored task state for this branch, if any.
            </p>
          </div>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn-primary"
            disabled={!canSubmit || busy}
            onClick={() => void submit()}
          >
            {busy ? "Creating…" : "Create"}
          </button>
        </div>
      </div>
    </div>
  );
}
