import { invoke } from "@tauri-apps/api/core";

/** Open the worktree in an external tool. Failures surface as error toasts via
 * the core's own error.raised path — no handling needed here. */
export function openWorktree(branch: string, target: "explorer" | "editor") {
  void invoke("open_worktree", { branch, target }).catch(() => {
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
