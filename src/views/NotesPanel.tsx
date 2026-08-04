import { useEffect } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Icon } from "../components/Icon";
import { useNotes } from "../state/notes";
import type { WorktreeInfo } from "../types/worktrees";

/**
 * `TASK_NOTES.md` of the selected worktree, read-only.
 *
 * This is the hand-off channel between agents: an implementation session writes it as its
 * last act, and whoever picks the branch up next (a review-fix session, or the user) reads
 * the reasoning instead of guessing it from the diff. Editing happens where it belongs — in
 * the file, by an agent or by hand — so this panel only reads.
 */
export function NotesPanel({ worktree }: { worktree: WorktreeInfo }) {
  const branch = worktree.branch as string;
  const notes = useNotes((s) => s.byBranch[branch]);
  const loading = useNotes((s) => s.loading[branch]);
  const error = useNotes((s) => s.error);
  const fetch = useNotes((s) => s.fetch);
  const refresh = useNotes((s) => s.refresh);
  const clearError = useNotes((s) => s.clearError);

  useEffect(() => {
    // fetch is a stable zustand action.
    void fetch(branch);
  }, [branch, fetch]);

  return (
    <div className="notes-panel">
      <div className="panel-header">
        <h2>
          Task notes
          {notes?.updated_at && (
            <span className="count">{new Date(notes.updated_at).toLocaleString()}</span>
          )}
        </h2>
        <button className="small" disabled={!!loading} onClick={() => void refresh(branch)}>
          <Icon name="refresh" spin={!!loading} /> Refresh
        </button>
      </div>

      {error && (
        <div className="error-banner" onClick={clearError} title="Click to dismiss">
          {error}
        </div>
      )}

      {notes?.path && (
        <span className="repo-line" title={notes.path}>
          {notes.path}
        </span>
      )}

      {notes?.unavailable ? (
        <p className="empty">{notes.unavailable}</p>
      ) : notes?.exists ? (
        <div className="notes-body md">
          <ReactMarkdown remarkPlugins={[remarkGfm]}>{notes.raw}</ReactMarkdown>
        </div>
      ) : (
        <p className="empty">
          No <code>TASK_NOTES.md</code> yet — it is written when an implementation session closes,
          and it is committed with the branch like any other file.
        </p>
      )}
    </div>
  );
}
