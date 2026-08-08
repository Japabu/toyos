//! Merging what a sharded run measured into the profile the next one reads.
//!
//! A runner is a fresh clone and has no `target/test-durations`, so
//! `longest_first` prices every test the same and `Shard::keep`'s LPT
//! degenerates to round-robin. That put 191 of 268 tests on one shard of run
//! `31238056513` and cut it off at its job timeout while another finished in
//! sixteen minutes. `tests/test-durations` is the answer and it is committed,
//! because the machines that need it are the ones that have run nothing.
//!
//! **Its numbers come from a runner and not from here**, deliberately:
//! cross-arch TCG on an M4 Pro and KVM on four Azure cores do not agree about
//! which tests are long, the dev host overwrites every name it measures with its
//! own, and the file exists for the checkout that has measured nothing.
//!
//! Why a command and not a `cat`: the shards are a *partition*, and that is the
//! property the merged file's usefulness rests on. A name in two of them means
//! two shards ran the same test — which is exactly the failure
//! `specs/ci-plan.md` §4 records, three shards of `nvme_` where one test ran
//! twice and one ran nowhere, and all three reported green. A concatenation
//! cannot see it; this refuses it by name.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// The file name a sharded run leaves its own measurement in.
const SHARD_PREFIX: &str = "test-durations.shard-";

/// `--merge-durations <dir>`: every shard file under `dir`, into
/// `tests/test-durations`.
///
/// `dir` is where `gh run download` put the artifacts, so the files sit one
/// level down in a directory per shard; the walk is recursive for that reason
/// and for no other.
pub fn dispatch(root: &Path, args: &[String]) {
    let Some(pos) = args.iter().position(|a| a == "--merge-durations") else {
        unreachable!("dispatched on the flag being there")
    };
    let dir = args.get(pos + 1).unwrap_or_else(|| {
        panic!("--merge-durations needs the directory the shard files are in")
    });
    let dir = Path::new(dir);

    let mut files = Vec::new();
    collect(dir, &mut files);
    assert!(
        !files.is_empty(),
        "no {SHARD_PREFIX}* under {}: a sharded run uploads one per shard",
        dir.display()
    );

    let mut merged: BTreeMap<String, (u64, String)> = BTreeMap::new();
    for file in &files {
        let who = file.file_name().expect("a file has a name").to_string_lossy().into_owned();
        let text = fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        for line in text.lines() {
            let Some((name, ms)) = line.rsplit_once(' ') else { continue };
            let Ok(ms) = ms.parse::<u64>() else { continue };
            if let Some((_, first)) = merged.insert(name.to_string(), (ms, who.clone())) {
                assert_eq!(
                    first, who,
                    "{name} was measured by both {first} and {who}, so those two shards were \
                     not a partition and one of them ran a test the other owned — the profile \
                     they were taken with disagreed with itself"
                );
            }
        }
    }

    let body: String = merged.iter().map(|(n, (ms, _))| format!("{n} {ms}\n")).collect();
    let out = root.join("tests/test-durations");
    fs::write(&out, body).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
    println!(
        "{}: {} test(s) from {} shard file(s)",
        out.display(),
        merged.len(),
        files.len()
    );
}

fn collect(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(SHARD_PREFIX))
        {
            out.push(path);
        }
    }
}
