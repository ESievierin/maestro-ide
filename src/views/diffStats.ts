export interface FileDiffStats {
  additions: number;
  deletions: number;
}

/**
 * Per-file +/- line counts, parsed from the full multi-file unified diff the
 * core already sends (`DiffSnapshot.unified`). Purely a display concern, so
 * it's computed client-side rather than adding fields to the wire protocol.
 */
export function parseDiffStats(unified: string): Record<string, FileDiffStats> {
  const stats: Record<string, FileDiffStats> = {};
  if (!unified) return stats;

  let pendingOldPath: string | null = null;
  let currentPath: string | null = null;

  for (const line of unified.split("\n")) {
    if (line.startsWith("--- ")) {
      const p = line.slice(4).trim();
      pendingOldPath = p === "/dev/null" ? null : p.replace(/^a\//, "");
      continue;
    }
    if (line.startsWith("+++ ")) {
      const p = line.slice(4).trim();
      const newPath = p === "/dev/null" ? null : p.replace(/^b\//, "");
      // A deletion has no `b/` side — fall back to the `a/` path so the
      // removed file still gets an entry.
      currentPath = newPath ?? pendingOldPath;
      if (currentPath && !(currentPath in stats)) {
        stats[currentPath] = { additions: 0, deletions: 0 };
      }
      continue;
    }
    if (!currentPath) continue;
    if (line.startsWith("+")) stats[currentPath].additions++;
    else if (line.startsWith("-")) stats[currentPath].deletions++;
  }
  return stats;
}

export interface HunkRange {
  /** 1-based start line in the "new" file. */
  start: number;
  /** 1-based end line (inclusive) in the "new" file. */
  end: number;
}

const HUNK_HEADER = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/;

/**
 * Hunk line-ranges (in the new file's line numbers) for one file's diff,
 * parsed from the same full multi-file unified diff `parseDiffStats` reads.
 * Powers the change-overview strip next to the editor.
 */
export function parseFileHunks(unified: string, path: string): HunkRange[] {
  const hunks: HunkRange[] = [];
  if (!unified || !path) return hunks;

  let pendingOldPath: string | null = null;
  let currentPath: string | null = null;

  for (const line of unified.split("\n")) {
    if (line.startsWith("--- ")) {
      const p = line.slice(4).trim();
      pendingOldPath = p === "/dev/null" ? null : p.replace(/^a\//, "");
      continue;
    }
    if (line.startsWith("+++ ")) {
      const p = line.slice(4).trim();
      const newPath = p === "/dev/null" ? null : p.replace(/^b\//, "");
      currentPath = newPath ?? pendingOldPath;
      continue;
    }
    if (currentPath !== path) continue;
    const m = HUNK_HEADER.exec(line);
    if (m) {
      const start = parseInt(m[1], 10);
      const count = m[2] !== undefined ? parseInt(m[2], 10) : 1;
      hunks.push({ start, end: Math.max(start, start + count - 1) });
    }
  }
  return hunks;
}
