//! The tracker's own honesty gate: every issue file says what it is, in the
//! words `issues/README.md` closes the two deciding fields to.
//!
//! `ls` is the index and the frontmatter is the query, so the directory's whole
//! usefulness is that `rg -l '^status: open' issues/` means *unheld work* and
//! `rg -l '^kind: track' issues/` means *staged work*. A value outside the
//! closed list answers neither query and is invisible in both: one file carried
//! `kind: design-debt` — an *area* directory name typed into the kind field —
//! among 363 others, and nothing in the tree could see it, because until now
//! nothing read `kind:` programmatically at all. `src/redlist.rs` resolves an
//! issue path, which is a different question: whether a Rust table's citation
//! still points at a file.
//!
//! **The two lists live here and not in a scan over the README**, because
//! documentation carries no gates in this tree (`src/CLAUDE.md`): a test over
//! prose is exactly the artifact an owner ruling deleted. The README says what
//! the fields *mean*; this says what a file may hold, and this is what reds.
//!
//! Cheap on purpose — 363 files read and parsed is milliseconds, so it runs in
//! `cargo test --lib` on every machine that builds the tree rather than in a
//! job somebody has to remember.

use std::path::{Path, PathBuf};

/// What is owed. `issues/README.md`'s `status` column, closed.
const STATUSES: &[&str] = &["open", "assigned", "expected-red", "owner", "none"];

/// What the entry is. `issues/README.md`'s `kind` column, closed.
///
/// Not the `Areas` list beside it: an area is a directory, and a directory name
/// in this field is the mistake this gate exists to name.
const KINDS: &[&str] = &["defect", "finding", "track", "question", "rejected"];

/// The kinds that answer "what is owed" by themselves, and the statuses they
/// may therefore be paired with.
///
/// `kind` says what the entry is and `status` says what is owed; two of the
/// kinds say both, so they may not be contradicted. This is what makes
/// `rg -l '^status: open'` mean unheld work rather than "every file nobody was
/// assigned" — the `question` and `rejected` files all said `open` once, and
/// the query over-reported by eleven with nothing able to tell.
const ALLOWED_STATUS: &[(&str, &[&str])] = &[
    ("defect", &["open", "assigned", "expected-red"]),
    ("finding", &["open", "assigned", "expected-red"]),
    ("track", &["open", "assigned"]),
    ("question", &["owner"]),
    ("rejected", &["none"]),
];

/// The one file in `issues/` that is not an issue.
const README: &str = "issues/README.md";

/// Every refusal the tracker's frontmatter earns, one line each.
///
/// Takes the files rather than reading them, so the negative control can stage
/// a bad one without writing into the tree.
fn refusals(files: &[(String, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (path, text) in files {
        let Some(front) = frontmatter(text) else {
            bad.push(format!(
                "{path}: no `---` frontmatter block. Every issue carries one, and it is what \
                 every query over this directory reads"
            ));
            continue;
        };
        let status = field(front, "status");
        let kind = field(front, "kind");
        match (status, kind) {
            (None, _) => bad.push(format!("{path}: no `status:` field")),
            (_, None) => bad.push(format!("{path}: no `kind:` field")),
            (Some(status), Some(kind)) => {
                if !STATUSES.contains(&status) {
                    bad.push(format!(
                        "{path}: `status: {status}` is not one of {STATUSES:?}, so no query over \
                         this directory can see the file"
                    ));
                }
                if !KINDS.contains(&kind) {
                    bad.push(format!(
                        "{path}: `kind: {kind}` is not one of {KINDS:?}, so no query over this \
                         directory can see the file — an area is a directory, not a kind"
                    ));
                }
                if let Some((_, allowed)) = ALLOWED_STATUS.iter().find(|(k, _)| *k == kind) {
                    if STATUSES.contains(&status) && !allowed.contains(&status) {
                        bad.push(format!(
                            "{path}: `kind: {kind}` already says what is owed, and \
                             `status: {status}` says otherwise — it may only be {allowed:?}"
                        ));
                    }
                }
            }
        }
    }
    bad
}

/// The leading `---` block, or nothing if the file does not open with one.
fn frontmatter(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// One field's value, trimmed. A field is `name: value` at the start of a line.
fn field<'a>(front: &'a str, name: &str) -> Option<&'a str> {
    front
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

/// Every issue file in the tracker, path relative to the repository root.
///
/// `issues/README.md` is the one exclusion and it is by exact path: a `README`
/// in an area directory would be prose nobody asked for, and this would say so.
fn tracker(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(root, &root.join("issues"), &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{} is the issue tracker: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
            continue;
        }
        if path.extension().is_none_or(|e| e != "md") {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if relative == README {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        out.push((relative, text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The gate over the tracker as it stands.
    ///
    /// The count is asserted for the reason every scan here asserts one: a walk
    /// that quietly found nothing would wave the whole directory through and
    /// read as green.
    #[test]
    fn every_issue_file_says_what_it_is() {
        let files = tracker(&repo_root());
        assert!(files.len() > 100, "only {} issue file(s) found", files.len());
        let bad = refusals(&files);
        assert!(
            bad.is_empty(),
            "`issues/README.md` closes `status` and `kind`, and a value outside either is \
             invisible to every query over this directory:\n  {}",
            bad.join("\n  ")
        );
    }

    /// The teeth, and the first case is the one this was written for: an area
    /// directory's name typed into the `kind` field, which is how
    /// `kind: design-debt` lived in the tracker unseen.
    #[test]
    fn the_gate_refuses_what_the_readme_does_not_define() {
        let staged = |front: &str| {
            vec![(
                "issues/area/staged.md".to_string(),
                format!("---\n{front}\n---\n\n# a heading\n"),
            )]
        };
        let says = |front: &str, needle: &str| {
            let bad = refusals(&staged(front));
            assert!(
                bad.iter().any(|b| b.contains(needle)),
                "expected a refusal naming {needle:?}, got {bad:?}"
            );
        };

        says("status: open\nkind: design-debt\nopened: 2026-08-15", "is not one of");
        says("status: pending\nkind: defect\nopened: 2026-08-15", "is not one of");
        says("status: open\nopened: 2026-08-15", "no `kind:` field");
        says("kind: defect\nopened: 2026-08-15", "no `status:` field");
        says("status: open\nkind: question\nopened: 2026-08-15", "already says what is owed");
        says("status: open\nkind: rejected\nopened: 2026-08-15", "already says what is owed");

        assert!(
            refusals(&[("issues/area/bare.md".to_string(), "# no frontmatter\n".to_string())])
                .iter()
                .any(|b| b.contains("no `---` frontmatter block"))
        );

        // The positive control: the shapes above are refused for their field
        // and not for being staged.
        assert!(refusals(&staged("status: open\nkind: defect\nopened: 2026-08-15")).is_empty());
        assert!(refusals(&staged("status: none\nkind: rejected\nopened: 2026-08-15")).is_empty());
        assert!(refusals(&staged("status: owner\nkind: question\nopened: 2026-08-15")).is_empty());
    }
}
