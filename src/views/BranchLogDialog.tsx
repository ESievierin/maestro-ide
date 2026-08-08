import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";

interface LogEntry {
  sha: string;
  subject: string;
  author: string;
  date: string;
}

/**
 * "What exactly is on this branch": the commits its base doesn't have, newest
 * first — the pre-merge / pre-push sanity read.
 */
export function BranchLogDialog({ branch, onClose }: { branch: string; onClose: () => void }) {
  const [entries, setEntries] = useState<LogEntry[] | null>(null);
  useEscapeToClose(onClose);

  useEffect(() => {
    let stale = false;
    void invoke<LogEntry[]>("branch_log", { branch })
      .then((log) => {
        if (!stale) setEntries(log);
      })
      .catch(() => {
        if (!stale) setEntries([]); // failure already surfaced as an error toast
      });
    return () => {
      stale = true;
    };
  }, [branch]);

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal check-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="log" /> Commits · {branch}
        </h3>
        <p className="hint">Commits on this branch that its base branch does not have.</p>

        {entries === null ? (
          <p className="empty">Loading…</p>
        ) : entries.length === 0 ? (
          <p className="empty">No commits over the base — everything here is already merged.</p>
        ) : (
          <ul className="branch-log">
            {entries.map((entry) => (
              <li key={entry.sha}>
                <code className="log-sha">{entry.sha}</code>
                <span className="log-subject" title={entry.subject}>
                  {entry.subject}
                </span>
                <span className="ac-desc">
                  {entry.author} · {entry.date}
                </span>
              </li>
            ))}
          </ul>
        )}

        <div className="modal-actions">
          <button className="ghost" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
