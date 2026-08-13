// Persistence for the diff viewer's per-file "viewed" checkmarks. Keyed by
// branch and pinned to a merge-base: syncing with the base (which moves the
// merge-base) resets the review state, an app restart does not.

const keyOf = (branch: string) => `maestro.viewed.${branch}`;

export function loadViewed(branch: string, mergeBase: string): Set<string> {
  try {
    const raw = localStorage.getItem(keyOf(branch));
    if (!raw) return new Set();
    const parsed = JSON.parse(raw) as { mergeBase?: string; files?: unknown };
    if (parsed.mergeBase !== mergeBase || !Array.isArray(parsed.files)) return new Set();
    return new Set(parsed.files.filter((f): f is string => typeof f === "string"));
  } catch {
    return new Set();
  }
}

export function saveViewed(branch: string, mergeBase: string, files: Set<string>): void {
  try {
    if (files.size === 0) {
      localStorage.removeItem(keyOf(branch));
    } else {
      localStorage.setItem(keyOf(branch), JSON.stringify({ mergeBase, files: [...files] }));
    }
  } catch {
    // Quota/serialization failures just mean the checkmarks don't survive a
    // restart — never worth breaking the viewer over.
  }
}
