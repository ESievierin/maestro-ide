import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { EditorState, type Extension } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import { MergeView, goToNextChunk, goToPreviousChunk, unifiedMergeView } from "@codemirror/merge";
import { LanguageDescription } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import { oneDark } from "@codemirror/theme-one-dark";
import { Icon } from "../components/Icon";
import { SelectMenu } from "../components/SelectMenu";
import { useDiffJump } from "../state/diffJump";
import { selectSnapshot, useDiffs } from "../state/diffs";
import { selectQuestions, useQuestions } from "../state/questions";
import type { ChangedFile, DiffScope, LineEnding } from "../types/diffs";
import type { LineQuestion } from "../types/questions";
import type { WorktreeInfo } from "../types/worktrees";
import { BlastRadius } from "./BlastRadius";
import { ReviewGuide } from "./ReviewGuide";
import {
  type LineRange,
  lineQuestionsField,
  selectionListener,
  setLineQuestions,
} from "./diffQuestions";
import {
  extractFileDiff,
  parseDiffStats,
  parseFileHunks,
  type FileDiffStats,
  type HunkRange,
} from "./diffStats";

/** A slim strip beside the editor marking where every change sits in the
 * file — the same "you are here, and here's what else changed" overview
 * IDE diff viewers (Rider, VS Code) show next to the scrollbar. */
function ChangeOverview({
  hunks,
  totalLines,
  onJump,
}: {
  hunks: readonly HunkRange[];
  totalLines: number;
  onJump: (line: number) => void;
}) {
  if (totalLines <= 0 || hunks.length === 0) return null;
  return (
    <div className="diff-minimap">
      {hunks.map((h, i) => {
        const top = ((h.start - 1) / totalLines) * 100;
        const height = Math.max(0.8, ((h.end - h.start + 1) / totalLines) * 100);
        return (
          <button
            key={i}
            type="button"
            className="diff-minimap-mark"
            style={{ top: `${top}%`, height: `${height}%` }}
            title={`Line ${h.start}${h.end > h.start ? `–${h.end}` : ""}`}
            onClick={() => onJump(h.start)}
          />
        );
      })}
    </div>
  );
}

/** Reviewing a diff is mostly "walk the changes": jump to the next/previous
 * changed chunk, and when there isn't one, move to the next/previous file. */
function chunkNavKeymap(onBoundary: (direction: 1 | -1) => void) {
  return keymap.of([
    {
      key: "Mod-ArrowDown",
      run: (view) => {
        if (!goToNextChunk(view)) onBoundary(1);
        return true;
      },
    },
    {
      key: "Mod-ArrowUp",
      run: (view) => {
        if (!goToPreviousChunk(view)) onBoundary(-1);
        return true;
      },
    },
  ]);
}

function eolLabel(eol: LineEnding | null): string | null {
  if (!eol || eol === "none") return null;
  return eol === "lf" ? "LF" : eol === "crlf" ? "CRLF" : "Mixed";
}

/** A Rider-style line-ending indicator for the selected file: the plain
 * label when both sides agree, and an explicit "LF → CRLF" (or "Mixed")
 * warning when they don't — which is also the answer to "why does this file
 * show as changed when the diff looks empty": `--ignore-cr-at-eol` keeps the
 * visible diff clean, but git still (correctly) flags the blob as modified
 * when only the line endings differ. */
const EOL_OPTIONS = [
  { value: "lf", label: "LF" },
  { value: "crlf", label: "CRLF" },
];

/** One side of {@link EolSidesHeader}: a name, then either a picker (the
 * worktree file, when convertible) or a plain read-only badge. */
function EolSide({
  name,
  eol,
  onConvert,
  busy,
}: {
  name: string;
  eol: LineEnding | null;
  onConvert?: (eol: "lf" | "crlf") => void;
  busy?: boolean;
}) {
  const label = eolLabel(eol);
  return (
    <span className="eol-side">
      <span className="eol-side-name" title={name}>
        {name}
      </span>
      {onConvert && label ? (
        <SelectMenu
          title={`Line endings in ${name}: ${label}. Pick a style to rewrite the file on disk.`}
          value={eol as string}
          placeholder={label}
          disabled={busy}
          options={EOL_OPTIONS}
          onChange={(v) => onConvert(v as "lf" | "crlf")}
        />
      ) : (
        <span
          className={`badge ${eol === "mixed" ? "badge-warn" : "badge-muted"}`}
          title={label ? `Line endings in ${name}: ${label}` : `${name}: no file here`}
        >
          {label ?? "—"}
        </span>
      )}
    </span>
  );
}

/** Rider-style line-ending indicators for the selected file, one per side so
 * "which side has which line ending" never has to be inferred: left is the
 * base branch's blob (history — always read-only), right is the current
 * branch (the worktree file, picker-enabled — choosing a style rewrites it
 * on disk, same interaction as Rider's own line-separator selector). Also
 * the explanation for "why does this show as modified when the diff looks
 * empty": `--ignore-cr-at-eol` (in the core) keeps the visible diff clean,
 * but git still flags the blob as modified when only the line endings
 * differ between the two sides. */
function EolSidesHeader({
  baseName,
  oldEol,
  branchName,
  newEol,
  onConvert,
  busy,
}: {
  baseName: string;
  oldEol: LineEnding | null;
  branchName: string;
  newEol: LineEnding | null;
  onConvert?: (eol: "lf" | "crlf") => void;
  busy?: boolean;
}) {
  if ((!oldEol || oldEol === "none") && (!newEol || newEol === "none")) return null;
  return (
    <div className="eol-sides">
      <EolSide name={baseName} eol={oldEol} />
      <span className="eol-side-sep">→</span>
      <EolSide name={branchName} eol={newEol} onConvert={onConvert} busy={busy} />
    </div>
  );
}

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
  stats,
  selected,
  viewed,
  onClick,
  onToggleViewed,
}: {
  file: ChangedFile;
  stats?: FileDiffStats;
  selected: boolean;
  viewed: boolean;
  onClick: () => void;
  onToggleViewed: () => void;
}) {
  return (
    <li className={`${selected ? "selected" : ""} ${viewed ? "viewed" : ""}`} onClick={onClick}>
      <button
        type="button"
        className="file-viewed-toggle"
        title={viewed ? "Mark as not viewed" : "Mark as viewed"}
        onClick={(e) => {
          e.stopPropagation();
          onToggleViewed();
        }}
      >
        <Icon name={viewed ? "check" : "circle"} size={12} />
      </button>
      <span className={`file-status file-status-${file.status}`}>{file.status}</span>
      <span className="file-path" title={file.path}>
        {file.old_path ? `${file.old_path} → ${file.path}` : file.path}
      </span>
      {stats && (stats.additions > 0 || stats.deletions > 0) && (
        <span className="file-stats">
          {stats.additions > 0 && <span className="file-stat-add">+{stats.additions}</span>}
          {stats.deletions > 0 && <span className="file-stat-del">−{stats.deletions}</span>}
        </span>
      )}
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
  onViewReady,
  onBoundaryChunk,
}: {
  path: string;
  oldText: string;
  newText: string;
  questions: readonly LineQuestion[];
  onSelectionChange: (range: LineRange | null) => void;
  onViewReady: (view: EditorView | null) => void;
  onBoundaryChunk: (direction: 1 | -1) => void;
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
            chunkNavKeymap(onBoundaryChunk),
            unifiedMergeView({
              original: oldText,
              mergeControls: false,
              highlightChanges: false,
              gutter: true,
              collapseUnchanged: {},
              // Default scanLimit (500 changed chars) guards against a
              // quadratic blow-up on a wildly different file, but also trips
              // on an ordinary file with several scattered small edits,
              // collapsing the whole unresolved span into one giant change.
              diffConfig: { scanLimit: 30000, timeout: 3000 },
            }),
          ],
        }),
        parent: host,
      });
      viewRef.current = view;
      onViewReady(view);
      view.dispatch({ effects: setLineQuestions.of(questions) });
    })();

    return () => {
      cancelled = true;
      viewRef.current = null;
      onViewReady(null);
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
  onViewReady,
  onBoundaryChunk,
}: {
  path: string;
  oldText: string;
  newText: string;
  questions: readonly LineQuestion[];
  onSelectionChange: (range: LineRange | null) => void;
  onViewReady: (view: EditorView | null) => void;
  onBoundaryChunk: (direction: 1 | -1) => void;
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
            chunkNavKeymap(onBoundaryChunk),
          ],
        },
        parent: host,
        gutter: true,
        // In split view, in-line change highlighting is what makes it readable.
        highlightChanges: true,
        collapseUnchanged: {},
        // Default scanLimit (500 changed chars) guards against a quadratic
        // blow-up on a wildly different file, but also trips on an ordinary
        // file with several scattered small edits, bailing the precise
        // algorithm into one giant replacement chunk for the whole span.
        diffConfig: { scanLimit: 30000, timeout: 3000 },
      });
      viewRef.current = view;
      onViewReady(view.b);
      view.b.dispatch({ effects: setLineQuestions.of(questions) });
    })();

    return () => {
      cancelled = true;
      viewRef.current = null;
      onViewReady(null);
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
  const [filePair, setFilePair] = useState<{
    path: string;
    old: string;
    new: string;
    oldEol: LineEnding | null;
    newEol: LineEnding | null;
  } | null>(null);
  /** Set when the core refused to send a file's contents (see MAX_FILE_DIFF_BYTES). */
  const [tooLarge, setTooLarge] = useState<{ path: string; message: string } | null>(null);

  const askLineQuestion = useQuestions((s) => s.ask);
  const questions = useQuestions(selectQuestions(branch, selectedPath ?? ""));
  const [selection, setSelection] = useState<LineRange | null>(null);
  const [asking, setAsking] = useState(false);
  const [questionText, setQuestionText] = useState("");
  const handleSelectionChange = useCallback((range: LineRange | null) => setSelection(range), []);

  const [search, setSearch] = useState("");
  const [viewed, setViewed] = useState<Set<string>>(new Set());
  const [committing, setCommitting] = useState(false);
  const [commitMessage, setCommitMessage] = useState("");
  const [commitBusy, setCommitBusy] = useState(false);
  const [eolBusy, setEolBusy] = useState(false);
  const activeViewRef = useRef<EditorView | null>(null);
  const handleViewReady = useCallback((view: EditorView | null) => {
    activeViewRef.current = view;
  }, []);

  const stats = useMemo(() => parseDiffStats(snapshot?.unified ?? ""), [snapshot?.unified]);
  const hunks = useMemo(
    () => parseFileHunks(snapshot?.unified ?? "", selectedPath ?? ""),
    [snapshot?.unified, selectedPath],
  );
  const currentFileDiff = useMemo(
    () => extractFileDiff(snapshot?.unified ?? "", selectedPath ?? ""),
    [snapshot?.unified, selectedPath],
  );
  const totalLines = filePair?.new ? filePair.new.split("\n").length : 0;
  const jumpToLine = useCallback((line: number) => {
    const view = activeViewRef.current;
    if (!view) return;
    const clamped = Math.min(Math.max(1, line), view.state.doc.lines);
    const pos = view.state.doc.line(clamped).from;
    view.dispatch({
      selection: { anchor: pos },
      effects: EditorView.scrollIntoView(pos, { y: "center" }),
    });
    view.focus();
  }, []);
  const totals = useMemo(
    () =>
      Object.values(stats).reduce(
        (acc, s) => ({
          additions: acc.additions + s.additions,
          deletions: acc.deletions + s.deletions,
        }),
        { additions: 0, deletions: 0 },
      ),
    [stats],
  );
  const filteredFiles = useMemo(() => {
    const files = snapshot?.files ?? [];
    const q = search.trim().toLowerCase();
    if (!q) return files;
    return files.filter(
      (f) => f.path.toLowerCase().includes(q) || f.old_path?.toLowerCase().includes(q),
    );
  }, [snapshot?.files, search]);
  const viewedCount = useMemo(
    () => (snapshot?.files ?? []).filter((f) => viewed.has(f.path)).length,
    [snapshot?.files, viewed],
  );

  const toggleViewed = useCallback((path: string) => {
    setViewed((v) => {
      const next = new Set(v);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  /** Move to the next/previous file in the (filtered) list, wrapping around —
   * called directly from the toolbar, and as the fallback when a within-file
   * chunk-navigation command runs out of chunks in the current file. */
  const selectAdjacentFile = useCallback(
    (direction: 1 | -1) => {
      if (filteredFiles.length === 0) return;
      const idx = filteredFiles.findIndex((f) => f.path === selectedPath);
      const next = idx === -1 ? 0 : (idx + direction + filteredFiles.length) % filteredFiles.length;
      setSelectedPath(filteredFiles[next].path);
    },
    [filteredFiles, selectedPath],
  );

  const goToNextChange = useCallback(() => {
    const view = activeViewRef.current;
    if (view && !goToNextChunk(view)) selectAdjacentFile(1);
    else if (!view) selectAdjacentFile(1);
  }, [selectAdjacentFile]);
  const goToPreviousChange = useCallback(() => {
    const view = activeViewRef.current;
    if (view && !goToPreviousChunk(view)) selectAdjacentFile(-1);
    else if (!view) selectAdjacentFile(-1);
  }, [selectAdjacentFile]);

  useEffect(() => {
    // fetch is a stable zustand action; snapshot comes from the core cache.
    void fetch(branch, scope);
    setSelectedPath(null);
    setFilePair(null);
    setSearch("");
    setViewed(new Set());
  }, [branch, scope, fetch]);

  // Selecting a different file — or the file content changing under us (agent edits,
  // diff.updated refresh) — drops the in-progress selection and ask form: the line
  // numbers it captured no longer point at the same text.
  useEffect(() => {
    setSelection(null);
    setAsking(false);
    setQuestionText("");
  }, [selectedPath, filePair?.old, filePair?.new]);

  const submitQuestion = async () => {
    if (!selectedPath || !selection || !questionText.trim()) return;
    const asked = await askLineQuestion({
      branch,
      path: selectedPath,
      start: selection.start,
      end: selection.end,
      question: questionText.trim(),
      scope,
    });
    if (asked) {
      setAsking(false);
      setQuestionText("");
    }
  };

  const submitCommit = async () => {
    const message = commitMessage.trim();
    if (!message) return;
    setCommitBusy(true);
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      const summary = await invoke<string>("commit_worktree", { branch, message });
      setCommitting(false);
      setCommitMessage("");
      const { useToasts } = await import("../state/toasts");
      useToasts.getState().push({
        severity: "info",
        code: "committed",
        message: `Committed: ${summary}`,
      });
      // The working-tree diff is now empty and the committed diff grew; recompute
      // whichever is showing and let the sidebar's dirty badge clear.
      void refresh(branch, scope);
      const { useWorktrees } = await import("../state/worktrees");
      void useWorktrees.getState().refresh();
    } catch {
      // run_core already published error.raised — it shows as an error toast.
    } finally {
      setCommitBusy(false);
    }
  };

  const convertLineEnding = async (path: string, eol: "lf" | "crlf") => {
    setEolBusy(true);
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      await invoke("set_line_ending", { branch, path, eol });
      // The file on disk changed — refresh drops every cached file diff for
      // this branch, so the next load picks up the new content and EOL.
      await refresh(branch, scope);
      const diff = await loadFile(branch, scope, path);
      if (diff && !diff.too_large) {
        setFilePair({
          path: diff.path,
          old: diff.old ?? "",
          new: diff.new ?? "",
          oldEol: diff.old_eol,
          newEol: diff.new_eol,
        });
      }
      const { useWorktrees } = await import("../state/worktrees");
      void useWorktrees.getState().refresh();
    } catch {
      // run_core already published error.raised — it shows as an error toast.
    } finally {
      setEolBusy(false);
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

  // A "view diff" jump from the transcript wins over the plain auto-select
  // above (declared after it, in the same effect flush) — but only once the
  // file it names is actually one of this snapshot's changed files.
  const pendingJump = useDiffJump((s) => s.pending);
  useEffect(() => {
    if (!pendingJump || pendingJump.branch !== branch || !snapshot) return;
    if (snapshot.files.some((f) => f.path === pendingJump.path)) {
      setSelectedPath(pendingJump.path);
      useDiffJump.getState().clear();
    }
  }, [pendingJump, branch, snapshot]);

  // Load old/new contents for the selected file.
  useEffect(() => {
    if (!selectedPath) return;
    let stale = false;
    void loadFile(branch, scope, selectedPath).then((diff) => {
      if (stale || !diff) return;
      if (diff.too_large) {
        // Do not hand a multi-megabyte side to CodeMirror; say why instead.
        setTooLarge({ path: diff.path, message: diff.too_large });
        setFilePair(null);
        return;
      }
      setTooLarge(null);
      setFilePair({
        path: diff.path,
        old: diff.old ?? "",
        new: diff.new ?? "",
        oldEol: diff.old_eol,
        newEol: diff.new_eol,
      });
    });
    return () => {
      stale = true;
    };
  }, [branch, scope, selectedPath, loadFile, snapshot]);

  return (
    <div className="diff-viewer">
      <div className="diff-toolbar">
        <div className="actions">
          <div className="btn-group">
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
        </div>
        <span className="session-meta">
          {snapshot ? (
            <>
              vs {snapshot.base} (merge-base {snapshot.merge_base.slice(0, 8)}) ·{" "}
              {snapshot.files.length} files
              {(totals.additions > 0 || totals.deletions > 0) && (
                <>
                  {" · "}
                  <span className="file-stat-add">+{totals.additions}</span>{" "}
                  <span className="file-stat-del">−{totals.deletions}</span>
                </>
              )}
              {snapshot.files.length > 0 && ` · ${viewedCount}/${snapshot.files.length} viewed`}
            </>
          ) : loading ? (
            "computing…"
          ) : (
            ""
          )}
        </span>
        <div className="actions">
          <div className="btn-group">
            <button
              className="small icon-only"
              onClick={goToPreviousChange}
              disabled={!snapshot || snapshot.files.length === 0}
              title="Previous change (Ctrl+↑)"
            >
              <Icon name="arrow-up" size={13} />
            </button>
            <button
              className="small icon-only"
              onClick={goToNextChange}
              disabled={!snapshot || snapshot.files.length === 0}
              title="Next change (Ctrl+↓)"
            >
              <Icon name="arrow-down" size={13} />
            </button>
          </div>
          <div className="btn-group">
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
          </div>
          {selectedPath && (
            <button
              className="small icon-only ghost"
              title={`Open ${selectedPath} in the editor`}
              onClick={() => {
                void import("../utils/actions").then(({ openWorktree }) =>
                  openWorktree(branch, "editor", selectedPath),
                );
              }}
            >
              <Icon name="external-link" size={13} />
            </button>
          )}
          {selectedPath && (
            <button
              className="small icon-only ghost"
              title={`Copy diff for ${selectedPath}`}
              onClick={() => {
                void import("../utils/actions").then(({ copyDiff }) =>
                  copyDiff(selectedPath, currentFileDiff),
                );
              }}
            >
              <Icon name="copy" size={13} />
            </button>
          )}
          {(snapshot?.files.length ?? 0) > 1 && (
            <button
              className="small ghost"
              title={`Copy diff for all ${snapshot?.files.length} files`}
              onClick={() => {
                const allFiles = snapshot?.files.length ?? 0;
                void import("../utils/actions").then(({ copyDiff }) =>
                  copyDiff(`all ${allFiles} files`, snapshot?.unified ?? ""),
                );
              }}
            >
              <Icon name="copy" size={13} /> Copy all
            </button>
          )}
          <button
            className="small ghost"
            onClick={() => void refresh(branch, scope)}
            disabled={loading}
            title="Recompute this diff"
          >
            <Icon name={loading ? "spinner" : "refresh"} spin={loading} /> Refresh
          </button>
          {selection && !asking && (
            <button className="small btn-primary" onClick={() => setAsking(true)}>
              Ask about line{selection.start === selection.end ? "" : "s"} {selection.start}
              {selection.start === selection.end ? "" : `–${selection.end}`}
            </button>
          )}
          {scope === "worktree" && (snapshot?.files.length ?? 0) > 0 && !committing && (
            <button
              className="small btn-primary"
              title="Stage everything and commit in this worktree"
              onClick={() => setCommitting(true)}
            >
              <Icon name="check" /> Commit…
            </button>
          )}
        </div>
      </div>

      {committing && (
        <div className="line-question-form">
          <textarea
            autoFocus
            rows={2}
            placeholder="Commit message…"
            value={commitMessage}
            onChange={(e) => setCommitMessage(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                e.preventDefault();
                void submitCommit();
              }
            }}
          />
          <div className="actions">
            <button
              className="small btn-primary"
              onClick={() => void submitCommit()}
              disabled={commitBusy || !commitMessage.trim()}
            >
              {commitBusy ? "Committing…" : "Commit all"}
            </button>
            <button
              className="small ghost"
              disabled={commitBusy}
              onClick={() => {
                setCommitting(false);
                setCommitMessage("");
              }}
            >
              Cancel
            </button>
            <span className="hint">
              Stages every change in the worktree (Ctrl+Enter to commit).
            </span>
          </div>
        </div>
      )}

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
              className="small btn-primary"
              onClick={() => void submitQuestion()}
              disabled={!questionText.trim()}
            >
              Ask
            </button>
            <button
              className="small ghost"
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
          <div className="diff-files-panel">
            <ReviewGuide
              branch={branch}
              knownFiles={(snapshot?.files ?? []).map((f) => f.path)}
              onSelect={setSelectedPath}
            />
            {snapshot && snapshot.files.length > 1 && (
              <div className="diff-files-toolbar">
                {snapshot.files.length > 4 && (
                  <input
                    type="text"
                    className="diff-files-search"
                    placeholder="Filter files…"
                    value={search}
                    onChange={(e) => setSearch(e.target.value)}
                  />
                )}
                <button
                  className="small ghost"
                  title={
                    viewedCount === snapshot.files.length
                      ? "Mark every file as not viewed"
                      : "Mark every file as viewed"
                  }
                  onClick={() =>
                    setViewed(
                      viewedCount === snapshot.files.length
                        ? new Set()
                        : new Set(snapshot.files.map((f) => f.path)),
                    )
                  }
                >
                  {viewedCount === snapshot.files.length ? "Clear viewed" : "Mark all viewed"}
                </button>
              </div>
            )}
            <ul className="diff-files">
              {filteredFiles.map((file) => (
                <FileRow
                  key={file.path}
                  file={file}
                  stats={stats[file.path]}
                  selected={file.path === selectedPath}
                  viewed={viewed.has(file.path)}
                  onClick={() => setSelectedPath(file.path)}
                  onToggleViewed={() => toggleViewed(file.path)}
                />
              ))}
              {filteredFiles.length === 0 && (
                <li className="diff-files-empty">No files match “{search}”.</li>
              )}
            </ul>
            <BlastRadius branch={branch} />
          </div>
          <div className="diff-editor">
            {tooLarge && tooLarge.path === selectedPath ? (
              <p className="empty">{tooLarge.message}</p>
            ) : filePair && filePair.path === selectedPath ? (
              <div className="diff-editor-body">
                <div className="diff-editor-main">
                  <EolSidesHeader
                    baseName={snapshot?.base ?? "base"}
                    oldEol={filePair.oldEol}
                    branchName={branch}
                    newEol={filePair.newEol}
                    busy={eolBusy}
                    onConvert={
                      scope === "worktree"
                        ? (eol) => void convertLineEnding(selectedPath, eol)
                        : undefined
                    }
                  />
                  {viewMode === "split" ? (
                    <SplitFileDiff
                      path={filePair.path}
                      oldText={filePair.old}
                      newText={filePair.new}
                      questions={questions}
                      onSelectionChange={handleSelectionChange}
                      onViewReady={handleViewReady}
                      onBoundaryChunk={selectAdjacentFile}
                    />
                  ) : (
                    <UnifiedFileDiff
                      path={filePair.path}
                      oldText={filePair.old}
                      newText={filePair.new}
                      questions={questions}
                      onSelectionChange={handleSelectionChange}
                      onViewReady={handleViewReady}
                      onBoundaryChunk={selectAdjacentFile}
                    />
                  )}
                </div>
                <ChangeOverview hunks={hunks} totalLines={totalLines} onJump={jumpToLine} />
              </div>
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
