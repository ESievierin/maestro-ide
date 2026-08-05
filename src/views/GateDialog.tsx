import { useState } from "react";
import { Icon } from "../components/Icon";
import { useGates } from "../state/gates";
import type { PendingGate } from "../types/gates";

/** The raw command (Bash tool) or pretty-printed args for anything else. */
function commandOf(gate: PendingGate): string {
  if (gate.raw_args && typeof gate.raw_args === "object" && "command" in gate.raw_args) {
    const command = (gate.raw_args as { command?: unknown }).command;
    if (typeof command === "string") return command;
  }
  return JSON.stringify(gate.raw_args, null, 2);
}

function GateModal({ gate, queued }: { gate: PendingGate; queued: number }) {
  const respond = useGates((s) => s.respond);
  const error = useGates((s) => s.error);
  const [values, setValues] = useState<Record<string, string>>(() =>
    Object.fromEntries(gate.params.map((p) => [p.key, p.value])),
  );
  const [feedback, setFeedback] = useState("");
  const [busy, setBusy] = useState(false);

  const submit = async (allow: boolean) => {
    setBusy(true);
    const edited = gate.params.map((p) => ({ ...p, value: values[p.key] ?? p.value }));
    await respond(gate.gate_id, allow, edited, feedback.trim() || undefined);
    setBusy(false);
  };

  return (
    <div className="modal-backdrop">
      <div className="modal gate-modal">
        <h3>
          <Icon name="shield" /> Approval required: {gate.kind}
        </h3>
        <p className="gate-context">
          {gate.branch || "unknown branch"} · session {gate.session_id.slice(0, 8)} · {gate.tool}
        </p>
        <pre className="gate-command">
          <code>{commandOf(gate)}</code>
        </pre>
        {gate.note && <p className="hint warn">Not editable: {gate.note}</p>}
        <div className="form-grid">
          {gate.params.map((p) => (
            <label key={p.key}>
              {p.label}
              {p.multiline ? (
                <textarea
                  rows={6}
                  value={values[p.key] ?? ""}
                  onChange={(e) => setValues((v) => ({ ...v, [p.key]: e.target.value }))}
                />
              ) : (
                <input
                  value={values[p.key] ?? ""}
                  onChange={(e) => setValues((v) => ({ ...v, [p.key]: e.target.value }))}
                />
              )}
            </label>
          ))}
          <label>
            Feedback on deny (optional)
            <input
              value={feedback}
              placeholder="Sent back to the agent when you deny"
              onChange={(e) => setFeedback(e.target.value)}
            />
          </label>
        </div>
        {error && <p className="hint warn">{error}</p>}
        {queued > 0 && (
          <p className="hint gate-queue-hint">
            {queued} more pending approval{queued > 1 ? "s" : ""}
          </p>
        )}
        <div className="modal-actions">
          <button className="danger" disabled={busy} onClick={() => void submit(false)}>
            Deny
          </button>
          <button className="btn-primary" disabled={busy} onClick={() => void submit(true)}>
            Allow
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Global gate queue: whenever the core pauses a gated tool call, the oldest
 * pending gate is shown as a modal until the queue is empty.
 */
export function GateDialog() {
  const pending = useGates((s) => s.pending);
  if (pending.length === 0) return null;
  const gate = pending[0];
  // Key by gate id so param edits reset when the next gate comes up.
  return <GateModal key={gate.gate_id} gate={gate} queued={pending.length - 1} />;
}
