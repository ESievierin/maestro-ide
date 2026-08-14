// Mirrors PromptFile in src-tauri/src/core/prompts/mod.rs.

export interface PromptFile {
  name: string;
  description: string | null;
  variables: string[];
  /** Full file contents, frontmatter included. */
  content: string;
  /** A built-in default exists for this name (so it can be reset). */
  has_default: boolean;
  /** The file differs from that default. */
  modified: boolean;
  /** The user edited this template AND the built-in default changed since —
   * resetting would pick up the newer default (discarding the edit). */
  update_available: boolean;
}
