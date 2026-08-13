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

// Mirrors the Rust types in src-tauri/src/core/impact/mod.rs.

export interface ImpactLink {
  /** The changed (or ring-1) file being referenced. */
  target: string;
  kind: "import" | "reference";
  /** The stem or symbol that matched. */
  matched: string;
}

export interface ImpactedFile {
  path: string;
  /** 1 = references a changed file directly; 2 = imports a ring-1 file. */
  distance: number;
  kind: "import" | "reference";
  links: ImpactLink[];
}

export interface ImpactReport {
  branch: string;
  analyzed: string[];
  skipped: string[];
  impacted: ImpactedFile[];
  scanned: number;
  /** True when a cap was hit — the radius may be wider than reported. */
  truncated: boolean;
}
