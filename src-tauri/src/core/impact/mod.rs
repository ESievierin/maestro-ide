//! Blast-radius analysis: which files reference what just changed on a branch.
//!
//! An agent edits five files; the question a reviewer actually has is "what else
//! does this touch?". This module answers it without a language server: changed
//! files get a *signature* (their name stem plus their exported symbols, extracted
//! with per-language line scans), and every other tracked source file is scanned
//! for references to those signatures — import lines count as strong links,
//! plain symbol mentions as weaker ones. A second ring (files importing the
//! first ring) is computed from import links only, so the radius stays signal,
//! not noise.
//!
//! Everything is bounded: file count, file size, symbols per file, result count.
//! A repo an order of magnitude bigger than expected degrades to a truncated
//! report, never a hung UI.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use crate::core::diff::{DiffManager, DiffScope};
use crate::core::worktree::{GitProvider, WorktreeManager};
use crate::error::{MaestroError, Result};

/// Extensions we treat as source code (scanned as candidates and analyzed as
/// changed files). Everything else — configs, lockfiles, images — is skipped.
const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "cs", "py", "go", "java", "kt", "swift", "rb",
    "php", "vue", "svelte",
];

/// File stems too generic to identify a module by name; the parent directory
/// name identifies those files instead (`core/session/mod.rs` → `session`).
const GENERIC_STEMS: &[&str] = &["mod", "index", "main", "lib", "types", "utils", "helpers"];

/// Caps. Hitting any of them sets `truncated` on the report instead of erroring.
const MAX_CHANGED_ANALYZED: usize = 100;
const MAX_CANDIDATES: usize = 20_000;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 30;
const MAX_IMPACTED: usize = 500;
const MAX_LINKS_PER_FILE: usize = 8;

/// The blast radius of a branch's current diff.
#[derive(Clone, Debug, Serialize)]
pub struct ImpactReport {
    pub branch: String,
    /// Changed source files whose signatures were searched for.
    pub analyzed: Vec<String>,
    /// Changed files skipped (non-source extension, or over the analysis cap).
    pub skipped: Vec<String>,
    /// Files outside the diff that reference the changed ones, strongest first.
    pub impacted: Vec<ImpactedFile>,
    /// How many candidate files were actually scanned.
    pub scanned: usize,
    /// True when any cap was hit — the radius may be wider than reported.
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImpactedFile {
    pub path: String,
    /// 1 = references a changed file directly; 2 = imports a ring-1 file.
    pub distance: u8,
    /// Strongest link: `"import"` beats `"reference"`.
    pub kind: String,
    pub links: Vec<ImpactLink>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ImpactLink {
    /// The changed (or ring-1) file this one points at.
    pub target: String,
    /// `"import"` (an import/use line names it) or `"reference"` (a symbol
    /// exported from it is mentioned).
    pub kind: String,
    /// The stem or symbol that matched.
    pub matched: String,
}

/// What we search for on behalf of one changed file.
#[derive(Clone, Debug)]
struct FileSignature {
    path: String,
    /// Name stem (or parent-dir name for generic stems like `mod.rs`).
    stem: String,
    /// Exported symbols, per-language line scan; empty for deleted files.
    symbols: Vec<String>,
}

pub struct ImpactManager {
    git: Arc<dyn GitProvider>,
    worktrees: Arc<WorktreeManager>,
    diffs: Arc<DiffManager>,
}

impl ImpactManager {
    pub fn new(
        git: Arc<dyn GitProvider>,
        worktrees: Arc<WorktreeManager>,
        diffs: Arc<DiffManager>,
    ) -> Self {
        Self {
            git,
            worktrees,
            diffs,
        }
    }

    /// Analyze the blast radius of `branch`'s working-tree diff.
    pub fn analyze(&self, branch: &str) -> Result<ImpactReport> {
        let worktree = self
            .worktrees
            .list()?
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .ok_or_else(|| MaestroError::InvalidData {
                message: format!("no worktree for branch: {branch}"),
            })?;
        let root = worktree.path;

        let snapshot = self.diffs.get(branch, DiffScope::Worktree)?;
        let mut truncated = false;

        // Signatures of the changed source files.
        let mut analyzed = Vec::new();
        let mut skipped = Vec::new();
        let mut signatures = Vec::new();
        for file in &snapshot.files {
            if !is_source_file(&file.path) {
                skipped.push(file.path.clone());
                continue;
            }
            if signatures.len() >= MAX_CHANGED_ANALYZED {
                truncated = true;
                skipped.push(file.path.clone());
                continue;
            }
            // A deleted file has no content on disk; its stem still matters —
            // anything still importing it is exactly what broke.
            let content = read_bounded(&root.join(&file.path));
            signatures.push(derive_signature(&file.path, content.as_deref()));
            analyzed.push(file.path.clone());
        }
        if signatures.is_empty() {
            return Ok(ImpactReport {
                branch: branch.to_string(),
                analyzed,
                skipped,
                impacted: Vec::new(),
                scanned: 0,
                truncated,
            });
        }

        // Candidate files: everything tracked, minus the diff itself.
        let changed_set: HashMap<&str, ()> = snapshot
            .files
            .iter()
            .map(|f| (f.path.as_str(), ()))
            .collect();
        let mut candidates: Vec<String> = self
            .git
            .list_files(&root)?
            .into_iter()
            .filter(|p| is_source_file(p) && !changed_set.contains_key(p.as_str()))
            .collect();
        if candidates.len() > MAX_CANDIDATES {
            truncated = true;
            candidates.truncate(MAX_CANDIDATES);
        }

        // Ring 1: scan every candidate against the changed files' signatures.
        let mut impacted: Vec<ImpactedFile> = Vec::new();
        let mut scanned = 0usize;
        for path in &candidates {
            let Some(content) = read_bounded(&root.join(path)) else {
                continue;
            };
            scanned += 1;
            let links = scan_for_links(&content, &signatures);
            if !links.is_empty() {
                impacted.push(build_impacted(path, 1, links));
                if impacted.len() >= MAX_IMPACTED {
                    truncated = true;
                    break;
                }
            }
        }

        // Ring 2: who imports the ring-1 files (import links only — symbol
        // mentions two hops out are almost always noise).
        let ring1_import_paths: Vec<String> = impacted
            .iter()
            .filter(|f| f.kind == "import")
            .map(|f| f.path.clone())
            .collect();
        if !ring1_import_paths.is_empty() && impacted.len() < MAX_IMPACTED {
            let ring1_signatures: Vec<FileSignature> = ring1_import_paths
                .iter()
                .map(|p| derive_signature(p, None))
                .collect();
            let already: std::collections::HashSet<String> =
                impacted.iter().map(|f| f.path.clone()).collect();
            for path in &candidates {
                if already.contains(path.as_str()) {
                    continue;
                }
                let Some(content) = read_bounded(&root.join(path)) else {
                    continue;
                };
                let links: Vec<ImpactLink> = scan_for_links(&content, &ring1_signatures)
                    .into_iter()
                    .filter(|l| l.kind == "import")
                    .collect();
                if !links.is_empty() {
                    impacted.push(build_impacted(path, 2, links));
                    if impacted.len() >= MAX_IMPACTED {
                        truncated = true;
                        break;
                    }
                }
            }
        }

        // Strongest first: ring 1 imports, ring 1 references, then ring 2.
        impacted.sort_by(|a, b| {
            let rank = |f: &ImpactedFile| (f.distance, if f.kind == "import" { 0 } else { 1 });
            rank(a).cmp(&rank(b)).then_with(|| a.path.cmp(&b.path))
        });

        tracing::info!(
            branch,
            analyzed = analyzed.len(),
            impacted = impacted.len(),
            scanned,
            truncated,
            "blast radius computed"
        );
        Ok(ImpactReport {
            branch: branch.to_string(),
            analyzed,
            skipped,
            impacted,
            scanned,
            truncated,
        })
    }
}

fn build_impacted(path: &str, distance: u8, mut links: Vec<ImpactLink>) -> ImpactedFile {
    // Imports first so the strongest evidence survives the cap.
    links.sort_by(|a, b| {
        let rank = |l: &ImpactLink| if l.kind == "import" { 0 } else { 1 };
        rank(a).cmp(&rank(b)).then_with(|| a.target.cmp(&b.target))
    });
    links.dedup();
    let kind = if links.iter().any(|l| l.kind == "import") {
        "import"
    } else {
        "reference"
    };
    links.truncate(MAX_LINKS_PER_FILE);
    ImpactedFile {
        path: path.to_string(),
        distance,
        kind: kind.to_string(),
        links,
    }
}

fn is_source_file(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| SOURCE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Read a file's text, or `None` when it's missing, unreadable, too large, or
/// binary — every one of those just means "skip it", never an error.
fn read_bounded(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    if content.contains('\0') {
        return None;
    }
    Some(content)
}

/// Name stem plus exported symbols for one changed file.
fn derive_signature(path: &str, content: Option<&str>) -> FileSignature {
    let p = Path::new(path);
    let raw_stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let stem = if GENERIC_STEMS.contains(&raw_stem.to_ascii_lowercase().as_str()) {
        p.parent()
            .and_then(|d| d.file_name())
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .unwrap_or(raw_stem)
    } else {
        raw_stem
    };
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let symbols = content
        .map(|c| extract_symbols(c, &ext))
        .unwrap_or_default();
    FileSignature {
        path: path.to_string(),
        stem,
        symbols,
    }
}

/// Exported/public symbols via a cheap per-language line scan. Deliberately
/// shallow: names that appear in other files are the point, not a full parse.
fn extract_symbols(content: &str, ext: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    for line in content.lines() {
        if symbols.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
        let trimmed = line.trim_start();
        let name = match ext {
            "rs" => rust_symbol(trimmed),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => ts_symbol(trimmed),
            "cs" => csharp_symbol(trimmed),
            "py" => python_symbol(line),
            _ => None,
        };
        if let Some(name) = name {
            // Single letters and two-char names match everything; skip them.
            if name.len() >= 3 && !symbols.contains(&name) {
                symbols.push(name);
            }
        }
    }
    symbols
}

fn rust_symbol(line: &str) -> Option<String> {
    let rest = line.strip_prefix("pub")?;
    // `pub(crate)`, `pub(super)`, …
    let rest = if let Some(after) = rest.strip_prefix('(') {
        after.split_once(')')?.1
    } else {
        rest
    };
    let rest = rest.trim_start();
    for kw in [
        "fn ",
        "struct ",
        "enum ",
        "trait ",
        "const ",
        "type ",
        "static ",
        "async fn ",
    ] {
        if let Some(after) = rest.strip_prefix(kw) {
            return first_identifier(after);
        }
    }
    None
}

fn ts_symbol(line: &str) -> Option<String> {
    let rest = line.strip_prefix("export ")?;
    let rest = rest.strip_prefix("default ").unwrap_or(rest);
    let rest = rest.strip_prefix("abstract ").unwrap_or(rest);
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    for kw in [
        "function ",
        "const ",
        "let ",
        "class ",
        "interface ",
        "type ",
        "enum ",
    ] {
        if let Some(after) = rest.strip_prefix(kw) {
            return first_identifier(after);
        }
    }
    None
}

fn csharp_symbol(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("public ")
        .or_else(|| line.strip_prefix("internal "))?;
    let mut rest = rest;
    // Skip modifier soup in any order.
    loop {
        let mut advanced = false;
        for m in [
            "static ",
            "sealed ",
            "abstract ",
            "partial ",
            "readonly ",
            "ref ",
        ] {
            if let Some(after) = rest.strip_prefix(m) {
                rest = after;
                advanced = true;
            }
        }
        if !advanced {
            break;
        }
    }
    // Types are the contracts other files name; methods are too noisy.
    for kw in ["class ", "interface ", "record ", "struct ", "enum "] {
        if let Some(after) = rest.strip_prefix(kw) {
            return first_identifier(after);
        }
    }
    None
}

fn python_symbol(line: &str) -> Option<String> {
    // Column 0 only: top-level definitions.
    for kw in ["def ", "class "] {
        if let Some(after) = line.strip_prefix(kw) {
            return first_identifier(after);
        }
    }
    None
}

fn first_identifier(text: &str) -> Option<String> {
    let name: String = text
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Does `content` contain `needle` as a whole word (non-identifier chars on
/// both sides)?
fn contains_word(content: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = content.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut from = 0;
    while let Some(pos) = content[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_ident(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = end;
    }
    false
}

/// Is this line an import/use/require line for its (unknown) language?
fn is_import_line(trimmed: &str) -> bool {
    trimmed.starts_with("use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("from ")
        || trimmed.starts_with("export ") && trimmed.contains(" from ")
        || trimmed.contains("require(")
}

/// All links from one candidate file to the given signatures.
fn scan_for_links(content: &str, signatures: &[FileSignature]) -> Vec<ImpactLink> {
    let mut links = Vec::new();

    // Import lines referencing a changed file's stem = strong link.
    for line in content.lines() {
        let trimmed = line.trim_start();
        if !is_import_line(trimmed) {
            continue;
        }
        for sig in signatures {
            if contains_word(trimmed, &sig.stem)
                && !links
                    .iter()
                    .any(|l: &ImpactLink| l.target == sig.path && l.kind == "import")
            {
                links.push(ImpactLink {
                    target: sig.path.clone(),
                    kind: "import".to_string(),
                    matched: sig.stem.clone(),
                });
            }
        }
    }

    // Exported-symbol mentions anywhere = weaker link.
    for sig in signatures {
        if links.iter().any(|l| l.target == sig.path) {
            continue; // already linked via import; symbols add nothing.
        }
        for symbol in &sig.symbols {
            if contains_word(content, symbol) {
                links.push(ImpactLink {
                    target: sig.path.clone(),
                    kind: "reference".to_string(),
                    matched: symbol.clone(),
                });
                break;
            }
        }
    }

    links
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(path: &str, content: &str) -> FileSignature {
        derive_signature(path, Some(content))
    }

    #[test]
    fn stems_fall_back_to_the_directory_for_generic_names() {
        assert_eq!(
            derive_signature("core/session/mod.rs", None).stem,
            "session"
        );
        assert_eq!(
            derive_signature("src/components/index.ts", None).stem,
            "components"
        );
        assert_eq!(
            derive_signature("src/state/sessions.ts", None).stem,
            "sessions"
        );
        assert_eq!(
            derive_signature("Data/UserRepository.cs", None).stem,
            "UserRepository"
        );
    }

    #[test]
    fn rust_symbols_are_extracted_from_pub_items() {
        let s = sig(
            "core/gate/mod.rs",
            "pub struct GateManager {}\npub(crate) fn match_rule() {}\nfn private() {}\npub const MAX_RULES: usize = 4;\n",
        );
        assert_eq!(s.symbols, vec!["GateManager", "match_rule", "MAX_RULES"]);
    }

    #[test]
    fn ts_symbols_are_extracted_from_exports() {
        let s = sig(
            "src/utils/agentAsk.ts",
            "export function askMainAgent() {}\nexport const START_MODEL = \"x\";\nconst hidden = 1;\nexport interface AskResult {}\n",
        );
        assert_eq!(s.symbols, vec!["askMainAgent", "START_MODEL", "AskResult"]);
    }

    #[test]
    fn csharp_symbols_are_types_only() {
        let s = sig(
            "Logic/InMailPolicy.cs",
            "public sealed class InMailCreditsSkipPolicy {}\n    public void HelperMethod() {}\ninternal interface ISkipPolicy {}\n",
        );
        assert_eq!(s.symbols, vec!["InMailCreditsSkipPolicy", "ISkipPolicy"]);
    }

    #[test]
    fn import_lines_link_stronger_than_symbol_mentions() {
        let changed = vec![sig(
            "src/state/sessions.ts",
            "export const useSessions = 1;\nexport function activeSessionCount() {}\n",
        )];
        // An import line naming the module → import link.
        let importer = "import { useSessions } from \"../state/sessions\";\n";
        let links = scan_for_links(importer, &changed);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, "import");
        assert_eq!(links[0].matched, "sessions");

        // A mere mention of an exported symbol → reference link.
        let mentioner = "// uses activeSessionCount indirectly\nconst n = activeSessionCount();\n";
        let links = scan_for_links(mentioner, &changed);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, "reference");
        assert_eq!(links[0].matched, "activeSessionCount");
    }

    #[test]
    fn whole_word_matching_rejects_substrings() {
        assert!(contains_word("let session_id = 1;", "session_id"));
        assert!(!contains_word("let session_identifier = 1;", "session_id"));
        assert!(!contains_word("mysession_id", "session_id"));
        assert!(contains_word(
            "use crate::core::session::manager;",
            "manager"
        ));
    }

    #[test]
    fn rust_use_lines_link_by_module_stem() {
        let changed = vec![sig(
            "src-tauri/src/core/session/manager.rs",
            "pub struct SessionManager {}\n",
        )];
        let links = scan_for_links(
            "use crate::core::session::manager::SessionManager;\n",
            &changed,
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, "import");
    }

    #[test]
    fn unrelated_files_produce_no_links() {
        let changed = vec![sig("src/state/pr.ts", "export const usePr = 1;\n")];
        let links = scan_for_links(
            "import { x } from \"./other\";\nconst y = somethingElse();\n",
            &changed,
        );
        assert!(links.is_empty());
    }

    #[test]
    fn short_symbols_are_skipped_entirely() {
        let s = sig(
            "src/x.ts",
            "export const on = 1;\nexport const id = 2;\nexport const useX = 3;\n",
        );
        assert_eq!(
            s.symbols,
            vec!["useX"],
            "2-char names match everything, so they are dropped"
        );
    }

    #[test]
    fn deleted_files_still_match_by_stem() {
        let changed = vec![derive_signature("src/utils/oldHelper.ts", None)];
        let links = scan_for_links("import { a } from \"../utils/oldHelper\";\n", &changed);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, "import");
        assert_eq!(links[0].matched, "oldHelper");
    }

    #[test]
    fn build_impacted_prefers_imports_and_caps_links() {
        let mut links = Vec::new();
        for i in 0..12 {
            links.push(ImpactLink {
                target: format!("file{i}.rs"),
                kind: "reference".into(),
                matched: format!("Sym{i}"),
            });
        }
        links.push(ImpactLink {
            target: "imported.rs".into(),
            kind: "import".into(),
            matched: "imported".into(),
        });
        let impacted = build_impacted("dependent.rs", 1, links);
        assert_eq!(impacted.kind, "import");
        assert_eq!(impacted.links.len(), MAX_LINKS_PER_FILE);
        assert_eq!(
            impacted.links[0].kind, "import",
            "imports survive the cap first"
        );
    }
}
