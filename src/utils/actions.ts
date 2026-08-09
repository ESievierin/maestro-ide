import { invoke } from "@tauri-apps/api/core";
import { useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import type { Notes } from "../types/notes";
import type { Session } from "../types/sessions";
import { defaultTranscriptFilename, transcriptToMarkdown } from "./exportTranscript";

// TODO: this file has seen better days

/** A filesystem-safe default filename: no path separators or reserved
 * Windows characters. */
function defaultNotesFilename(branch: string): string {
  const branchSlug = branch.replace(/[\\/:*?"<>|]+/g, "-");
  return `TASK_NOTES-${branchSlug}.md`;
}

/** Save a worktree's TASK_NOTES.md to a file on disk — for pasting into a PR
 * description or a team wiki page, the same reasoning as `exportTranscript`
 * below. Silent no-op if the user cancels the save dialog. */
export async function exportNotes(notes: Notes): Promise<void> {
  const { save } = await import("@tauri-apps/plugin-dialog");
  const path = await save({
    title: "Export task notes",
    defaultPath: defaultNotesFilename(notes.branch),
    filters: [{ name: "Markdown", extensions: ["md"] }],
  });
  if (!path) return;

  const { useToasts } = await import("../state/toasts");
  try {
    await invoke("write_text_file", { path, content: notes.raw });
    useToasts.getState().push({
      severity: "info",
      code: "exported",
      message: `Notes exported to ${path}`,
    });
  } catch (e) {
    useToasts.getState().push({
      severity: "warning",
      code: "export-failed",
      message: `Could not export notes: ${String(e)}`,
    });
  }
}

/** Open the worktree (or one of its files) in an external tool. Failures surface
 * as error toasts via the core's own error.raised path — no handling needed here. */
export function openWorktree(branch: string, target: "explorer" | "editor", file?: string) {
  void invoke("open_worktree", { branch, target, file: file ?? null }).catch(() => {
    // run_core already published error.raised for the toast; swallow the reject.
  });
}

/** Copy a worktree path with a confirming toast. */
export async function copyPath(path: string) {
  await navigator.clipboard.writeText(path);
  const { useToasts } = await import("../state/toasts");
  useToasts.getState().push({
    severity: "info",
    code: "copied",
    message: "Worktree path copied to the clipboard.",
  });
}

/** Remove a worktree, confirming (with the native dialog — window.confirm can
 * hang the webview, see the close-hang fix) when it has uncommitted changes. */
export async function removeWorktree(branch: string): Promise<void> {
  const { remove } = useWorktrees.getState();
  const outcome = await remove(branch, false);
  if (outcome?.outcome === "dirty_confirmation_required") {
    const { confirm } = await import("@tauri-apps/plugin-dialog");
    const forceIt = await confirm(
      `Worktree "${branch}" has uncommitted changes.\nRemove anyway and discard them?`,
      { title: "MaestroIDE", kind: "warning" },
    );
    if (forceIt) await remove(branch, true);
  }
}

/** Sync every non-primary worktree with its base, one at a time — a
 * conflict on one branch must never stop the rest from being attempted —
 * then report one summary toast instead of a flood of per-branch ones. */
export async function syncAllWorktrees(): Promise<void> {
  const { worktrees, sync } = useWorktrees.getState();
  const targets = worktrees.filter((w) => !w.is_primary && w.branch);
  if (targets.length === 0) return;

  let upToDate = 0;
  const conflicted: string[] = [];
  const failed: string[] = [];
  for (const wt of targets) {
    const branch = wt.branch as string;
    const outcome = await sync(branch);
    if (!outcome) failed.push(branch);
    else if (outcome.merged) upToDate++;
    else if (outcome.conflicts.length > 0) conflicted.push(branch);
    else failed.push(branch);
  }

  const { useToasts } = await import("../state/toasts");
  const parts = [`${upToDate} up to date`];
  if (conflicted.length > 0) {
    parts.push(`${conflicted.length} conflicted (${conflicted.join(", ")})`);
  }
  if (failed.length > 0) {
    parts.push(`${failed.length} failed (${failed.join(", ")})`);
  }
  useToasts.getState().push({
    severity: conflicted.length > 0 || failed.length > 0 ? "warning" : "info",
    code: "sync-all",
    message: `Synced ${targets.length} worktree${targets.length === 1 ? "" : "s"}: ${parts.join(", ")}.`,
  });
}

/** Save a session's transcript to a markdown file on disk — for sharing
 * outside the app or archiving. Silent no-op if the user cancels the save
 * dialog; a real write failure surfaces as a toast, not just a swallowed
 * rejection, since there is no `error.raised` bus event backing this one
 * (rendering happens client-side, `write_text_file` is a plain fs write). */
export async function exportTranscript(session: Session): Promise<void> {
  const { save } = await import("@tauri-apps/plugin-dialog");
  const path = await save({
    title: "Export session transcript",
    defaultPath: defaultTranscriptFilename(session),
    filters: [{ name: "Markdown", extensions: ["md"] }],
  });
  if (!path) return;

  const items = useSessions.getState().transcripts[session.id] ?? [];
  const markdown = transcriptToMarkdown(session, items);
  const { useToasts } = await import("../state/toasts");
  try {
    await invoke("write_text_file", { path, content: markdown });
    useToasts.getState().push({
      severity: "info",
      code: "exported",
      message: `Transcript exported to ${path}`,
    });
  } catch (e) {
    useToasts.getState().push({
      severity: "warning",
      code: "export-failed",
      message: `Could not export transcript: ${String(e)}`,
    });
  }
}

/** Copy a session's transcript to the clipboard as markdown — a lighter
 * companion to `exportTranscript` for pasting into Slack/a PR description
 * without needing to attach a file. */
export async function copyTranscript(session: Session): Promise<void> {
  const items = useSessions.getState().transcripts[session.id] ?? [];
  const markdown = transcriptToMarkdown(session, items);
  const { useToasts } = await import("../state/toasts");
  try {
    await navigator.clipboard.writeText(markdown);
    useToasts.getState().push({
      severity: "info",
      code: "copied",
      message: "Transcript copied to the clipboard as markdown.",
    });
  } catch (e) {
    useToasts.getState().push({
      severity: "warning",
      code: "copy-failed",
      message: `Could not copy transcript: ${String(e)}`,
    });
  }
}

/** Copy one file's unified diff to the clipboard as plain text — a lighter
 * companion to `copyTranscript` for pasting into Slack/a PR description
 * without switching to a terminal. */
export async function copyDiff(path: string, diffText: string): Promise<void> {
  const { useToasts } = await import("../state/toasts");
  if (!diffText.trim()) {
    useToasts.getState().push({
      severity: "warning",
      code: "copy-failed",
      message: `No diff to copy for ${path}.`,
    });
    return;
  }
  try {
    await navigator.clipboard.writeText(diffText);
    useToasts.getState().push({
      severity: "info",
      code: "copied",
      message: `Diff for ${path} copied to the clipboard.`,
    });
  } catch (e) {
    useToasts.getState().push({
      severity: "warning",
      code: "copy-failed",
      message: `Could not copy diff: ${String(e)}`,
    });
  }
}

/** Delete every finished session of `branch` in one go — confirms first when
 * there's more than a couple, since a single stray row isn't worth
 * interrupting anyone for but a pile of them is exactly what this is for. */
export async function clearFinishedSessions(branch: string, finishedCount: number): Promise<void> {
  if (finishedCount > 2) {
    const { confirm } = await import("@tauri-apps/plugin-dialog");
    const ok = await confirm(
      `Delete ${finishedCount} finished session${finishedCount === 1 ? "" : "s"} on this branch? This cannot be undone.`,
      { title: "MaestroIDE", kind: "warning" },
    );
    if (!ok) return;
  }
  await useSessions.getState().removeAllFinished(branch);
}
