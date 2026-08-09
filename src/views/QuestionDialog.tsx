import { useEffect, useMemo, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Icon } from "../components/Icon";
import { useSessions } from "../state/sessions";
import { waitForTerminal } from "../utils/agentAsk";
import type {
  DialogAnswer,
  DialogQuestion,
  ElicitationPayload,
  UserDialog,
} from "../types/sessions";
import {
  asElicitation,
  asPlanText,
  asQuestionPayload,
  DIALOG_ASK_USER_QUESTION,
  DIALOG_ELICITATION,
  DIALOG_PLAN_APPROVAL,
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

  // A kind (or payload) we cannot render must still be answered, or the turn hangs.
  useEffect(() => {
    if (!payload && !plan && !elicitation) void respondDialog(dialog.sessionId, null);
  }, [payload, plan, elicitation, dialog.sessionId, respondDialog]);

  const questions = payload?.questions ?? [];
  const answered = questions.filter((q) => {
    const draft = drafts[q.question] ?? emptyDraft;
    return draft.picked.length > 0 || draft.text.trim().length > 0;
  }).length;

  if (plan) return <PlanReview dialog={dialog} plan={plan} branch={branch} />;
  if (elicitation) return <ElicitationRequestView dialog={dialog} request={elicitation} />;
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
