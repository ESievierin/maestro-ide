import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";

interface HealthCheck {
  name: string;
  ok: boolean;
  detail: string;
}

interface HealthReport {
  checks: HealthCheck[];
}

const LABELS: Record<string, string> = {
  git: "git",
  gh: "GitHub CLI",
  editor: "Editor",
  jira: "Jira",
  repository: "Repository",
};

/**
 * A read-only snapshot of whether the environment this app depends on is
 * actually working — most useful right after moving to a new machine
 * (pairs with Settings → Backup's export/import: move the setup, then check
 * it actually works here). Nothing here calls a network API or changes
 * anything.
 */
export function HealthCheckDialog({ onClose }: { onClose: () => void }) {
  const [report, setReport] = useState<HealthReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEscapeToClose(onClose);

  const run = () => {
    setBusy(true);
    setError(null);
    invoke<HealthReport>("run_health_check")
      .then(setReport)
      .catch((e) => setError(String(e)))
      .finally(() => setBusy(false));
  };

  useEffect(run, []);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal health-check-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="check" /> Setup check
        </h3>
        <p className="hint">
          Whether the tools this app depends on are actually working on this machine — useful after
          moving your setup here with Settings → Backup.
        </p>

        {error && (
          <p className="error-banner" onClick={() => setError(null)} title="Click to dismiss">
            {error}
          </p>
        )}

        {busy && !report ? (
          <p className="hint">
            <Icon name="spinner" spin /> Checking…
          </p>
        ) : (
          report && (
            <ul className="health-check-list">
              {report.checks.map((c) => (
                <li key={c.name} className={c.ok ? "health-check-ok" : "health-check-fail"}>
                  <Icon name={c.ok ? "check" : "alert"} size={13} />
                  <span className="health-check-name">{LABELS[c.name] ?? c.name}</span>
                  <span className="health-check-detail">{c.detail}</span>
                </li>
              ))}
            </ul>
          )
        )}

        <div className="modal-actions">
          <button className="small ghost" disabled={busy} onClick={run}>
            <Icon name={busy ? "spinner" : "refresh"} spin={busy} /> Re-check
          </button>
          <button className="ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
