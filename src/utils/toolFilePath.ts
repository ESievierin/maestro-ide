import type { TranscriptItem } from "../types/sessions";

const FILE_EDITING_TOOLS = new Set(["Edit", "Write", "MultiEdit", "NotebookEdit"]);

/** Best-effort extraction of the file a file-editing tool call touched, from
 * its JSON-stringified (and possibly truncated, see sidecar's `summarizeInput`)
 * input summary. `file_path` is always among the first fields these tools
 * take, so a regex on the raw text survives truncation even when the rest of
 * a large edit does not — a full `JSON.parse` of a truncated summary would not. */
export function editedFilePath(item: Extract<TranscriptItem, { kind: "tool_use" }>): string | null {
  if (!FILE_EDITING_TOOLS.has(item.name)) return null;
  const match = item.summary.match(/"file_path"\s*:\s*"((?:[^"\\]|\\.)*)"/);
  if (!match) return null;
  try {
    return JSON.parse(`"${match[1]}"`) as string;
  } catch {
    return match[1];
  }
}

/** `absolutePath` relative to `worktreeRoot`, or null when it is not inside
 * it — normalizes backslashes so a Windows tool-call path matches a
 * `/`-joined diff path. */
export function relativeToWorktree(absolutePath: string, worktreeRoot: string): string | null {
  const normalize = (p: string) => p.replace(/\\/g, "/").replace(/\/+$/, "");
  const root = normalize(worktreeRoot);
  const path = normalize(absolutePath);
  const prefix = `${root}/`;
  if (path.toLowerCase().startsWith(prefix.toLowerCase())) {
    return path.slice(prefix.length);
  }
  return null;
}
