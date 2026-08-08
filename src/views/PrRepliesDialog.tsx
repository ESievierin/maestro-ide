import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { usePr, openUrl, type PrComment, type ReplyOutcome } from "../state/pr";
import { useWorktrees } from "../state/worktrees";
import type { WorktreeInfo } from "../types/worktrees";

/**
 * The review-comment round: every comment on this branch's PR with an editable
 * reply draft next to it. Drafts can be generated (pr-reply prompt template,
 * fed the branch diff), edited freely, and posted all at once — optionally
 * committing pending fixes in the same click. Nothing is posted until that
 * click: this dialog IS the human-in-the-loop step.
 */
export function PrRepliesDialog({
  worktree,
  onClose,
}: {
  worktree: WorktreeInfo;
  onClose: () => void;
}) {
  const branch = worktree.branch as string;
  const dirty = worktree.status?.dirty ?? false;
  const listComments = usePr((s) => s.listComments);
  const generateReplies = usePr((s) => s.generateReplies);
  const postReplies = usePr((s) => s.postReplies);
  useEscapeToClose(onClose);

  const [loading, setLoading] = useState(true);
  const [comments, setComments] = useState<PrComment[]>([]);
  const [drafts, setDrafts] = useState<Record<number, string>>({});
  const [commitMessage, setCommitMessage] = useState("");
  const [genBusy, setGenBusy] = useState(false);
  const [postBusy, setPostBusy] = useState(false);
  const [phase, setPhase] = useState<string | null>(null);
  const [outcomes, setOutcomes] = useState<ReplyOutcome[] | null>(null);

  useEffect(() => {
    void (async () => {
      setLoading(true);
      const list = await listComments(branch);
      setComments(list ?? []);
      setLoading(false);
    })();
  }, [branch, listComments]);

  const generate = async () => {
    setGenBusy(true);
    const generated = await generateReplies(branch, comments);
    setGenBusy(false);
    if (generated) {
      // Generated text fills gaps; anything the user already typed wins.
      setDrafts((existing) => {
        const next = { ...existing };
        for (const [id, text] of Object.entries(generated)) {
          const key = Number(id);
          if (!next[key]?.trim()) next[key] = text;
        }
        return next;
      });
    }
  };

  const filled = comments.filter((c) => drafts[c.id]?.trim());
  const canPost = !postBusy && filled.length > 0;

  const post = async () => {
    if (filled.length === 0) return;
    setPostBusy(true);
    try {
      if (dirty && commitMessage.trim()) {
        setPhase("Committing…");
        try {
          await invoke<string>("commit_worktree", { branch, message: commitMessage.trim() });
          void useWorktrees.getState().refresh();
        } catch {
          return; // toast is up; drafts stay
        }
      }
      setPhase(`Posting ${filled.length} repl${filled.length === 1 ? "y" : "ies"}…`);
      const results = await postReplies(
        comments[0]?.pr ?? 0,
        filled.map((c) => ({ comment_id: c.id, body: drafts[c.id].trim() })),
      );
      if (results) setOutcomes(results);
    } finally {
      setPostBusy(false);
      setPhase(null);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal replies-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="reply" /> Review comments · {branch}
        </h3>

        {loading ? (
          <p className="empty">
            <Icon name="spinner" spin /> Loading the PR's review comments…
          </p>
        ) : comments.length === 0 ? (
          <p className="empty">
            No review comments — either there is no open PR for this branch, or nobody commented
            yet.
          </p>
        ) : outcomes ? (
          <div className="replies-outcomes">
            {outcomes.map((o) => {
              const comment = comments.find((c) => c.id === o.comment_id);
              return (
                <p key={o.comment_id} className={o.ok ? "hint success" : "hint warn"}>
                  {o.ok ? <Icon name="check" size={12} /> : <Icon name="alert" size={12} />} Reply
                  to {comment?.author ?? "?"} on <code>{comment?.path || "PR"}</code>:{" "}
                  {o.ok ? (
                    <button className="small ghost" onClick={() => void openUrl(o.detail)}>
                      posted <Icon name="external-link" size={11} />
                    </button>
                  ) : (
                    o.detail
                  )}
                </p>
              );
            })}
          </div>
        ) : (
          <>
            <div className="replies-toolbar">
              <button
                className="small"
                disabled={genBusy}
                title="Draft a reply per comment from the branch diff (pr-reply prompt template)"
                onClick={() => void generate()}
              >
                {genBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Generate replies
              </button>
              <span className="ac-desc">
                {filled.length}/{comments.length} drafted — empty drafts are skipped
              </span>
            </div>

            <div className="replies-list">
              {comments.map((c) => (
                <div key={c.id} className="reply-item">
                  <p className="reply-comment">
                    <strong>{c.author}</strong>
                    {c.path && (
                      <>
                        {" on "}
                        <code>{c.path}</code>
                      </>
                    )}
                    <button
                      className="small icon-only ghost"
                      title="Open the comment on GitHub"
                      onClick={() => void openUrl(c.url)}
                    >
                      <Icon name="external-link" size={11} />
                    </button>
                  </p>
                  <blockquote className="reply-quote">{c.body}</blockquote>
                  <textarea
                    rows={2}
                    placeholder="Reply (leave empty to skip)"
                    value={drafts[c.id] ?? ""}
                    onChange={(e) => setDrafts((d) => ({ ...d, [c.id]: e.target.value }))}
                  />
                </div>
              ))}
            </div>

            {dirty && (
              <label className="replies-commit">
                Uncommitted changes in this worktree — commit them in the same go (optional)
                <input
                  type="text"
                  placeholder="Commit message (empty = don't commit)"
                  value={commitMessage}
                  onChange={(e) => setCommitMessage(e.target.value)}
                />
              </label>
            )}

            {phase && (
              <p className="hint">
                <Icon name="spinner" spin /> {phase}
              </p>
            )}
          </>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            {outcomes ? "Close" : "Cancel"}
          </button>
          {!outcomes && comments.length > 0 && (
            <button className="btn-primary" disabled={!canPost} onClick={() => void post()}>
              {postBusy
                ? "Working…"
                : dirty && commitMessage.trim()
                  ? `Commit & post ${filled.length} repl${filled.length === 1 ? "y" : "ies"}`
                  : `Post ${filled.length} repl${filled.length === 1 ? "y" : "ies"}`}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
