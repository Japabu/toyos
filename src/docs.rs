//! What the two documentation shapes in this tree promise, and the gates that
//! make them bind: byte budgets for the `CLAUDE.md` files, and frontmatter and
//! reference resolution for `specs/issues/`.
//!
//! Every `CLAUDE.md` is loaded into an agent's context — the root into every
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
//!
//! `specs/issues/` is the same argument one level down. Its frontmatter is the
//! only query anybody has over two hundred files, and a field nothing checks is
//! a field that is right until the day it is not: every field parsed on the day
//! the directory was split, and eleven files still disagreed with what its own
//! README said `status: open` meant.

use std::path::{Path, PathBuf};

/// Directories the walk does not enter, each named rather than matched on a
/// leading dot. A blanket dot-skip hid `.github/`, which git tracks and which
/// an agent editing a workflow reads like any other subtree.
///
/// `target` and `node_modules` are build output. `rust` is the compiler
/// submodule, whose documentation is upstream's rather than ours. `.git` is
/// git's own store. `.claude` is per-developer and git does not track it, so a
/// budget on anything inside it would be red on one machine and green on the
/// next.
const SKIP: &[&str] = &["target", "node_modules", "rust", ".git", ".claude"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `CLAUDE.md` in the tree, as a path relative to the repository root.
pub fn claude_files() -> Vec<String> {
    let root = repo_root();
    let mut found = Vec::new();
    walk(&root, &root, &mut |path| {
        if path.file_name().is_some_and(|n| n == "CLAUDE.md") {
            found.push(rel(&root, path));
        }
    });
    found.sort();
    found
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn walk(root: &Path, dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if path.is_dir() {
            if SKIP.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, visit);
        } else {
            visit(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// One issue file's frontmatter, or why it has none this can read.
    struct Frontmatter {
        fields: BTreeMap<String, String>,
    }

    impl Frontmatter {
        /// Parses the `---`-delimited block a `specs/issues/` file opens with.
        ///
        /// Deliberately not a YAML parser: the format is `key: value`, one per
        /// line, and anything else is a file to name rather than a shape to
        /// accommodate.
        fn parse(text: &str) -> Result<Self, String> {
            let mut lines = text.lines();
            if lines.next() != Some("---") {
                return Err("does not open with a `---` frontmatter block".into());
            }
            let mut fields = BTreeMap::new();
            for line in lines {
                if line == "---" {
                    return Ok(Frontmatter { fields });
                }
                let Some((key, value)) = line.split_once(": ") else {
                    return Err(format!("frontmatter line is not `key: value`: {line:?}"));
                };
                if fields.insert(key.to_string(), value.to_string()).is_some() {
                    return Err(format!("frontmatter names `{key}` twice"));
                }
            }
            Err("frontmatter block is never closed by a `---`".into())
        }

        fn get(&self, key: &str) -> Option<&str> {
            self.fields.get(key).map(String::as_str)
        }
    }

    /// `true` for a `YYYY-MM-DD` that names a day that exists.
    fn is_a_date(s: &str) -> bool {
        let parts: Vec<&str> = s.split('-').collect();
        let [y, m, d] = parts[..] else { return false };
        if (y.len(), m.len(), d.len()) != (4, 2, 2) {
            return false;
        }
        let (Ok(y), Ok(m), Ok(d)) = (y.parse::<u32>(), m.parse::<u32>(), d.parse::<u32>()) else {
            return false;
        };
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let last = match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return false,
        };
        (1..=last).contains(&d)
    }

    /// Every `specs/issues/<area>/<slug>.md` path this text names.
    ///
    /// Hand-rolled rather than a regex because the build system has no regex
    /// dependency and the shape is fixed: the prefix, an area, a slug, `.md`.
    fn issue_paths_in(text: &str) -> BTreeSet<String> {
        const PREFIX: &str = "specs/issues/";
        let mut found = BTreeSet::new();
        let bytes = text.as_bytes();
        let mut at = 0;
        while let Some(hit) = text[at..].find(PREFIX) {
            let start = at + hit;
            let mut end = start + PREFIX.len();
            while end < bytes.len() && matches!(bytes[end], b'a'..=b'z' | b'0'..=b'9' | b'-' | b'/' | b'.')
            {
                end += 1;
            }
            at = end.max(start + PREFIX.len());
            let candidate = &text[start..end];
            if candidate.ends_with(".md") && candidate[PREFIX.len()..].contains('/') {
                found.insert(candidate.to_string());
            }
        }
        found
    }


    /// The area directories under `specs/issues/`, and the whole list. A file
    /// outside one of these is not reachable by the queries the README documents.
    const AREAS: &[&str] = &[
        "isolation",
        "panic-path",
        "kernel",
        "audio",
        "diagnostics",
        "build",
        "design-debt",
        "hardware",
        "filesystem",
        "boot-media",
    ];

    /// Frontmatter `status`, paired with the `kind`s that may carry it.
    ///
    /// `kind` says what an entry is and `status` says what is owed, so two of the
    /// kinds answer the second question by themselves and may not then contradict
    /// it. Without the pairing `rg -l '^status: open'` counts the questions and the
    /// rejections as unheld work.
    const STATUS_KINDS: &[(&str, &[&str])] = &[
        ("open", &["defect", "finding"]),
        ("assigned", &["defect", "finding"]),
        ("expected-red", &["defect", "finding"]),
        ("owner", &["question"]),
        ("none", &["rejected"]),
    ];

    const KINDS: &[&str] = &["defect", "finding", "question", "rejected"];

    /// Extensions the reference scan reads. Everything this tree writes a
    /// `specs/issues/` path into is one of these, and an allow-list keeps the walk
    /// off `assets/soundfont.sf2` and the OVMF images.
    const TEXT: &[&str] = &["md", "rs", "toml", "yml", "yaml", "sh", "json", "txt"];


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

        /// What all of them may weigh together.
        ///
        /// Strictly below the sum of `BUDGETS` (96,000), because five per-file
        /// budgets can each be honoured while the set grows past what a session
        /// spanning the tree would tolerate, and nothing prices the set. 72,254 at
        /// the 2026-08-09 measurement.
        const TOTAL_BUDGET: usize = 80_000;

        fn issue_files() -> Vec<PathBuf> {
            let root = repo_root().join("specs/issues");
            let mut out = Vec::new();
            for area in std::fs::read_dir(&root).expect("specs/issues is readable").flatten() {
                if !area.path().is_dir() {
                    continue;
                }
                for f in std::fs::read_dir(area.path()).expect("an area is readable").flatten() {
                    let path = f.path();
                    if path.extension().is_some_and(|e| e == "md") {
                        out.push(path);
                    }
                }
            }
            out.sort();
            out
        }

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
        fn the_claude_md_set_is_within_its_total_budget() {
            let root = repo_root();
            let total: usize = BUDGETS
                .iter()
                .map(|(rel, _)| std::fs::metadata(root.join(rel)).expect("budgeted file").len() as usize)
                .sum();
            assert!(
                total <= TOTAL_BUDGET,
                "the CLAUDE.md set is {total} bytes against a total budget of {TOTAL_BUDGET}. \
                 Every file may be inside its own budget and the set still be too large: \
                 what an agent pays is the root plus every subtree it reads."
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

        /// The frontmatter is the only query anybody has over `specs/issues/`, so a
        /// file it cannot answer for is invisible rather than wrong.
        #[test]
        fn every_issue_is_well_formed() {
            let root = repo_root();
            let mut bad: Vec<String> = Vec::new();
            let mut slugs: BTreeMap<String, String> = BTreeMap::new();

            for entry in std::fs::read_dir(root.join("specs/issues")).expect("specs/issues").flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    if !AREAS.contains(&name.as_str()) {
                        bad.push(format!("specs/issues/{name}/ is not one of the areas the README lists"));
                    }
                } else if name != "README.md" {
                    bad.push(format!("specs/issues/{name} is not in an area directory"));
                }
            }

            for path in issue_files() {
                let shown = rel(&root, &path);
                let slug = path.file_stem().unwrap().to_string_lossy().into_owned();
                if let Some(first) = slugs.insert(slug.clone(), shown.clone()) {
                    bad.push(format!(
                        "{shown}: the slug `{slug}` is also {first}, and a slug is an issue's identity"
                    ));
                }
                let text = std::fs::read_to_string(&path).expect("an issue file is readable");
                let fm = match Frontmatter::parse(&text) {
                    Ok(fm) => fm,
                    Err(why) => {
                        bad.push(format!("{shown}: {why}"));
                        continue;
                    }
                };

                for key in fm.fields.keys() {
                    if !["status", "kind", "opened", "task"].contains(&key.as_str()) {
                        bad.push(format!("{shown}: `{key}` is not a frontmatter field"));
                    }
                }

                let kind = match fm.get("kind") {
                    Some(k) if KINDS.contains(&k) => Some(k),
                    Some(k) => {
                        bad.push(format!("{shown}: `kind: {k}` is not one of {KINDS:?}"));
                        None
                    }
                    None => {
                        bad.push(format!("{shown}: no `kind`"));
                        None
                    }
                };

                match fm.get("status") {
                    Some(s) => match STATUS_KINDS.iter().find(|(name, _)| *name == s) {
                        Some((_, allowed)) => {
                            if let Some(kind) = kind {
                                if !allowed.contains(&kind) {
                                    bad.push(format!(
                                        "{shown}: `kind: {kind}` may not carry `status: {s}` — \
                                         the README's pairing table says {allowed:?}"
                                    ));
                                }
                            }
                        }
                        None => {
                            let names: Vec<&str> = STATUS_KINDS.iter().map(|(n, _)| *n).collect();
                            bad.push(format!("{shown}: `status: {s}` is not one of {names:?}"));
                        }
                    },
                    None => bad.push(format!("{shown}: no `status`")),
                }

                match fm.get("opened") {
                    Some(d) if is_a_date(d) => {}
                    Some(d) => bad.push(format!("{shown}: `opened: {d}` is not a YYYY-MM-DD date")),
                    None => bad.push(format!("{shown}: no `opened`")),
                }

                if let Some(t) = fm.get("task") {
                    if t.parse::<u32>().is_err() {
                        bad.push(format!("{shown}: `task: {t}` is not a number"));
                    }
                }

                let body = text.splitn(3, "---\n").nth(2).unwrap_or_default();
                let headings = body.lines().filter(|l| l.starts_with("# ")).count();
                if headings != 1 {
                    bad.push(format!("{shown}: {headings} `# ` headings, and an issue has one"));
                }
            }

            assert!(
                bad.is_empty(),
                "specs/issues is what `ls` and `rg` are asked instead of an index:\n  {}",
                bad.join("\n  ")
            );
        }

        /// A reference that names a file is a claim something can check, which is
        /// the whole argument for naming one instead of an area directory. This is
        /// the something.
        #[test]
        fn every_named_issue_file_resolves() {
            let root = repo_root();
            let mut dangling: Vec<String> = Vec::new();
            walk(&root, &root, &mut |path| {
                let is_text = path
                    .extension()
                    .is_some_and(|e| TEXT.contains(&e.to_string_lossy().as_ref()));
                if !is_text {
                    return;
                }
                let Ok(text) = std::fs::read_to_string(path) else {
                    return;
                };
                for named in issue_paths_in(&text) {
                    if !root.join(&named).is_file() {
                        dangling.push(format!("{} names {named}", rel(&root, path)));
                    }
                }
            });
            dangling.sort();
            dangling.dedup();
            assert!(
                dangling.is_empty(),
                "a reference that names a file and misses is worse than one that names a directory, \
                 because it reads as checked:\n  {}",
                dangling.join("\n  ")
            );
        }

        /// The three rules above are only as good as the two things they parse,
        /// and neither is reachable from a file the repository is allowed to
        /// contain — a well-formed tree cannot exercise a rejection.
        #[test]
        fn the_parsers_refuse_what_the_rules_are_written_against() {
            assert!(is_a_date("2026-08-09"));
            assert!(is_a_date("2024-02-29"));
            assert!(!is_a_date("2026-02-30"));
            assert!(!is_a_date("2023-02-29"));
            assert!(!is_a_date("2026-13-01"));
            assert!(!is_a_date("2026-8-9"));
            assert!(!is_a_date("yesterday"));

            let fm = Frontmatter::parse("---\nstatus: open\nkind: defect\n---\n\n# x\n")
                .expect("a well-formed block parses");
            assert_eq!(fm.get("status"), Some("open"));
            assert_eq!(fm.get("task"), None);
            assert!(Frontmatter::parse("# x\n").is_err());
            assert!(Frontmatter::parse("---\nstatus: open\n").is_err());
            assert!(Frontmatter::parse("---\nstatus\n---\n").is_err());
            assert!(Frontmatter::parse("---\nkind: a\nkind: b\n---\n").is_err());

            // Assembled rather than written out: `every_named_issue_file_resolves`
            // scans this file too, and a literal fixture path is one it would
            // then try to open.
            let pfx = String::from("specs/") + "issues/";
            let found = issue_paths_in(&format!(
                "see {pfx}audio/one.md and `{pfx}build/two.md`, \
                 but not {pfx}audio/ nor {pfx}three.md"
            ));
            assert_eq!(
                found.into_iter().collect::<Vec<_>>(),
                [format!("{pfx}audio/one.md"), format!("{pfx}build/two.md")]
            );
        }
    }
