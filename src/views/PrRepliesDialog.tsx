import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon, StatusDot } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { askViaFollowup, findResumableSession } from "../utils/agentAsk";
import { usePr, openUrl, type PrComment, type ReplyOutcome } from "../state/pr";
import { useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import type { WorktreeInfo } from "../types/worktrees";

/** Parse `[reply to <id>]\n<text>` blocks into id → text. A block whose id
 * isn't among the comments we actually asked about is dropped — a
 * hallucinated id must not create a reply. */
function parseReplyDrafts(raw: string, knownIds: number[]): Record<number, string> {
  const known = new Set(knownIds);
  const drafts: Record<number, string> = {};
  let current: number | null = null;
  let buffer: string[] = [];
  const flush = () => {
    if (current !== null) {
      const text = buffer.join("\n").trim();
      if (known.has(current) && text) drafts[current] = text;
    }
    buffer = [];
  };
  for (const line of raw.split("\n")) {
    const match = /^\[reply to\s*(\d+)\]$/.exec(line.trim());
    if (match) {
      flush();
      current = Number(match[1]);
    } else if (current !== null) {
      buffer.push(line);
    }
  }
  flush();
  return drafts;
}

const START_MODEL = "sonnet";
const START_EFFORT = "high";
const REPLY_EFFORT = "xhigh";

/**
 * The review-comment round, built around one persistent session per PR:
 *
 * 1. Comments are fetched once, grouped by file, and handed to a
 *    `review_fix` session (resumed from the branch's implementation session
 *    when one exists) — a real, visible chat session the user can discuss
 *    the plan with, ask questions in, or use to actually implement fixes.
 * 2. "Generate replies" sends that same session a follow-up asking for the
 *    final `[reply to id]` drafts, at a bumped reasoning effort — editable,
 *    and re-runnable with extra clarifications.
 * 3. "Post" is the only thing that ever reaches GitHub, and only for the
 *    drafts left non-empty.
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
  const renderReplyFollowup = usePr((s) => s.renderReplyFollowup);
  const postReplies = usePr((s) => s.postReplies);
  useEscapeToClose(onClose);

  const [loadingComments, setLoadingComments] = useState(true);
  const [comments, setComments] = useState<PrComment[]>([]);
  const [drafts, setDrafts] = useState<Record<number, string>>({});
  const [extra, setExtra] = useState("");
  const [commitMessage, setCommitMessage] = useState("");
  const [startBusy, setStartBusy] = useState(false);
  const [genBusy, setGenBusy] = useState(false);
  const [postBusy, setPostBusy] = useState(false);
  const [phase, setPhase] = useState<string | null>(null);
  const [outcomes, setOutcomes] = useState<ReplyOutcome[] | null>(null);

  // Reactive: the status line updates live even while the user is on the
  // Chat tab actually talking to this session.
  const session = useSessions((s) => {
    const reviews = (s.byBranch[branch] ?? []).filter((sess) => sess.session_type === "review_fix");
    return reviews.length > 0 ? reviews[reviews.length - 1] : undefined;
  });
  const sessionTerminal = session && ["done", "failed", "cancelled"].includes(session.status);

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
      await useSessions.getState().fetch(branch);
      // Prefer continuing a prior review conversation (it already has the
      // implementer's context baked in); otherwise resume the implementer
      // directly.
      const resumeFrom = findResumableSession(branch, ["review_fix", "implementation"])?.id;
      const prompt = grouped
        .flatMap(([path, list]) => [
          `## ${path}`,
          ...list.map((c) => `[comment ${c.id}] ${c.author}:\n${c.body}\n${c.url}`),
          "",
        ])
        .join("\n");
      const spawned = await useSessions.getState().spawn({
        branch,
        prompt,
        session_type: "review_fix",
        model: START_MODEL,
        effort: START_EFFORT,
        resume_from: resumeFrom,
      });
      if (spawned) {
        useWorktrees.getState().setTab("chat");
        onClose();
      }
    } finally {
      setStartBusy(false);
    }
  };

  const generateReplies = async () => {
    if (!session) return;
    setGenBusy(true);
    setPhase("Asking the review session for final replies…");
    try {
      const prompt = await renderReplyFollowup(extra.trim() || undefined);
      if (!prompt) return;
      const { text } = await askViaFollowup({
        sessionId: session.id,
        prompt,
        effort: REPLY_EFFORT,
      });
      if (text) {
        setDrafts(
          parseReplyDrafts(
            text,
            comments.map((c) => c.id),
          ),
        );
      }
    } finally {
      setGenBusy(false);
      setPhase(null);
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
              {!session ? (
                <button className="small" disabled={startBusy} onClick={() => void startReview()}>
                  {startBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />} Start review
                  session
                </button>
              ) : (
                <>
                  <StatusDot tone={session.status} pulse={session.status === "streaming"} />
                  <span className="ac-desc">Review session: {session.status}</span>
                  <button
                    className="small ghost"
                    onClick={() => {
                      useWorktrees.getState().setTab("chat");
                      onClose();
                    }}
                  >
                    <Icon name="chat" size={12} /> Open chat
                  </button>
                  {sessionTerminal && (
                    <button
                      className="small ghost"
                      disabled={startBusy}
                      title="Continue this conversation in a new session"
                      onClick={() => void startReview()}
                    >
                      {startBusy ? <Icon name="spinner" spin /> : <Icon name="refresh" size={12} />}{" "}
                      Start a new one
                    </button>
                  )}
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

            {session && !sessionTerminal && (
              <div className="replies-generate">
                <textarea
                  rows={2}
                  placeholder="Optional clarifications for regenerating the replies…"
                  value={extra}
                  onChange={(e) => setExtra(e.target.value)}
                />
                <button
                  className="small"
                  disabled={genBusy}
                  title="Ask the review session for its final reply drafts"
                  onClick={() => void generateReplies()}
                >
                  {genBusy ? <Icon name="spinner" spin /> : <Icon name="bot" />}{" "}
                  {Object.keys(drafts).length > 0 ? "Regenerate replies" : "Generate replies"}
                </button>
              </div>
            )}

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
