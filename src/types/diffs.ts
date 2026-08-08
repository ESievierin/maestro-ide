// Mirrors the Rust types in src-tauri/src/core/diff/ and core/worktree/provider.rs.

export interface ChangedFile {
  path: string;
  /** Single-letter git status: A, M, D, R, … */
  status: string;
  old_path: string | null;
}

export type DiffScope = "branch" | "worktree";

export interface DiffSnapshot {
  branch: string;
  scope: DiffScope;
  base: string;
  merge_base: string;
  files: ChangedFile[];
  unified: string;
  computed_at: string;
}

/** `"lf" | "crlf" | "mixed" | "none"` — detected before any normalization. */
export type LineEnding = "lf" | "crlf" | "mixed" | "none";

export interface FileDiff {
  path: string;
  old: string | null;
  new: string | null;
  /** Set when the core refused to send the contents; render this, not an empty editor. */
  too_large: string | null;
  /** Line-ending style of the merge-base content. `null` for an added file. */
  old_eol: LineEnding | null;
  /** Line-ending style of the branch/worktree content. `null` for a deleted file. */
  new_eol: LineEnding | null;
}

export interface BlameLine {
  sha: string;
  author: string;
  summary: string;
  line: number;
  content: string;
}
