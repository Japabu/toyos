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

    let out = root.join("tests/test-durations");
    let before = read_profile(&out);
    report(&merged, &before, files.len());

    let body: String = merged.iter().map(|(n, (ms, _))| format!("{n} {ms}\n")).collect();
    fs::write(&out, body).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
    println!(
        "{}: {} test(s) from {} shard file(s)",
        out.display(),
        merged.len(),
        files.len()
    );
}

fn read_profile(path: &Path) -> BTreeMap<String, u64> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.rsplit_once(' '))
        .filter_map(|(n, ms)| ms.parse().ok().map(|ms| (n.to_string(), ms)))
        .collect()
}

/// What the run this merges was actually partitioned into, and what the profile
/// it partitioned on had to say about it.
///
/// **Both halves are measurements and neither is a model.** The spread is the
/// shard files' own totals; the ideal is their sum over the shard count. Nobody
/// has to be told what a better partition would have produced, because the run
/// that produced these files already answered it.
///
/// The unpriced names are the ones that made this worth printing. `Shard::keep`
/// costs a name the profile has never seen at the longest that *was* measured —
/// deliberate conservatism, and eight such names in run `31331494794` were
/// eight phantom four-minute tests steering a twelve-way partition. Nothing in
/// the tree noticed: a test added without a profile entry is silent, and it
/// stays silent until somebody reads two shard timings side by side.
fn report(
    merged: &BTreeMap<String, (u64, String)>,
    before: &BTreeMap<String, u64>,
    shards: usize,
) {
    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    for (ms, who) in merged.values() {
        *totals.entry(who.as_str()).or_default() += ms;
    }
    let (low, high) = (
        totals.values().min().copied().unwrap_or(0),
        totals.values().max().copied().unwrap_or(0),
    );
    let ideal = merged.values().map(|(ms, _)| ms).sum::<u64>() / shards.max(1) as u64;
    println!(
        "[durations] the shards measured {:.1}s to {:.1}s of tests; an even split is {:.1}s, \
         so this partition cost {:.1}s of critical path",
        low as f64 / 1000.0,
        high as f64 / 1000.0,
        ideal as f64 / 1000.0,
        (high.saturating_sub(ideal)) as f64 / 1000.0,
    );

    let unpriced: Vec<&str> =
        merged.keys().filter(|n| !before.contains_key(*n)).map(String::as_str).collect();
    if !unpriced.is_empty() {
        println!(
            "[durations] {} name(s) the profile did not price, each costed at the longest it \
             knew: {}",
            unpriced.len(),
            unpriced.join(", ")
        );
    }
    let gone: Vec<&str> =
        before.keys().filter(|n| !merged.contains_key(*n)).map(String::as_str).collect();
    if !gone.is_empty() {
        println!(
            "[durations] {} name(s) the profile prices and no shard ran: {}",
            gone.len(),
            gone.join(", ")
        );
    }
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
