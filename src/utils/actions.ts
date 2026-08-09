import { invoke } from "@tauri-apps/api/core";
import { useSessions } from "../state/sessions";
import { useWorktrees } from "../state/worktrees";

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
