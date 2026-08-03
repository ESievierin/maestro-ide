import { useEffect, useRef, useState } from "react";
import { EditorState } from "@codemirror/state";
import { EditorView, lineNumbers } from "@codemirror/view";
import { unifiedMergeView } from "@codemirror/merge";
import { LanguageDescription } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { oneDark } from "@codemirror/theme-one-dark";
import { useDiffs } from "../state/diffs";
import type { ChangedFile } from "../types/diffs";
import type { WorktreeInfo } from "../types/worktrees";

function FileRow({
  file,
  selected,
  onClick,
}: {
  file: ChangedFile;
  selected: boolean;
  onClick: () => void;
}) {
  return (
    <li className={selected ? "selected" : ""} onClick={onClick}>
      <span className={`file-status file-status-${file.status}`}>{file.status}</span>
      <span className="file-path" title={file.path}>
        {file.old_path ? `${file.old_path} → ${file.path}` : file.path}
      </span>
    </li>
  );
}

/** Read-only CodeMirror unified diff of one file. */
function UnifiedFileDiff({
  path,
  oldText,
  newText,
}: {
  path: string;
  oldText: string;
  newText: string;
}) {
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const host = ref.current;
    if (!host) return;
    let view: EditorView | null = null;
    let cancelled = false;

    void (async () => {
      // Standard syntax highlighting by file extension (lazy-loaded grammars).
      const description = LanguageDescription.matchFilename(languages, path);
      let language = null;
      if (description) {
        try {
          language = await description.load();
        } catch {
          language = null;
        }
      }
      if (cancelled) return;

      view = new EditorView({
        state: EditorState.create({
          doc: newText,
          extensions: [
            lineNumbers(),
            EditorView.editable.of(false),
            EditorState.readOnly.of(true),
            oneDark,
            unifiedMergeView({
              original: oldText,
              mergeControls: false,
              // Line-level marks only — word-level highlighting is out of scope.
              highlightChanges: false,
              gutter: true,
            }),
            ...(language ? [language] : []),
          ],
        }),
        parent: host,
      });
    })();

    return () => {
      cancelled = true;
      view?.destroy();
    };
  }, [path, oldText, newText]);

  return <div className="cm-host" ref={ref} />;
}

export function DiffViewer({ worktree }: { worktree: WorktreeInfo }) {
  const branch = worktree.branch as string;
  const snapshot = useDiffs((s) => s.snapshots[branch]);
  const { fetch, refresh, loadFile, loading, error, clearError } = useDiffs();
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [filePair, setFilePair] = useState<{ path: string; old: string; new: string } | null>(null);

  useEffect(() => {
    // fetch is a stable zustand action; snapshot comes from the core cache.
    void fetch(branch);
    setSelectedPath(null);
    setFilePair(null);
  }, [branch, fetch]);

  // Keep the selection valid and auto-select the first file.
  useEffect(() => {
    if (!snapshot) return;
    const paths = snapshot.files.map((f) => f.path);
    if (selectedPath && !paths.includes(selectedPath)) {
      setSelectedPath(null);
      setFilePair(null);
    } else if (!selectedPath && paths.length > 0) {
      setSelectedPath(paths[0]);
    }
  }, [snapshot, selectedPath]);

  // Load old/new contents for the selected file.
  useEffect(() => {
    if (!selectedPath) return;
    let stale = false;
    void loadFile(branch, selectedPath).then((diff) => {
      if (!stale && diff) {
        setFilePair({ path: diff.path, old: diff.old ?? "", new: diff.new ?? "" });
      }
    });
    return () => {
      stale = true;
    };
  }, [branch, selectedPath, loadFile, snapshot]);

  return (
    <div className="diff-viewer">
      <div className="diff-toolbar">
        <span className="session-meta">
          {snapshot
            ? `vs ${snapshot.base} (merge-base ${snapshot.merge_base.slice(0, 8)}) · ${snapshot.files.length} files`
            : loading
              ? "computing…"
              : ""}
        </span>
        <button className="small" onClick={() => void refresh(branch)} disabled={loading}>
          Refresh
        </button>
      </div>

      {error && (
        <div className="error-banner" onClick={clearError} title="Click to dismiss">
          {error}
        </div>
      )}

      {snapshot && snapshot.files.length === 0 ? (
        <div className="main-empty">
          <p>No changes vs {snapshot.base}.</p>
        </div>
      ) : (
        <div className="diff-body">
          <ul className="diff-files">
            {snapshot?.files.map((file) => (
              <FileRow
                key={file.path}
                file={file}
                selected={file.path === selectedPath}
                onClick={() => setSelectedPath(file.path)}
              />
            ))}
          </ul>
          <div className="diff-editor">
            {filePair && filePair.path === selectedPath ? (
              <UnifiedFileDiff path={filePair.path} oldText={filePair.old} newText={filePair.new} />
            ) : selectedPath ? (
              <p className="empty">Loading {selectedPath}…</p>
            ) : (
              <p className="empty">Select a file.</p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
