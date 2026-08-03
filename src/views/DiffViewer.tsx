import { useCallback, useEffect, useRef, useState } from "react";
import { EditorState, type Extension } from "@codemirror/state";
import { EditorView, lineNumbers } from "@codemirror/view";
import { MergeView, unifiedMergeView } from "@codemirror/merge";
import { LanguageDescription } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { oneDark } from "@codemirror/theme-one-dark";
import { selectSnapshot, useDiffs } from "../state/diffs";
import { selectQuestions, useQuestions } from "../state/questions";
import type { ChangedFile, DiffScope } from "../types/diffs";
import type { LineQuestion } from "../types/questions";
import type { WorktreeInfo } from "../types/worktrees";
import {
  type LineRange,
  lineQuestionsField,
  selectionListener,
  setLineQuestions,
} from "./diffQuestions";

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

/** Read-only CodeMirror unified diff of one file. The single editor here shows the
 * "new" side, so line selection and question blocks attach directly to it. */
function UnifiedFileDiff({
  path,
  oldText,
  newText,
  questions,
  onSelectionChange,
}: {
  path: string;
  oldText: string;
  newText: string;
  questions: LineQuestion[];
  onSelectionChange: (range: LineRange | null) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const viewRef = useRef<EditorView | null>(null);

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
            lineQuestionsField,
            selectionListener(onSelectionChange),
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
      viewRef.current = view;
      view.dispatch({ effects: setLineQuestions.of(questions) });
    })();

    return () => {
      cancelled = true;
      viewRef.current = null;
      view?.destroy();
    };
  }, [path, oldText, newText]);

  useEffect(() => {
    viewRef.current?.dispatch({ effects: setLineQuestions.of(questions) });
  }, [questions]);

  return <div className="cm-host" ref={ref} />;
}

/** Rider-style side-by-side diff: old on the left, new on the right. Selection and
 * question blocks attach to the "new" (right) side only. */
function SplitFileDiff({
  path,
  oldText,
  newText,
  questions,
  onSelectionChange,
}: {
  path: string;
  oldText: string;
  newText: string;
  questions: LineQuestion[];
  onSelectionChange: (range: LineRange | null) => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const viewRef = useRef<MergeView | null>(null);

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
          extensions: [
            ...readOnlyExtensions(language),
            lineQuestionsField,
            selectionListener(onSelectionChange),
          ],
        },
        parent: host,
        gutter: true,
        // In split view, in-line change highlighting is what makes it readable.
        highlightChanges: true,
      });
      viewRef.current = view;
      view.b.dispatch({ effects: setLineQuestions.of(questions) });
    })();

    return () => {
      cancelled = true;
      viewRef.current = null;
      view?.destroy();
    };
  }, [path, oldText, newText]);

  useEffect(() => {
    viewRef.current?.b.dispatch({ effects: setLineQuestions.of(questions) });
  }, [questions]);

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

  const askLineQuestion = useQuestions((s) => s.ask);
  const questions = useQuestions(selectQuestions(branch, selectedPath ?? ""));
  const [selection, setSelection] = useState<LineRange | null>(null);
  const [asking, setAsking] = useState(false);
  const [questionText, setQuestionText] = useState("");
  const handleSelectionChange = useCallback((range: LineRange | null) => setSelection(range), []);

  useEffect(() => {
    // fetch is a stable zustand action; snapshot comes from the core cache.
    void fetch(branch, scope);
    setSelectedPath(null);
    setFilePair(null);
  }, [branch, scope, fetch]);

  // Selecting a different file drops the in-progress selection/ask form.
  useEffect(() => {
    setSelection(null);
    setAsking(false);
    setQuestionText("");
  }, [selectedPath]);

  const submitQuestion = async () => {
    if (!selectedPath || !selection || !questionText.trim()) return;
    const asked = await askLineQuestion({
      branch,
      path: selectedPath,
      start: selection.start,
      end: selection.end,
      question: questionText.trim(),
    });
    if (asked) {
      setAsking(false);
      setQuestionText("");
    }
  };

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
          {selection && !asking && (
            <button className="small" onClick={() => setAsking(true)}>
              Ask about line{selection.start === selection.end ? "" : "s"} {selection.start}
              {selection.start === selection.end ? "" : `–${selection.end}`}
            </button>
          )}
        </div>
      </div>

      {asking && selection && (
        <div className="line-question-form">
          <textarea
            autoFocus
            rows={2}
            placeholder={`Ask about line${selection.start === selection.end ? "" : "s"} ${selection.start}${selection.start === selection.end ? "" : `–${selection.end}`}…`}
            value={questionText}
            onChange={(e) => setQuestionText(e.target.value)}
          />
          <div className="actions">
            <button
              className="small"
              onClick={() => void submitQuestion()}
              disabled={!questionText.trim()}
            >
              Ask
            </button>
            <button
              className="small"
              onClick={() => {
                setAsking(false);
                setQuestionText("");
              }}
            >
              Cancel
            </button>
          </div>
        </div>
      )}

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
                <SplitFileDiff
                  path={filePair.path}
                  oldText={filePair.old}
                  newText={filePair.new}
                  questions={questions}
                  onSelectionChange={handleSelectionChange}
                />
              ) : (
                <UnifiedFileDiff
                  path={filePair.path}
                  oldText={filePair.old}
                  newText={filePair.new}
                  questions={questions}
                  onSelectionChange={handleSelectionChange}
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
