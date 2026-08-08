//! Byte budgets for the `CLAUDE.md` files, and the gate that makes them bind.
//!
//! Every one of these is loaded into an agent's context — the root into every
//! session and every subagent, a subdirectory file whenever an agent reads a
//! file in that subtree. The cost is paid per dispatch and never shows up in a
//! build time, so nothing pushes back on growth except this.
//!
//! **Bytes, not lines.** The bar this replaces was "keep it under ~200 lines"
//! and the file passed it while carrying a 3,220-character line; a line count
//! cannot see an essay written as one paragraph.
//!
//! **No default.** A `CLAUDE.md` with no budget here fails the gate rather than
//! being waved through, so adding one is a decision somebody made on purpose.

use std::path::{Path, PathBuf};

/// Directories the walk does not enter: build output, the compiler submodule,
/// and anything git is not tracking for us.
const SKIP: &[&str] = &["target", "rust", ".git", "node_modules"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `CLAUDE.md` in the tree, as a path relative to the repository root.
pub fn claude_files() -> Vec<String> {
    let root = repo_root();
    let mut found = Vec::new();
    walk(&root, &root, &mut found);
    found.sort();
    found
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if SKIP.contains(&name.as_ref()) || name.starts_with('.') {
                continue;
            }
            walk(root, &path, out);
        } else if name == "CLAUDE.md" {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each `CLAUDE.md` may weigh, keyed by its path from the repository root.
    ///
    /// Each number is the file's size at the 2026-08-08 split with room for a few
    /// additions. Raising one is a decision with a reason: the root's is the tight
    /// one, because it is the only file every dispatch pays for whether or not the
    /// agent ever goes near the subtree it describes.
    const BUDGETS: &[(&str, usize)] = &[
        ("CLAUDE.md", 40_000),
        ("kernel/CLAUDE.md", 24_000),
        ("userland/CLAUDE.md", 12_000),
        ("tests/CLAUDE.md", 10_000),
        ("src/CLAUDE.md", 10_000),
    ];

    #[test]
    fn every_claude_md_is_within_its_budget() {
        let root = repo_root();
        let mut over = Vec::new();
        for (rel, budget) in BUDGETS {
            let path = root.join(rel);
            let size = std::fs::metadata(&path)
                .unwrap_or_else(|e| panic!("{rel} is budgeted but not readable: {e}"))
                .len() as usize;
            if size > *budget {
                over.push(format!(
                    "{rel}: {size} bytes against a budget of {budget} — \
                     move what only one subtree needs into that subtree's own \
                     CLAUDE.md, resolved narrative into git log, and detail into specs/"
                ));
            }
        }
        assert!(
            over.is_empty(),
            "documentation over budget:\n  {}",
            over.join("\n  ")
        );
    }

    #[test]
    fn every_claude_md_has_a_budget() {
        let budgeted: Vec<&str> = BUDGETS.iter().map(|(p, _)| *p).collect();
        let undeclared: Vec<String> = claude_files()
            .into_iter()
            .filter(|f| !budgeted.contains(&f.as_str()))
            .collect();
        assert!(
            undeclared.is_empty(),
            "a CLAUDE.md with no budget in src/docs.rs is one nothing pushes back on:\n  {}",
            undeclared.join("\n  ")
        );
    }
}
