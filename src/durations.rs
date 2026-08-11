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
    let count = whole_run(&files);

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
    report(&merged, &before, count);

    let body: String = merged.iter().map(|(n, (ms, _))| format!("{n} {ms}\n")).collect();
    fs::write(&out, body).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
    println!(
        "{}: {} test(s) from {} shard file(s)",
        out.display(),
        merged.len(),
        files.len()
    );
}

/// The shard count these files are all of, refusing anything that is not a
/// whole run.
///
/// **The other half of the partition, and it was not being checked.** The
/// merge already refuses a name two shards both measured, which is
/// `specs/ci-plan.md` §4's defect from one side. From the other side a shard
/// that measured *nothing* — cancelled at its timeout, or an artifact upload
/// that failed — leaves eleven files, and merging them wrote a profile missing
/// a twelfth of the suite. Those names then price at the longest the profile
/// knows on every later run, which is exactly the eight phantom four-minute
/// tests §11.2 measured steering a twelve-way split. The command that exists to
/// keep the profile honest was the thing that could quietly break it.
///
/// The information was always there: a shard writes
/// `test-durations.shard-<i>-of-<n>`, so the file names say both how many
/// shards there were and which one each is.
fn whole_run(files: &[std::path::PathBuf]) -> usize {
    let mut seen: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut counts: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for file in files {
        let name = file.file_name().expect("a file has a name").to_string_lossy().into_owned();
        let spec = name.strip_prefix(SHARD_PREFIX).unwrap_or_else(|| {
            panic!("{name} was collected as a shard file and does not start with {SHARD_PREFIX}")
        });
        let (index, count) = spec.split_once("-of-").unwrap_or_else(|| {
            panic!("{name}: a shard file is named {SHARD_PREFIX}<index>-of-<count>")
        });
        let (index, count) = match (index.parse::<usize>(), count.parse::<usize>()) {
            (Ok(i), Ok(n)) if i >= 1 && i <= n => (i, n),
            _ => panic!("{name}: {index:?}/{count:?} is not a shard of a run"),
        };
        counts.insert(count);
        seen.entry(index).or_default().push(name);
    }

    assert!(
        counts.len() == 1,
        "these files are from more than one sharded run — shard counts {:?}. A profile merged \
         across two runs is a partition of neither.",
        counts
    );
    let count = *counts.iter().next().expect("one count");

    let twice: Vec<String> = seen
        .values()
        .filter(|f| f.len() > 1)
        .map(|f| f.join(" and "))
        .collect();
    assert!(twice.is_empty(), "one shard left two files: {}", twice.join("; "));

    let missing: Vec<String> =
        (1..=count).filter(|i| !seen.contains_key(i)).map(|i| i.to_string()).collect();
    assert!(
        missing.is_empty(),
        "shard(s) {} of {count} left no measurement, so this is not a whole run. Merging what is \
         here would write a profile missing everything those shards own, and every later run \
         would price those names at the longest this one knew — which is the imbalance the \
         profile exists to remove. Re-run the shards that did not finish.",
        missing.join(", ")
    );
    count
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn shards(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| PathBuf::from("/tmp").join(format!("{SHARD_PREFIX}{n}"))).collect()
    }

    #[test]
    fn a_whole_run_is_every_shard_of_one_run_exactly_once() {
        assert_eq!(whole_run(&shards(&["1-of-3", "2-of-3", "3-of-3"])), 3);
        assert_eq!(whole_run(&shards(&["1-of-1"])), 1);
    }

    /// Teeth, and the middle one is the defect this was written for: eleven
    /// files of a twelve-way run merged to a profile missing a twelfth of the
    /// suite, and said so in a line among others while writing it anyway.
    #[test]
    fn a_partial_or_mixed_set_is_refused_by_name() {
        let refusal = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let err = std::panic::catch_unwind(|| whole_run(&shards(&refs)))
                .expect_err("this set is not a whole run");
            err.downcast_ref::<String>().cloned().unwrap_or_default()
        };

        assert!(refusal(&["1-of-3", "3-of-3"]).contains("shard(s) 2 of 3 left no measurement"));
        assert!(refusal(&["1-of-2", "2-of-2", "1-of-3"]).contains("more than one sharded run"));
        assert!(refusal(&["1-of-2", "1-of-2", "2-of-2"]).contains("one shard left two files"));
        assert!(refusal(&["4-of-3"]).contains("is not a shard of a run"));
        assert!(refusal(&["one-of-three"]).contains("is not a shard of a run"));
        assert!(refusal(&["7"]).contains("<index>-of-<count>"));
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
