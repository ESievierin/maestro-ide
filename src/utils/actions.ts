import { invoke } from "@tauri-apps/api/core";
import { useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";
import type { Session } from "../types/sessions";
import { defaultTranscriptFilename, transcriptToMarkdown } from "./exportTranscript";

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
