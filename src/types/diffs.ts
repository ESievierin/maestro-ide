// Mirrors the Rust types in src-tauri/src/core/diff/ and core/worktree/provider.rs.

export interface ChangedFile {
  path: string;
  /** Single-letter git status: A, M, D, R, … */
  status: string;
  old_path: string | null;
}

export interface DiffSnapshot {
  branch: string;
  base: string;
  merge_base: string;
  files: ChangedFile[];
  unified: string;
  computed_at: string;
}

export interface FileDiff {
  path: string;
  old: string | null;
  new: string | null;
}

export interface BlameLine {
  sha: string;
  author: string;
  summary: string;
  line: number;
  content: string;
}
