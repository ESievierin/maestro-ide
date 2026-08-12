import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { confirm, open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { Icon } from "../components/Icon";
import { SelectMenu } from "../components/SelectMenu";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import { useAttention } from "../state/attention";
import {
  comboFromEvent,
  HOTKEY_ACTIONS,
  isBareModifierKey,
  useHotkeyBindings,
} from "../state/hotkeys";
import { useUI } from "../state/ui";
import { closeOnBackdropMouseDown } from "../utils/backdropClose";

/** One rebindable shortcut: its current combo, a capture-next-keypress
 * rebind control, and a per-row reset when it differs from the default. */
function HotkeyRow({ action }: { action: (typeof HOTKEY_ACTIONS)[number] }) {
  const combo = useHotkeyBindings((s) => s.comboFor(action.id));
  const isCustom = useHotkeyBindings((s) => action.id in s.overrides);
  const [listening, setListening] = useState(false);
  const [conflict, setConflict] = useState<string | null>(null);

  return (
    <div className="hotkey-row">
      <span className="hotkey-row-label">{action.label}</span>
      {listening ? (
        <button
          type="button"
          className="small hotkey-capture"
          autoFocus
          onBlur={() => setListening(false)}
          onKeyDown={(e) => {
            e.preventDefault();
            e.stopPropagation();
            if (e.key === "Escape") {
              setListening(false);
              return;
            }
            if (isBareModifierKey(e.key)) return;
            const next = comboFromEvent(e);
            const owner = useHotkeyBindings.getState().actionBoundTo(next);
            if (owner && owner !== action.id) {
              const label = HOTKEY_ACTIONS.find((a) => a.id === owner)?.label ?? owner;
              setConflict(`"${next}" is already used by "${label}".`);
              return;
            }
            useHotkeyBindings.getState().setBinding(action.id, next);
            setConflict(null);
            setListening(false);
          }}
        >
          Press a key… (Esc to cancel)
        </button>
      ) : (
        <button
          type="button"
          className="small ghost hotkey-combo"
          onClick={() => {
            setConflict(null);
            setListening(true);
          }}
        >
          <kbd>{combo}</kbd>
        </button>
      )}
      {isCustom && !listening && (
        <button
          type="button"
          className="small icon-only ghost"
          title="Reset to default"
          onClick={() => useHotkeyBindings.getState().resetOne(action.id)}
        >
          <Icon name="refresh" size={11} />
        </button>
      )}
      {conflict && <span className="hint hotkey-conflict">{conflict}</span>}
    </div>
  );
}

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
interface BranchUsage {
  branch: string;
  totals: UsageTotals;
}

interface ImportSummary {
  settings_applied: number;
  prompts_written: number;
  settings_skipped: string[];
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
  const digestEnabled = useAttention((s) => s.digestEnabled);
  const setDigestEnabled = useAttention((s) => s.setDigestEnabled);
  const notifyError = useAttention((s) => s.error);

  const [telemetryEnabled, setTelemetryEnabledState] = useState<boolean | null>(null);
  const [writerPolicy, setWriterPolicyState] = useState<string | null>(null);
  const [branchNaming, setBranchNamingState] = useState<string | null>(null);
  const [finalizeTimeout, setFinalizeTimeoutState] = useState<number | null>(null);
  const [usage, setUsage] = useState<UsageSummary | null>(null);
  const [usageByBranch, setUsageByBranch] = useState<BranchUsage[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [backupBusy, setBackupBusy] = useState(false);
  const [backupMessage, setBackupMessage] = useState<string | null>(null);

  const refetchToggles = () => {
    void invoke<boolean>("get_telemetry_enabled").then(setTelemetryEnabledState);
    void invoke<string>("get_single_writer_policy").then(setWriterPolicyState);
    void invoke<string>("get_branch_naming").then(setBranchNamingState);
    void invoke<number>("get_notes_finalize_timeout").then(setFinalizeTimeoutState);
  };

  useEffect(() => {
    refetchToggles();
    void invoke<UsageSummary>("get_usage_summary").then(setUsage);
    void invoke<BranchUsage[]>("get_usage_by_branch").then(setUsageByBranch);
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

  const changeBranchNaming = async (raw: string) => {
    const template = raw.trim();
    if (!template) return; // an empty template would make every new branch fail to name itself
    setBranchNamingState(template);
    try {
      await invoke("set_branch_naming", { template });
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

  const exportBundle = async () => {
    const path = await saveFileDialog({
      title: "Export settings & prompts",
      defaultPath: "maestro-settings.json",
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!path) return;
    setBackupBusy(true);
    setBackupMessage(null);
    try {
      await invoke("export_settings_bundle", { path });
      setBackupMessage(`Exported to ${path}`);
    } catch (e) {
      setError(String(e));
    } finally {
      setBackupBusy(false);
    }
  };

  const importBundle = async () => {
    const picked = await openFileDialog({
      title: "Import settings & prompts",
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    const path = typeof picked === "string" ? picked : null;
    if (!path) return;
    const ok = await confirm(
      "Importing overwrites any matching settings and prompt templates with the file's versions. Continue?",
      { title: "MaestroIDE", kind: "warning" },
    );
    if (!ok) return;
    setBackupBusy(true);
    setBackupMessage(null);
    try {
      const summary = await invoke<ImportSummary>("import_settings_bundle", { path });
      setBackupMessage(
        `Imported ${summary.settings_applied} setting${summary.settings_applied === 1 ? "" : "s"} and ` +
          `${summary.prompts_written} prompt${summary.prompts_written === 1 ? "" : "s"}.`,
      );
      refetchToggles();
    } catch (e) {
      setError(String(e));
    } finally {
      setBackupBusy(false);
    }
  };

  return (
    <div className="modal-backdrop" onMouseDown={closeOnBackdropMouseDown(onClose)}>
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
              {usageByBranch && usageByBranch.length > 0 && (
                <details className="settings-usage-by-branch">
                  <summary>By branch ({usageByBranch.length})</summary>
                  <ul>
                    {usageByBranch.map((b) => (
                      <li key={b.branch}>
                        <span className="settings-usage-branch-name" title={b.branch}>
                          {b.branch}
                        </span>
                        <span className="settings-usage-branch-totals">
                          {formatUsage(b.totals)}
                        </span>
                      </li>
                    ))}
                  </ul>
                </details>
              )}
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
          {notificationsEnabled && (
            <label
              className="settings-toggle"
              title="Instead of one OS notification per item, wait a few seconds and group anything that arrived in that window into a single 'N items need you' notification — useful when several agents finish near the same time."
            >
              <input
                type="checkbox"
                checked={digestEnabled}
                onChange={(e) => void setDigestEnabled(e.target.checked)}
              />
              Group notifications that arrive close together
            </label>
          )}
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
            Branch naming template
            <input
              key={branchNaming ?? "loading"}
              type="text"
              defaultValue={branchNaming ?? ""}
              disabled={branchNaming === null}
              onBlur={(e) => void changeBranchNaming(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") e.currentTarget.blur();
              }}
            />
            <span className="hint">
              Used when creating a worktree without attaching an existing branch. Placeholders:{" "}
              <code>{"{type}"}</code> (impl/fix/…), <code>{"{task-id}"}</code>,{" "}
              <code>{"{slug}"}</code>.
            </span>
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

        <div className="settings-section">
          <h4>Keyboard shortcuts</h4>
          <p className="hint">
            Click a shortcut to rebind it. Alt+1…9 (select the nth worktree) is fixed.
          </p>
          <div className="hotkey-rows">
            {HOTKEY_ACTIONS.map((action) => (
              <HotkeyRow key={action.id} action={action} />
            ))}
          </div>
          <button
            type="button"
            className="small ghost"
            onClick={() => useHotkeyBindings.getState().resetAll()}
          >
            Reset all to defaults
          </button>
        </div>

        <div className="settings-section">
          <h4>Backup</h4>
          <p className="hint">
            Export the portable subset of settings (not repo paths, accounts, or the Jira token)
            plus every prompt template to a JSON file — carry it to another machine, or just keep it
            as a backup.
          </p>
          <div className="settings-backup-actions">
            <button className="small" disabled={backupBusy} onClick={() => void exportBundle()}>
              <Icon name="download" size={12} /> Export…
            </button>
            <button className="small" disabled={backupBusy} onClick={() => void importBundle()}>
              <Icon name="upload" size={12} /> Import…
            </button>
          </div>
          {backupMessage && <p className="settings-usage-line">{backupMessage}</p>}
          <button
            className="small ghost"
            onClick={() => {
              onClose();
              useUI.getState().openDialog("health-check");
            }}
          >
            <Icon name="shield" size={12} /> Run setup check…
          </button>
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
