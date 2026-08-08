import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { usePr, openUrl, type CreatedPr } from "../state/pr";
import { useWorktrees } from "../state/worktrees";
import type { WorktreeInfo } from "../types/worktrees";

/**
 * One button from "pile of changes" to "PR is up": commit message on top
 * (typed or generated from the diff), PR title seeded from it, description
 * generated from the branch, then commit → push → `gh pr create` in a single
 * action. Generation runs through the editable `commit-message` /
 * `pr-description` prompt templates.
 */
export function CreatePrDialog({
  worktree,
  onClose,
}: {
  worktree: WorktreeInfo;
  onClose: () => void;
}) {
  const branch = worktree.branch as string;
  const dirty = worktree.status?.dirty ?? false;
  const generateCommitMessage = usePr((s) => s.generateCommitMessage);
  const generatePrDescription = usePr((s) => s.generatePrDescription);
  const createPr = usePr((s) => s.createPr);
  useEscapeToClose(onClose);

  const [commitMessage, setCommitMessage] = useState("");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [genCommitBusy, setGenCommitBusy] = useState(false);
  const [genPrBusy, setGenPrBusy] = useState(false);
  const [submitBusy, setSubmitBusy] = useState(false);
  const [phase, setPhase] = useState<string | null>(null);
  const [result, setResult] = useState<CreatedPr | null>(null);

  const needCommit = dirty;
  const canSubmit =
    !submitBusy && title.trim().length > 0 && (!needCommit || commitMessage.trim().length > 0);

  const genCommit = async () => {
    setGenCommitBusy(true);
    const message = await generateCommitMessage(branch);
    setGenCommitBusy(false);
    if (message) {
      setCommitMessage(message);
      if (!title.trim()) setTitle(message.split("\n")[0] ?? "");
    }
  };

  const genPr = async () => {
    setGenPrBusy(true);
    const draft = await generatePrDescription(branch);
    setGenPrBusy(false);
    if (draft) {
      if (draft.title) setTitle(draft.title);
      setBody(draft.body);
    }
  };

  const submit = async () => {
    setSubmitBusy(true);
    try {
      if (needCommit) {
        setPhase("Committing…");
        try {
          await invoke<string>("commit_worktree", { branch, message: commitMessage.trim() });
        } catch {
          return; // error toast already up; keep the dialog as-is
        }
      }
      setPhase("Pushing & creating the PR…");
      const created = await createPr(branch, title.trim(), body);
      if (created) {
        setResult(created);
        void useWorktrees.getState().refresh();
      }
    } finally {
      setSubmitBusy(false);
      setPhase(null);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal createpr-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="pr" /> Create PR · {branch}
        </h3>

        {result ? (
          <>
            <p className="hint success">
              <Icon name="check" /> Pull request created:{" "}
              <button className="small" onClick={() => void openUrl(result.url)}>
                <Icon name="external-link" /> {result.url}
              </button>
            </p>
            {result.push_report && <pre className="check-output">{result.push_report}</pre>}
          </>
        ) : (
          <div className="form-grid">
            {needCommit && (
              <label>
                Commit message — uncommitted changes will be committed first
                <div className="gen-row">
                  <textarea
                    rows={3}
                    placeholder="What and why — or let it be written from the diff"
                    value={commitMessage}
                    onChange={(e) => {
                      setCommitMessage(e.target.value);
                      if (!title.trim()) setTitle(e.target.value.split("\n")[0] ?? "");
                    }}
                  />
                  <button
                    className="small ghost"
                    disabled={genCommitBusy}
                    title="Generate from the diff (commit-message prompt template)"
                    onClick={() => void genCommit()}
                  >
                    {genCommitBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Generate
                  </button>
                </div>
              </label>
            )}

            <label>
              PR title
              <input
                type="text"
                placeholder={needCommit ? "Defaults to the commit summary" : "Imperative, short"}
                value={title}
                onChange={(e) => setTitle(e.target.value)}
              />
            </label>

            <label>
              Description
              <div className="gen-row">
                <textarea
                  rows={8}
                  placeholder="What changed, why, review notes — or generate it from the branch"
                  value={body}
                  onChange={(e) => setBody(e.target.value)}
                />
                <button
                  className="small ghost"
                  disabled={genPrBusy}
                  title="Generate title + description from the branch (pr-description prompt template)"
                  onClick={() => void genPr()}
                >
                  {genPrBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Generate
                </button>
              </div>
            </label>

            {phase && (
              <p className="hint">
                <Icon name="spinner" spin /> {phase}
              </p>
            )}
          </div>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            {result ? "Close" : "Cancel"}
          </button>
          {!result && (
            <button className="btn-primary" disabled={!canSubmit} onClick={() => void submit()}>
              {submitBusy
                ? "Working…"
                : needCommit
                  ? "Commit, push & create PR"
                  : "Push & create PR"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
