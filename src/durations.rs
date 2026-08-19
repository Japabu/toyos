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
//! property the merged file's usefulness rests on. A repeated name means two
//! shards claimed one test or one shard ran the same label twice — the first is
//! exactly the failure this has already produced: three shards of `nvme_` where
//! one test ran twice and one ran nowhere, and all three reported green. A
//! concatenation cannot see it; this refuses it by name.

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
            insert_measurement(&mut merged, name, ms, &who);
        }
    }

    let out = root.join("tests/test-durations");
    let before = read_profile(&out);
    report(&merged, &before, count);

    let profile = merged_profile(&merged, &before);
    let body: String = profile.iter().map(|(n, ms)| format!("{n} {ms}\n")).collect();
    fs::write(&out, body).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
    if let Err(refusal) = validate_written_profile(&profile, &before) {
        panic!(
            "the merged CI profile and tier declaration disagree:\n{refusal}\n\
             The measured profile was written to {} for inspection",
            out.display()
        );
    }
    println!(
        "{}: {} measured test(s) from {} shard file(s), {} timing row(s) written",
        out.display(),
        merged.len(),
        files.len(),
        profile.len(),
    );
}

/// The verdict issued only after the measured artifact has been written.
///
/// A new test's explicit UNMEASURED row buys exactly one KVM instrument run.
/// Even when that execution is fast, the commit carrying the marker stays red;
/// the next commit must replace it with the artifact's measured value.
fn validate_written_profile(
    profile: &BTreeMap<String, u64>,
    before: &BTreeMap<String, u64>,
) -> Result<(), String> {
    let provisional: Vec<&str> = before
        .iter()
        .filter(|(_, ms)| **ms == crate::tiers::UNMEASURED_MS)
        .map(|(label, _)| label.as_str())
        .collect();
    if !provisional.is_empty() {
        return Err(format!(
            "committed UNMEASURED profile marker(s) are provisional and may not land: {}. \
             Replace them with the values in the measured artifact and assign the final tier",
            provisional.join(", ")
        ));
    }
    crate::tiers::validate_ci_profile(profile)
}

/// Add one execution label to a whole-run profile.
///
/// A duplicate is never a second sample. Across files it means two shards
/// disagreed about ownership; within one file it means a shard ran one label
/// twice. Keeping either duration would let the other verdict disappear.
fn insert_measurement(
    merged: &mut BTreeMap<String, (u64, String)>,
    name: &str,
    ms: u64,
    who: &str,
) {
    if let Some((_, first)) = merged.insert(name.to_string(), (ms, who.to_string())) {
        panic!(
            "{name} was measured twice, first in {first} and again in {who}. Every execution \
             label must occur exactly once: two shards may disagree about ownership, or one \
             shard may have run the same test twice"
        );
    }
}

/// The profile a completed sharded run leaves behind.
///
/// Fast CI intentionally does not run the nightly tier, so absence from its
/// shard files is not evidence that a nightly timing row is stale. Preserve
/// those committed measurements while still letting a run that did measure a
/// nightly test replace its old number. Every other absent row is a refusal:
/// otherwise a complete-looking shard set could drop a Fast test and erase the
/// only evidence that the required duration gate should have expected it.
fn merged_profile(
    measured: &BTreeMap<String, (u64, String)>,
    before: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    let mut after: BTreeMap<String, u64> =
        measured.iter().map(|(name, (ms, _))| (name.clone(), *ms)).collect();
    let nightly = crate::tiers::relegated_names();
    let missing_fast: Vec<&str> = before
        .keys()
        .filter(|label| !measured.contains_key(*label))
        .filter(|label| !nightly.contains(crate::tiers::canonical_profile_name(label)))
        .map(String::as_str)
        .collect();
    assert!(
        missing_fast.is_empty(),
        "the completed shard set did not measure Fast profile label(s): {}. A successful \
         fast run may omit only Nightly labels; delete a removed test's committed profile \
         row in the same change that removes its registration",
        missing_fast.join(", ")
    );
    for (label, ms) in before {
        if nightly.contains(crate::tiers::canonical_profile_name(label)) {
            after.entry(label.clone()).or_insert(*ms);
        }
    }
    after
}

/// The shard count these files are all of, refusing anything that is not a
/// whole run.
///
/// **The other half of the partition, and it was not being checked.** The
/// merge already refuses any duplicate execution label, including the
/// observed defect where two shards claimed one name. From the other side a
/// shard that measured *nothing* — cancelled at its timeout, or an artifact
/// upload that failed — leaves eleven files, and merging them wrote a profile missing
/// a twelfth of the suite. Those names then price at the longest the profile
/// knows on every later run, which is exactly the eight phantom four-minute
/// tests measured steering a twelve-way split. The command that exists to
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

    #[test]
    fn a_fast_only_merge_preserves_committed_nightly_timings() {
        let nightly = crate::tiers::relegated_names()
            .into_iter()
            .find(|name| *name != "audio_tone_load")
            .expect("an ordinary nightly test");
        let measured = BTreeMap::from([("fast".to_string(), (120, "shard-1".to_string()))]);
        let before = BTreeMap::from([
            ("fast".to_string(), 999),
            (nightly.to_string(), 45_000),
        ]);

        let after = merged_profile(&measured, &before);
        assert_eq!(after.get("fast"), Some(&120));
        assert_eq!(after.get(nightly), Some(&45_000));
    }

    #[test]
    fn a_complete_fast_run_may_not_erase_an_unmeasured_fast_label() {
        let measured = BTreeMap::from([("some_fast_test".to_string(), (120, "shard-1".to_string()))]);
        let before = BTreeMap::from([
            ("some_fast_test".to_string(), 999),
            ("missing_fast_test".to_string(), 321),
        ]);
        let err = std::panic::catch_unwind(|| merged_profile(&measured, &before))
            .expect_err("a complete fast run silently erased a Fast label");
        let refusal = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(refusal.contains("missing_fast_test"), "{refusal}");
        assert!(refusal.contains("may omit only Nightly"), "{refusal}");
    }

    #[test]
    fn one_shard_may_not_report_the_same_execution_label_twice() {
        let mut merged = BTreeMap::new();
        insert_measurement(&mut merged, "foo", 11_001, "test-durations.shard-1-of-12");
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            insert_measurement(&mut merged, "foo", 1, "test-durations.shard-1-of-12");
        }))
        .expect_err("the later short timing overwrote an over-ceiling execution");
        let refusal = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(refusal.contains("foo was measured twice"), "{refusal}");
        assert!(refusal.contains("one shard may have run"), "{refusal}");
    }

    #[test]
    fn an_unmeasured_marker_buys_one_red_measurement_commit() {
        let measured = BTreeMap::from([(
            "new_fast_test".to_string(),
            (321, "test-durations.shard-1-of-12".to_string()),
        )]);
        let before =
            BTreeMap::from([("new_fast_test".to_string(), crate::tiers::UNMEASURED_MS)]);
        let after = merged_profile(&measured, &before);
        assert_eq!(after.get("new_fast_test"), Some(&321));

        let refusal = validate_written_profile(&after, &before).unwrap_err();
        assert!(refusal.contains("new_fast_test"), "{refusal}");
        assert!(refusal.contains("may not land"), "{refusal}");
        assert!(refusal.contains("measured artifact"), "{refusal}");
    }

    #[test]
    fn a_measured_nightly_timing_replaces_the_committed_one() {
        let nightly = crate::tiers::relegated_names()
            .into_iter()
            .find(|name| *name != "audio_tone_load")
            .expect("an ordinary nightly test");
        let measured = BTreeMap::from([(
            nightly.to_string(),
            (12_345, "shard-1".to_string()),
        )]);
        let before = BTreeMap::from([(nightly.to_string(), 45_000)]);

        assert_eq!(merged_profile(&measured, &before).get(nightly), Some(&12_345));
    }

    #[test]
    fn audio_config_labels_follow_their_one_nightly_registration() {
        let measured = BTreeMap::from([
            ("fast".to_string(), (120, "shard-1".to_string())),
            ("audio_tone (smp=1)".to_string(), (7_000, "shard-1".to_string())),
            ("not_audio (smp=8)".to_string(), (8_000, "shard-1".to_string())),
        ]);
        let before = BTreeMap::from([
            ("audio_tone_load (smp=1)".to_string(), 40_524),
            ("audio_tone_load (smp=8)".to_string(), 11_121),
            ("audio_tone (smp=1)".to_string(), 8_156),
            ("not_audio (smp=8)".to_string(), 99_999),
        ]);

        let after = merged_profile(&measured, &before);
        assert_eq!(after.get("audio_tone_load (smp=1)"), Some(&40_524));
        assert_eq!(after.get("audio_tone_load (smp=8)"), Some(&11_121));
        assert_eq!(after.get("audio_tone (smp=1)"), Some(&7_000));
        assert_eq!(after.get("not_audio (smp=8)"), Some(&8_000));
    }
}
