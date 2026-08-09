import { useEffect, useMemo, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Icon } from "../components/Icon";
import { usePr } from "../state/pr";
import { useSessions } from "../state/sessions";
import { waitForTerminal } from "../utils/agentAsk";
import type {
  DialogAnswer,
  DialogQuestion,
  ElicitationPayload,
  ReviewCommentDraft,
  UserDialog,
} from "../types/sessions";
import {
  asElicitation,
  asPlanText,
  asQuestionPayload,
  asReviewCommentsPayload,
  DIALOG_ASK_USER_QUESTION,
  DIALOG_ELICITATION,
  DIALOG_PLAN_APPROVAL,
  DIALOG_REVIEW_COMMENTS,
  isTerminalStatus,
  isWriterMode,
} from "../types/sessions";

/** Per-question UI state: chosen option labels plus the free-text field. */
interface Draft {
  picked: string[];
  text: string;
}

const emptyDraft: Draft = { picked: [], text: "" };

/**
 * Build the answer the agent gets. A question answers as its selected option labels
 * (comma-separated for multi-select, which is the shape the CLI documents), or as the
 * free text when nothing was selected — that is the "Other" path. Text alongside a
 * selection becomes a note instead of replacing the answer.
 */
function buildAnswer(questions: DialogQuestion[], drafts: Record<string, Draft>): DialogAnswer {
  const answers: Record<string, string> = {};
  const annotations: DialogAnswer["annotations"] = {};
  for (const q of questions) {
    const draft = drafts[q.question] ?? emptyDraft;
    const text = draft.text.trim();
    if (draft.picked.length > 0) {
      answers[q.question] = draft.picked.join(", ");
      const preview = q.options.find((o) => o.label === draft.picked[0])?.preview;
      if (preview || text) {
        annotations[q.question] = { ...(preview && { preview }), ...(text && { notes: text }) };
      }
    } else if (text) {
      answers[q.question] = text;
    }
  }
  const hasAnnotations = Object.keys(annotations).length > 0;
  return { answers, ...(hasAnnotations && { annotations }) };
}

function QuestionCard({
  question,
  draft,
  onChange,
}: {
  question: DialogQuestion;
  draft: Draft;
  onChange: (next: Draft) => void;
}) {
  const toggle = (label: string) => {
    if (question.multiSelect) {
      const picked = draft.picked.includes(label)
        ? draft.picked.filter((l) => l !== label)
        : [...draft.picked, label];
      onChange({ ...draft, picked });
      return;
    }
    onChange({ ...draft, picked: draft.picked[0] === label ? [] : [label] });
  };

  const preview = question.options.find(
    (o) => o.preview && draft.picked.includes(o.label),
  )?.preview;

  return (
    <div className="q-card">
      <div className="q-head">
        <span className="q-chip">{question.header}</span>
        {question.multiSelect && <span className="q-multi">pick any</span>}
      </div>
      <p className="q-text">{question.question}</p>
      <div className="q-options">
        {question.options.map((option) => (
          <button
            key={option.label}
            className={`q-option ${draft.picked.includes(option.label) ? "picked" : ""}`}
            onClick={() => toggle(option.label)}
          >
            <span className="q-option-label">
              {question.multiSelect ? (
                <Icon name={draft.picked.includes(option.label) ? "check" : "square"} />
              ) : (
                <Icon name={draft.picked.includes(option.label) ? "check" : "circle"} />
              )}
              {option.label}
            </span>
            <span className="q-option-desc">{option.description}</span>
          </button>
        ))}
      </div>
      {preview && <pre className="q-preview">{preview}</pre>}
      <input
        type="text"
        className="q-other"
        placeholder={
          draft.picked.length > 0
            ? "Add a note for the agent (optional)…"
            : "Other: type your own answer…"
        }
        value={draft.text}
        onChange={(e) => onChange({ ...draft, text: e.target.value })}
      />
    </div>
  );
}

/**
 * An MCP server asking for something — usually finishing an OAuth flow in the browser.
 * Structured (form) requests carry a schema Maestro cannot render, so those can only be
 * declined; saying so beats a dialog with a disabled button and no explanation.
 */
function ElicitationRequestView({
  dialog,
  request,
}: {
  dialog: UserDialog;
  request: ElicitationPayload;
}) {
  const respondDialog = useSessions((s) => s.respondDialog);
  const [busy, setBusy] = useState(false);

  const submit = async (approved: boolean) => {
    setBusy(true);
    await respondDialog(dialog.sessionId, { approved });
    setBusy(false);
  };

  return (
    <div className="modal-backdrop">
      <div className="modal">
        <h3>
          <Icon name="shield" /> {request.title ?? `${request.server} needs your approval`}
        </h3>
        <p className="q-text">{request.message}</p>
        {request.description && <p className="hint">{request.description}</p>}
        {request.url && (
          <p className="hint">
            Open this in your browser, then come back and approve:
            <br />
            <code className="elicit-url">{request.url}</code>
          </p>
        )}
        {request.form && (
          <p className="hint warn">
            <Icon name="alert" /> This server wants structured input, which Maestro cannot render
            yet — declining is the only safe answer.
          </p>
        )}
        <div className="modal-actions">
          <button className="ghost" disabled={busy} onClick={() => void submit(false)}>
            Decline
          </button>
          {!request.form && (
            <button className="btn-primary" disabled={busy} onClick={() => void submit(true)}>
              {busy ? "Sending…" : "Approve"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * The plan the agent wants to act on. Approving it is what takes the session out of plan
 * mode, so the core re-checks the branch's single-writer rule first and the approval can
 * come back refused — the dialog stays open in that case, with the reason in the banner.
 *
 * A refusal almost always means another session on the same branch is already writing.
 * Detecting that client-side (instead of only learning it from the failure) lets this
 * offer to close the other one and retry in one click, rather than sending the user off
 * to find it themselves.
 */
function PlanReview({
  dialog,
  plan,
  branch,
}: {
  dialog: UserDialog;
  plan: string;
  branch: string;
}) {
  const respondDialog = useSessions((s) => s.respondDialog);
  const close = useSessions((s) => s.close);
  const [notes, setNotes] = useState("");
  const [busy, setBusy] = useState(false);
  const [closingConflict, setClosingConflict] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const conflictingWriter = useSessions((s) =>
    (s.byBranch[branch] ?? []).find(
      (sess) =>
        sess.id !== dialog.sessionId &&
        isWriterMode(sess.permission_mode) &&
        !isTerminalStatus(sess.status),
    ),
  );

  const submit = async (answer: DialogAnswer) => {
    setBusy(true);
    setSubmitError(null);
    await respondDialog(dialog.sessionId, answer);
    setBusy(false);
    if (useSessions.getState().dialogs[dialog.sessionId]) {
      // Still pending after the call: the approval was refused, not just slow.
      setSubmitError(useSessions.getState().error ?? "Could not start writing — try again.");
    }
  };

  const closeConflictAndApprove = async () => {
    if (!conflictingWriter) return;
    setClosingConflict(true);
    setSubmitError(null);
    await close(conflictingWriter.id);
    const cleared = await waitForTerminal(conflictingWriter.id, branch);
    setClosingConflict(false);
    if (!cleared) {
      setSubmitError(
        "The other session did not finish closing in time — check its tab, then try again.",
      );
      return;
    }
    await submit({ approved: true, ...(notes.trim() && { feedback: notes }) });
  };

  return (
    <div className="modal-backdrop">
      <div className="modal q-modal">
        <h3>
          <Icon name="file-text" /> Plan ready for review
        </h3>
        <div className="plan-body md">
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{plan}</ReactMarkdown>
        </div>
        {conflictingWriter && (
          <p className="hint warn">
            <Icon name="alert" size={12} /> Another session is already writing on this branch —
            approving needs it closed first.
          </p>
        )}
        {submitError && (
          <p className="hint warn">
            <Icon name="alert" size={12} /> {submitError}
          </p>
        )}
        <textarea
          className="plan-notes"
          rows={2}
          placeholder="What to change (sent to the agent if you keep planning)…"
          value={notes}
          onChange={(e) => setNotes(e.target.value)}
        />
        <div className="modal-actions">
          <span className="hint">Approving lets this session start writing.</span>
          <button
            className="ghost"
            disabled={busy || closingConflict}
            onClick={() => void submit({ approved: false, feedback: notes })}
          >
            Keep planning
          </button>
          {conflictingWriter ? (
            <button
              className="btn-primary"
              disabled={closingConflict}
              onClick={() => void closeConflictAndApprove()}
            >
              {closingConflict ? "Closing the other session…" : "Close it and approve"}
            </button>
          ) : (
            <button
              className="btn-primary"
              disabled={busy}
              onClick={() =>
                void submit({ approved: true, ...(notes.trim() && { feedback: notes }) })
              }
            >
              {busy ? "Starting…" : "Approve"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * Draft PR comments the agent wants to leave — new findings anchored to a
 * file+line, or replies to comments that already exist. This *is* the plan
 * for this kind of session: instead of writing a summary to a file, the
 * agent calls `submit_review_comments` with its exact proposed comments, the
 * human edits or drops any of them here, and approving is what posts —
 * nothing reaches GitHub any other way.
 */
function ReviewCommentsForm({
  dialog,
  payload,
}: {
  dialog: UserDialog;
  payload: { pr: number; comments: ReviewCommentDraft[]; summary?: string };
}) {
  const respondDialog = useSessions((s) => s.respondDialog);
  const postReviewComments = usePr((s) => s.postReviewComments);
  const [drafts, setDrafts] = useState<ReviewCommentDraft[]>(payload.comments);
  const [busy, setBusy] = useState(false);

  const updateBody = (index: number, body: string) => {
    setDrafts((d) => d.map((c, i) => (i === index ? { ...c, body } : c)));
  };
  const removeRow = (index: number) => {
    setDrafts((d) => d.filter((_, i) => i !== index));
  };

  const decline = async () => {
    setBusy(true);
    await respondDialog(dialog.sessionId, {
      approved: false,
      feedback: "The human declined to post these comments.",
    });
    setBusy(false);
  };

  const approveAndPost = async () => {
    const toPost = drafts.filter((c) => c.body.trim().length > 0);
    if (toPost.length === 0) return;
    setBusy(true);
    const outcome = await postReviewComments(
      payload.pr,
      toPost.map((c) => ({
        path: c.path,
        line: c.line,
        side: c.side,
        body: c.body.trim(),
        in_reply_to: c.in_reply_to,
      })),
    );
    const feedback =
      outcome === null
        ? "Posting failed — see the app's error banner for details; nothing was posted."
        : outcome.failed.length === 0
          ? `Posted ${outcome.posted} comment${outcome.posted === 1 ? "" : "s"}.`
          : `Posted ${outcome.posted} comment${outcome.posted === 1 ? "" : "s"}; ${outcome.failed.length} failed: ${outcome.failed.join("; ")}`;
    await respondDialog(dialog.sessionId, {
      approved: outcome !== null && outcome.posted > 0,
      feedback,
      comments: toPost,
    });
    setBusy(false);
  };

  return (
    <div className="modal-backdrop">
      <div className="modal q-modal review-comments-modal">
        <h3>
          <Icon name="reply" /> Draft review comments — PR #{payload.pr}
        </h3>
        {payload.summary && <p className="hint">{payload.summary}</p>}
        <ul className="review-comments-list">
          {drafts.map((c, i) => (
            <li key={i} className="review-comment-row">
              <div className="review-comment-meta">
                <code className="review-comment-location">
                  {c.path}:{c.line}
                  {c.side === "LEFT" ? " (base)" : ""}
                </code>
                {c.in_reply_to && (
                  <span className="badge badge-info">reply to #{c.in_reply_to}</span>
                )}
                <button
                  type="button"
                  className="small icon-only ghost"
                  title="Drop this comment — it will not be posted"
                  onClick={() => removeRow(i)}
                >
                  <Icon name="trash" size={12} />
                </button>
              </div>
              <textarea rows={2} value={c.body} onChange={(e) => updateBody(i, e.target.value)} />
            </li>
          ))}
          {drafts.length === 0 && <li className="hint">Every draft comment was dropped.</li>}
        </ul>
        <div className="modal-actions">
          <span className="hint">
            {drafts.filter((c) => c.body.trim()).length} of {payload.comments.length} will post.
          </span>
          <button className="ghost" disabled={busy} onClick={() => void decline()}>
            Decline
          </button>
          <button
            className="btn-primary"
            disabled={busy || drafts.filter((c) => c.body.trim()).length === 0}
            onClick={() => void approveAndPost()}
          >
            {busy ? "Posting…" : "Approve & post"}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * The blocking dialog the agent raised. Renders the kinds Maestro understands and
 * dismisses anything else — an unrendered dialog would leave the agent parked.
 */
export function QuestionDialog({ dialog, branch }: { dialog: UserDialog; branch: string }) {
  const respondDialog = useSessions((s) => s.respondDialog);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [clarify, setClarify] = useState("");
  const [busy, setBusy] = useState(false);

  const payload = useMemo(
    () =>
      dialog.dialogKind === DIALOG_ASK_USER_QUESTION ? asQuestionPayload(dialog.payload) : null,
    [dialog.dialogKind, dialog.payload],
  );
  const plan = useMemo(
    () => (dialog.dialogKind === DIALOG_PLAN_APPROVAL ? asPlanText(dialog.payload) : null),
    [dialog.dialogKind, dialog.payload],
  );
  const elicitation = useMemo(
    () => (dialog.dialogKind === DIALOG_ELICITATION ? asElicitation(dialog.payload) : null),
    [dialog.dialogKind, dialog.payload],
  );
  const reviewComments = useMemo(
    () =>
      dialog.dialogKind === DIALOG_REVIEW_COMMENTS ? asReviewCommentsPayload(dialog.payload) : null,
    [dialog.dialogKind, dialog.payload],
  );

  // A kind (or payload) we cannot render must still be answered, or the turn hangs.
  useEffect(() => {
    if (!payload && !plan && !elicitation && !reviewComments) {
      void respondDialog(dialog.sessionId, null);
    }
  }, [payload, plan, elicitation, reviewComments, dialog.sessionId, respondDialog]);

  const questions = payload?.questions ?? [];
  const answered = questions.filter((q) => {
    const draft = drafts[q.question] ?? emptyDraft;
    return draft.picked.length > 0 || draft.text.trim().length > 0;
  }).length;

  if (plan) return <PlanReview dialog={dialog} plan={plan} branch={branch} />;
  if (elicitation) return <ElicitationRequestView dialog={dialog} request={elicitation} />;
  if (reviewComments) return <ReviewCommentsForm dialog={dialog} payload={reviewComments} />;
  if (!payload) return null;

  const submit = async (answer: DialogAnswer | null) => {
    setBusy(true);
    await respondDialog(dialog.sessionId, answer);
    setBusy(false);
  };

  return (
    <div className="modal-backdrop">
      <div className="modal q-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="question" /> The agent needs your input
        </h3>
        {questions.map((q) => (
          <QuestionCard
            key={q.question}
            question={q}
            draft={drafts[q.question] ?? emptyDraft}
            onChange={(next) => setDrafts((d) => ({ ...d, [q.question]: next }))}
          />
        ))}

        <details className="q-clarify">
          <summary>Not the right question? Reply instead</summary>
          <textarea
            rows={3}
            placeholder="Tell the agent what to clarify; it reformulates instead of taking an answer."
            value={clarify}
            onChange={(e) => setClarify(e.target.value)}
          />
        </details>

        <div className="modal-actions">
          <span className="hint">
            {answered}/{questions.length} answered
          </span>
          <button className="ghost" disabled={busy} onClick={() => void submit(null)}>
            Dismiss
          </button>
          {clarify.trim().length > 0 ? (
            <button
              className="btn-primary"
              disabled={busy}
              onClick={() => void submit({ feedback: clarify })}
            >
              Send reply
            </button>
          ) : (
            <button
              className="btn-primary"
              disabled={busy || answered === 0}
              onClick={() => void submit(buildAnswer(questions, drafts))}
            >
              {busy ? "Sending…" : "Answer"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
