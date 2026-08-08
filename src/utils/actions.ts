import { invoke } from "@tauri-apps/api/core";

/** Open the worktree (or one of its files) in an external tool. Failures surface
 * as error toasts via the core's own error.raised path — no handling needed here. */
export function openWorktree(branch: string, target: "explorer" | "editor", file?: string) {
  void invoke("open_worktree", { branch, target, file: file ?? null }).catch(() => {
    // run_core already published error.raised for the toast; swallow the reject.
  });
}

/** Copy a worktree path with a confirming toast. */
export async function copyPath(path: string) {
  await copyText(path, "Worktree path copied to the clipboard.");
}

/** Copy arbitrary text with a confirming toast. */
export async function copyText(text: string, message: string) {
  await navigator.clipboard.writeText(text);
  const { useToasts } = await import("../state/toasts");
  useToasts.getState().push({
    severity: "info",
    code: "copied",
    message,
  });
}
