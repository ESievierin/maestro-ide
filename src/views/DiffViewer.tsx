import { useEffect, useRef, useState } from "react";
import { EditorState, type Extension } from "@codemirror/state";
import { EditorView, lineNumbers } from "@codemirror/view";
import { MergeView, unifiedMergeView } from "@codemirror/merge";
import { LanguageDescription } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { oneDark } from "@codemirror/theme-one-dark";
import { selectSnapshot, useDiffs } from "../state/diffs";
import type { ChangedFile, DiffScope } from "../types/diffs";
import type { WorktreeInfo } from "../types/worktrees";

type ViewMode = "split" | "unified";

const VIEW_MODE_KEY = "maestro.diffViewMode";

function loadViewMode(): ViewMode {
  return localStorage.getItem(VIEW_MODE_KEY) === "unified" ? "unified" : "split";
}

async function languageFor(path: string): Promise<Extension | null> {
  const description = LanguageDescription.matchFilename(languages, path);
  if (!description) return null;
  try {
    return await description.load();
  } catch {
    return null;
  }
}

function readOnlyExtensions(language: Extension | null): Extension[] {
  return [
    lineNumbers(),
    EditorView.editable.of(false),
    EditorState.readOnly.of(true),
    oneDark,
    ...(language ? [language] : []),
  ];
}

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
      const language = await languageFor(path);
      if (cancelled) return;
      view = new EditorView({
        state: EditorState.create({
          doc: newText,
          extensions: [
            ...readOnlyExtensions(language),
            unifiedMergeView({
              original: oldText,
              mergeControls: false,
              highlightChanges: false,
              gutter: true,
            }),
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

/** Rider-style side-by-side diff: old on the left, new on the right. */
function SplitFileDiff({
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
    let view: MergeView | null = null;
    let cancelled = false;

    void (async () => {
      const language = await languageFor(path);
      if (cancelled) return;
      view = new MergeView({
        a: {
          doc: oldText,
          extensions: readOnlyExtensions(language),
        },
        b: {
          doc: newText,
          extensions: readOnlyExtensions(language),
        },
        parent: host,
        gutter: true,
        // In split view, in-line change highlighting is what makes it readable.
        highlightChanges: true,
      });
    })();

    return () => {
      cancelled = true;
      view?.destroy();
    };
  }, [path, oldText, newText]);

  return <div className="cm-host cm-host-split" ref={ref} />;
}

export function DiffViewer({ worktree }: { worktree: WorktreeInfo }) {
  const branch = worktree.branch as string;
  const [scope, setScope] = useState<DiffScope>("worktree");
  const [viewMode, setViewMode] = useState<ViewMode>(loadViewMode);
  const snapshot = useDiffs(selectSnapshot(branch, scope));

  const changeViewMode = (mode: ViewMode) => {
    setViewMode(mode);
    localStorage.setItem(VIEW_MODE_KEY, mode);
  };
  const { fetch, refresh, loadFile, loading, error, clearError } = useDiffs();
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [filePair, setFilePair] = useState<{ path: string; old: string; new: string } | null>(null);

  useEffect(() => {
    // fetch is a stable zustand action; snapshot comes from the core cache.
    void fetch(branch, scope);
    setSelectedPath(null);
    setFilePair(null);
  }, [branch, scope, fetch]);

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
    void loadFile(branch, scope, selectedPath).then((diff) => {
      if (!stale && diff) {
        setFilePair({ path: diff.path, old: diff.old ?? "", new: diff.new ?? "" });
      }
    });
    return () => {
      stale = true;
    };
  }, [branch, scope, selectedPath, loadFile, snapshot]);

  return (
    <div className="diff-viewer">
      <div className="diff-toolbar">
        <div className="actions">
          <button
            className={`small ${scope === "worktree" ? "selected" : ""}`}
            onClick={() => setScope("worktree")}
            title="Merge-base → files on disk, including uncommitted and untracked"
          >
            Working tree
          </button>
          <button
            className={`small ${scope === "branch" ? "selected" : ""}`}
            onClick={() => setScope("branch")}
            title="Merge-base → branch head (committed only)"
          >
            Committed
          </button>
        </div>
        <span className="session-meta">
          {snapshot
            ? `vs ${snapshot.base} (merge-base ${snapshot.merge_base.slice(0, 8)}) · ${snapshot.files.length} files`
            : loading
              ? "computing…"
              : ""}
        </span>
        <div className="actions">
          <button
            className={`small ${viewMode === "split" ? "selected" : ""}`}
            onClick={() => changeViewMode("split")}
          >
            Split
          </button>
          <button
            className={`small ${viewMode === "unified" ? "selected" : ""}`}
            onClick={() => changeViewMode("unified")}
          >
            Unified
          </button>
          <button className="small" onClick={() => void refresh(branch, scope)} disabled={loading}>
            Refresh
          </button>
        </div>
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
              viewMode === "split" ? (
                <SplitFileDiff path={filePair.path} oldText={filePair.old} newText={filePair.new} />
              ) : (
                <UnifiedFileDiff
                  path={filePair.path}
                  oldText={filePair.old}
                  newText={filePair.new}
                />
              )
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
