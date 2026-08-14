//! What `specs/issues/` promises, and the gates that make it bind: frontmatter
//! shape and reference resolution.
//!
//! The frontmatter is the only query anybody has over two hundred files, and a
//! field nothing checks is a field that is right until the day it is not.

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
#[cfg(test)]
const SKIP: &[&str] = &["target", "node_modules", "rust", ".git", ".claude"];

#[cfg(test)]
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[cfg(test)]
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
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
        crate::day::Day::parse(s).is_some()
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
