import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { SelectMenu } from "../components/SelectMenu";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useAttention } from "../state/attention";

interface UsageTotals {
  cost_usd: number;
  turns: number;
  input_tokens: number;
  output_tokens: number;
}
interface UsageSummary {
  today: UsageTotals;
  all_time: UsageTotals;
}

function formatUsage(t: UsageTotals): string {
  return `$${t.cost_usd.toFixed(2)} · ${t.turns} turn${t.turns === 1 ? "" : "s"} · ${(
    t.input_tokens + t.output_tokens
  ).toLocaleString()} tokens`;
}

const WRITER_POLICY_OPTIONS = [
  {
    value: "read_only",
    label: "Downgrade to read-only",
    description: "A second writer session on the same branch starts read-only instead",
  },
  {
    value: "reject",
    label: "Reject outright",
    description: "Starting a second writer session on the same branch is refused",
  },
];

/**
 * App-wide toggles that used to be scattered across other panels (telemetry in
 * the daemon panel, notifications in the attention panel) or only reachable by
 * hand-editing config.toml (single-writer policy, notes finalize timeout).
 * Everything here is a `Store` setting on the backend — this dialog is just a
 * face for it, so it stays in sync with config.toml on the next restart too.
 */
export function SettingsDialog({ onClose }: { onClose: () => void }) {
  useEscapeToClose(onClose);
  const notificationsEnabled = useAttention((s) => s.notificationsEnabled);
  const setNotificationsEnabled = useAttention((s) => s.setNotificationsEnabled);
  const notifyError = useAttention((s) => s.error);

  const [telemetryEnabled, setTelemetryEnabledState] = useState<boolean | null>(null);
  const [writerPolicy, setWriterPolicyState] = useState<string | null>(null);
  const [finalizeTimeout, setFinalizeTimeoutState] = useState<number | null>(null);
  const [usage, setUsage] = useState<UsageSummary | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void invoke<boolean>("get_telemetry_enabled").then(setTelemetryEnabledState);
    void invoke<string>("get_single_writer_policy").then(setWriterPolicyState);
    void invoke<number>("get_notes_finalize_timeout").then(setFinalizeTimeoutState);
    void invoke<UsageSummary>("get_usage_summary").then(setUsage);
  }, []);

  const toggleTelemetry = async () => {
    if (telemetryEnabled === null) return;
    const next = !telemetryEnabled;
    setTelemetryEnabledState(next);
    try {
      await invoke("set_telemetry_enabled", { enabled: next });
    } catch (e) {
      setTelemetryEnabledState(!next);
      setError(String(e));
    }
  };

  const changeWriterPolicy = async (policy: string) => {
    setWriterPolicyState(policy);
    try {
      await invoke("set_single_writer_policy", { policy });
    } catch (e) {
      setError(String(e));
    }
  };

  const changeFinalizeTimeout = async (raw: string) => {
    const seconds = Math.max(0, Math.trunc(Number(raw) || 0));
    setFinalizeTimeoutState(seconds);
    try {
      await invoke("set_notes_finalize_timeout", { seconds });
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal settings-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="settings" /> Settings
        </h3>

        {(error || notifyError) && (
          <p className="error-banner" onClick={() => setError(null)} title="Click to dismiss">
            {error ?? notifyError}
          </p>
        )}

        <div className="settings-section">
          <h4>Usage</h4>
          {usage ? (
            <>
              <p className="settings-usage-line">
                <strong>Today:</strong> {formatUsage(usage.today)}
              </p>
              <p className="settings-usage-line">
                <strong>All time:</strong> {formatUsage(usage.all_time)}
              </p>
            </>
          ) : (
            <p className="hint">
              <Icon name="spinner" spin /> Loading…
            </p>
          )}
        </div>

        <div className="settings-section">
          <h4>Notifications &amp; telemetry</h4>
          <label
            className="settings-toggle"
            title="An OS notification when an agent blocks on a gate, permission, or fails"
          >
            <input
              type="checkbox"
              checked={notificationsEnabled}
              onChange={(e) => void setNotificationsEnabled(e.target.checked)}
            />
            OS notifications when an agent needs you
          </label>
          <label
            className="settings-toggle"
            title="Records every prompt and reply (and reasoning, when there is any) to ~/.maestro/telemetry for later analysis. Turning this off does not touch anything already written."
          >
            <input
              type="checkbox"
              checked={telemetryEnabled ?? true}
              disabled={telemetryEnabled === null}
              onChange={() => void toggleTelemetry()}
            />
            Record conversation telemetry
          </label>
        </div>

        <div className="settings-section">
          <h4>Session behavior</h4>
          <label className="settings-field">
            Second writer on the same branch
            <SelectMenu
              value={writerPolicy ?? "read_only"}
              options={WRITER_POLICY_OPTIONS}
              disabled={writerPolicy === null}
              onChange={(v) => void changeWriterPolicy(v)}
            />
          </label>
          <label className="settings-field">
            Notes finalize timeout (seconds, 0 disables)
            <input
              type="number"
              min={0}
              step={30}
              value={finalizeTimeout ?? ""}
              disabled={finalizeTimeout === null}
              onChange={(e) => void changeFinalizeTimeout(e.target.value)}
            />
            <span className="hint">
              How long a closing implementation session gets to write TASK_NOTES.md before it is
              closed anyway.
            </span>
          </label>
        </div>

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
