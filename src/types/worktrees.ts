// Mirrors the Rust types in src-tauri/src/core/worktree/.

export interface BranchStatus {
  dirty: boolean;
  ahead: number;
  behind: number;
}

export interface WorktreeInfo {
  branch: string | null;
  path: string;
  is_primary: boolean;
  task_id: string | null;
  base_branch: string | null;
  status: BranchStatus | null;
}

export interface RepoInfo {
  path: string;
  default_branch: string;
  /** Local branches (valid targets for attach-existing). */
  branches: string[];
  /** Remote-tracking branches (valid as base for new branches). */
  remote_branches: string[];
}

export interface CreateWorktreeRequest {
  existing_branch?: string;
  kind?: string;
  task_id?: string;
  slug?: string;
  base?: string;
}

export type RemoveOutcome = { outcome: "removed" } | { outcome: "dirty_confirmation_required" };

export interface MergeOutcome {
  merged: boolean;
  /** Paths with conflict markers, when `merged` is false because of a conflict. */
  conflicts: string[];
  /** Git's own stdout/stderr, shown verbatim on an unexpected (non-conflict) failure. */
  message: string;
}
