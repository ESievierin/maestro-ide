// Mirrors `core/notes` in src-tauri.

export interface NoteSection {
  title: string;
  body: string;
}

/**
 * `TASK_NOTES.md` of a branch. Missing notes are a state, not an error: `exists` false with
 * `unavailable` set means there is no worktree (or the file could not be read), while
 * `exists` false without it means the file simply has not been written yet.
 */
export interface Notes {
  branch: string;
  path: string | null;
  exists: boolean;
  unavailable: string | null;
  sections: NoteSection[];
  raw: string;
  updated_at: string | null;
}
