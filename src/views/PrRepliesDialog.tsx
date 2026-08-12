import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon, StatusDot } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { askMainAgent } from "../utils/agentAsk";
import { usePr, openUrl, type PrComment, type ReplyOutcome } from "../state/pr";
import { useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import { isTerminalStatus } from "../types/sessions";
import type { WorktreeInfo } from "../types/worktrees";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

const START_MODEL = "sonnet";
const START_EFFORT = "high";

/**
 * The review-comment round, built around the branch's persistent main agent:
 *
 * 1. Comments are fetched once, grouped by file, and sent to the branch's
 *    main agent — a real, visible chat session the user can discuss the
 *    plan with, ask questions in, or use to actually implement fixes.
 * 2. It discusses first: a plain-text summary of what it'd say and where,
 *    and whether a code change/commit is warranted — no tool call yet. Only
 *    once the human replies with something like "post the replies" does it
 *    call `submit_review_comments` itself, which raises a dedicated approval
 *    dialog over the chat (edit or drop any draft, then approve to post).
 * 3. The per-comment textareas below are for typing a reply by hand, without
 *    needing the agent's help at all — independent of step 2, and still the
 *    only way replies reach GitHub from *this* dialog. "Post" only sends the
 *    ones left non-empty.
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
  const renderCommitPrompt = usePr((s) => s.renderCommitPrompt);
  const postReplies = usePr((s) => s.postReplies);
  useEscapeToClose(onClose);

  const [loadingComments, setLoadingComments] = useState(true);
  const [comments, setComments] = useState<PrComment[]>([]);
  const [drafts, setDrafts] = useState<Record<number, string>>({});
  const [commitMessage, setCommitMessage] = useState("");
  const [startBusy, setStartBusy] = useState(false);
  const [genCommitBusy, setGenCommitBusy] = useState(false);
  const [postBusy, setPostBusy] = useState(false);
  const [phase, setPhase] = useState<string | null>(null);
  const [outcomes, setOutcomes] = useState<ReplyOutcome[] | null>(null);

  // Reactive: the status line updates live even while the user is on the
  // Chat tab actually talking to this session.
  const session = useSessions((s) =>
    (s.byBranch[branch] ?? []).find((sess) => sess.session_type === "main"),
  );

  useEffect(() => {
    void (async () => {
      setLoadingComments(true);
      const list = await listComments(branch);
      setComments(list ?? []);
      setLoadingComments(false);
    })();
    void useSessions.getState().fetch(branch);
  }, [branch, listComments]);

  const grouped = useMemo(() => {
    const byPath = new Map<string, PrComment[]>();
    for (const c of comments) {
      const key = c.path || "(general)";
      if (!byPath.has(key)) byPath.set(key, []);
      byPath.get(key)?.push(c);
    }
    return [...byPath.entries()];
  }, [comments]);

  const startReview = async () => {
    setStartBusy(true);
    try {
      let prompt =
        grouped
          .flatMap(([path, list]) => [
            `## ${path}`,
            ...list.map((c) => `[comment ${c.id}] ${c.author}:\n${c.body}\n${c.url}`),
            "",
          ])
          .join("\n") +
        `\nJudge whether each comment is actionable and correct. submit_review_comments ` +
        `is the only way a reply reaches GitHub: one entry per comment you want to reply ` +
        `to, in_reply_to set to that comment's id (the number in "[comment N]"), path/line ` +
        `matching that same comment's own, body your draft reply.`;
      const [gate, style] = await Promise.all([
        invoke<string>("render_review_workflow_gate").catch(() => ""),
        invoke<string>("render_review_reply_style").catch(() => ""),
      ]);
      if (gate.trim()) prompt += `\n\n${gate}`;
      if (style.trim()) prompt += `\n\n${style}`;
      const main = await useSessions.getState().ensureMain(branch);
      if (!main) return;
      await useSessions.getState().send(main.id, prompt);
      useWorktrees.getState().setTab("chat");
      onClose();
    } finally {
      setStartBusy(false);
    }
  };

  const genCommitMessage = async () => {
    setGenCommitBusy(true);
    try {
      const prompt = await renderCommitPrompt(branch, null);
      if (!prompt) return;
      const result = await askMainAgent({
        branch,
        prompt,
        model: START_MODEL,
        effort: START_EFFORT,
      });
      if (result?.text) setCommitMessage(result.text);
    } finally {
      setGenCommitBusy(false);
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
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
      <div className="modal replies-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="reply" /> Review comments · {branch}
        </h3>

        {loadingComments ? (
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
            <div className="replies-session-bar">
              <button
                className="small"
                disabled={startBusy}
                title="Send these comments to the branch's main agent"
                onClick={() => void startReview()}
              >
                {startBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Ask main agent
              </button>
              {session && (
                <>
                  <StatusDot tone={session.status} pulse={session.status === "streaming"} />
                  <span className="ac-desc">Main agent: {session.status}</span>
                  <button
                    className="small ghost"
                    onClick={() => {
                      useWorktrees.getState().setTab("chat");
                      onClose();
                    }}
                  >
                    <Icon name="chat" size={12} /> Open chat
                  </button>
                </>
              )}
            </div>

            <div className="replies-list">
              {grouped.map(([path, list]) => (
                <div key={path} className="reply-group">
                  <p className="reply-group-path">
                    <Icon name="file-text" size={12} /> {path}
                  </p>
                  {list.map((c) => (
                    <div key={c.id} className="reply-item">
                      <p className="reply-comment">
                        <strong>{c.author}</strong>
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
              ))}
            </div>

            {session && !isTerminalStatus(session.status) && (
              <p className="hint">
                The main agent discusses first, in the chat — it calls{" "}
                <code>submit_review_comments</code> only once you tell it to post, which raises
                a dialog for approving the drafts.
              </p>
            )}

            {dirty && (
              <label className="replies-commit">
                Uncommitted changes in this worktree — commit them in the same go (optional)
                <div className="gen-row">
                  <textarea
                    rows={2}
                    placeholder="Commit message (empty = don't commit) — or let it be generated"
                    value={commitMessage}
                    onChange={(e) => setCommitMessage(e.target.value)}
                  />
                  <button
                    className="small ghost"
                    disabled={genCommitBusy}
                    title="Ask the main agent to write it, with full context of what changed"
                    onClick={() => void genCommitMessage()}
                  >
                    {genCommitBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Generate
                  </button>
                </div>
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
