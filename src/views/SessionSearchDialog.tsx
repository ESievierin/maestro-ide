import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Icon } from "../components/Icon";
import { useEscapeToClose } from "../hooks/useEscapeToClose";
import type { SessionSearchResult } from "../types/sessions";
import { useUI } from "../state/ui";
import { useWorktrees } from "../state/worktrees";

const MIN_QUERY_LENGTH = 2;
const DEBOUNCE_MS = 250;

/**
 * Substring search across every session's transcript, on every branch — not
 * just the one currently open. A real project accumulates history worth
 * finding again without remembering which branch it happened on.
 */
export function SessionSearchDialog({ onClose }: { onClose: () => void }) {
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<SessionSearchResult[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  useEscapeToClose(onClose);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    const trimmed = query.trim();
    if (trimmed.length < MIN_QUERY_LENGTH) {
      setResults([]);
      return;
    }
    const handle = setTimeout(() => {
      setBusy(true);
      invoke<SessionSearchResult[]>("search_sessions", { query: trimmed })
        .then(setResults)
        .catch((e) => setError(String(e)))
        .finally(() => setBusy(false));
    }, DEBOUNCE_MS);
    return () => clearTimeout(handle);
  }, [query]);

  const jumpTo = (r: SessionSearchResult) => {
    useWorktrees.getState().select(r.branch);
    useWorktrees.getState().setTab("chat");
    useUI.getState().closeDialog();
    onClose();
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal session-search-modal" onClick={(e) => e.stopPropagation()}>
        <h3>
          <Icon name="search" /> Search session history
        </h3>
        <input
          ref={inputRef}
          type="text"
          placeholder="Search every branch's prompts and replies…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />

        {error && (
          <p className="error-banner" onClick={() => setError(null)} title="Click to dismiss">
            {error}
          </p>
        )}

        {busy && (
          <p className="hint">
            <Icon name="spinner" spin /> Searching…
          </p>
        )}

        {!busy && query.trim().length >= MIN_QUERY_LENGTH && results.length === 0 && (
          <p className="empty">No sessions mention "{query.trim()}".</p>
        )}

        {query.trim().length > 0 && query.trim().length < MIN_QUERY_LENGTH && (
          <p className="hint">Keep typing — at least {MIN_QUERY_LENGTH} characters.</p>
        )}

        {results.length > 0 && (
          <ul className="session-search-results">
            {results.map((r) => (
              <li key={r.session_id} onClick={() => jumpTo(r)}>
                <div className="session-search-result-head">
                  <Icon name="branch" size={11} /> {r.branch}
                  <span className="badge badge-muted">{r.session_type}</span>
                  <span className={`badge badge-${r.status === "failed" ? "failed" : "muted"}`}>
                    {r.status.replace("_", " ")}
                  </span>
                </div>
                <p className="session-search-snippet">{r.snippet}</p>
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
