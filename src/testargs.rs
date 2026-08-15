//! The suite's command line, checked against the flags it actually has.
//!
//! `tests/toyos.rs` reads its flags by name and takes the first remaining
//! positional argument as the run's filter. A flag it does not have therefore
//! costs nothing and its *value* becomes that filter, so a command line naming
//! a deleted flag runs one test and reports the run as a pass, and nothing
//! between such a command line and a green check refuses it.
//!
//! So the flag table is here, one entry per flag the harness reads, and the
//! filter falls out of the same pass rather than out of a second guess about
//! which words were already spoken for. A flag added to the harness and not to
//! this table is refused the first time anyone types it — the drift that is
//! loud rather than the one that narrows a gate.

use std::time::Duration;

/// One machine's slice of the suite.
///
/// A shard is a *host*, never a lane. `--jobs` divides one machine's cores
/// between guests that contend for them; this divides the work between machines
/// that share nothing, which is the only lever CI has and the one the dev host
/// does not have at all. The two compose: four shards at width 4 is sixteen
/// guests that no `HostSlots` has to count, because no two are on one host.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Shard {
    /// One-based, as it is written on the command line and in a job matrix.
    pub index: usize,
    pub count: usize,
}

impl Shard {
    /// The empty accumulator [`keep`](Self::keep) fills, one bin per shard.
    ///
    /// The only way to make one, so a caller cannot hand `keep` a vector of the
    /// wrong width; what it *can* still do is make a second one, which is the
    /// defect the doc on `keep` names.
    pub fn bins(self) -> Vec<Duration> {
        vec![Duration::ZERO; self.count]
    }

    /// Drop everything another shard owns, keeping the order of what is left.
    ///
    /// Longest-processing-time on the measured duration profile the suite
    /// already orders its queue by, because a shard's wall clock is its bin's
    /// total and the run's is the fullest bin. `items` is read in the order
    /// given, so a list already sorted descending gets LPT's bound and one that
    /// is not still gets a complete, deterministic partition — **every item
    /// lands in exactly one shard whatever the profile says**, which is the
    /// property a verdict depends on and the one the gates below hold.
    ///
    /// **`load` is the run's one accumulator, not this call's.** A suite that
    /// partitions several pools — the parallel tasks, the serial tail, gate A's
    /// configs — is one machine's wall clock either way, so the second pool has
    /// to fill the bins the first left light. Starting each call from
    /// [`bins`](Self::bins) makes each partition good and their sum bad, and
    /// the imbalances add: measured over run `31377439504`'s twelve shards it
    /// was a widest shard of 466.1 s against an even split of 369.1 s, where
    /// one accumulator over the same items put the widest bin at 363.9 s.
    /// Thread one through the calls, heaviest pool first.
    ///
    /// Every process partitioning one run must therefore make the same calls in
    /// the same order over the same items: the bins each call leaves are the
    /// next call's input, so a shard that skipped a pool would price every later
    /// one differently and the twelve would stop being a partition.
    ///
    /// `None` is an item the profile has never seen, and it is priced at the
    /// longest that was measured *in its own pool* — the same conservatism
    /// `longest_first` expresses by sorting unknowns first, in a form that can
    /// be added up. Where *nothing* was measured, every item prices the same and
    /// LPT degenerates to round-robin, which is the best a machine with no
    /// profile can do and is what every runner's first run gets.
    pub fn keep<T>(
        self,
        items: &mut Vec<T>,
        load: &mut [Duration],
        cost: impl Fn(&T) -> Option<Duration>,
    ) {
        assert_eq!(
            load.len(),
            self.count,
            "a {}-way shard reads {} bins, and a partition over the wrong number of them \
             would not be one",
            self.count,
            load.len()
        );
        let unmeasured = items
            .iter()
            .filter_map(&cost)
            .max()
            .unwrap_or(Duration::from_secs(1));
        let mut owner = Vec::with_capacity(items.len());
        for item in items.iter() {
            let bin = (0..self.count).min_by_key(|&b| load[b]).expect("count >= 1");
            load[bin] += cost(item).unwrap_or(unmeasured);
            owner.push(bin);
        }
        let mut i = 0;
        items.retain(|_| {
            let mine = owner[i] == self.index - 1;
            i += 1;
            mine
        });
    }
}

/// `--shard <index>/<count>`, or `None` for the whole suite.
///
/// `Err` is a refusal to print and exit on, like [`parse`]'s: a shard number
/// outside its range would take no tests and report the run green.
pub fn parse_shard(args: &[String]) -> Result<Option<Shard>, String> {
    let mut out = None;
    for (i, a) in args.iter().enumerate() {
        let spec = if let Some(v) = a.strip_prefix("--shard=") {
            v
        } else if a == "--shard" {
            args.get(i + 1)
                .map(String::as_str)
                .ok_or("--shard needs a slice, e.g. --shard 2/4")?
        } else {
            continue;
        };
        let (index, count) = spec
            .split_once('/')
            .ok_or_else(|| format!("--shard {spec}: not <index>/<count>, e.g. 2/4"))?;
        let index: usize = index
            .parse()
            .map_err(|_| format!("--shard {spec}: {index:?} is not a shard number"))?;
        let count: usize = count
            .parse()
            .map_err(|_| format!("--shard {spec}: {count:?} is not a shard count"))?;
        if !(1..=count).contains(&index) {
            return Err(format!(
                "--shard {spec}: shards are numbered 1 through {count}, and a run outside \
                 that range would take no tests and report itself green"
            ));
        }
        out = Some(Shard { index, count });
    }
    Ok(out)
}

/// Refuse a shard that owns nothing after the ordinary suite's filter, tier,
/// and task grouping have all been applied.
///
/// A valid shard number is not enough to establish that the selected suite has
/// at least that many bins. The check therefore belongs after `Shard::keep`,
/// where `total` is the number of verdicts this process can actually produce.
pub fn validate_ordinary_shard(
    shard: Option<Shard>,
    filter: Option<&str>,
    total: usize,
) -> Result<(), String> {
    let Some(shard) = shard else { return Ok(()) };
    if total > 0 {
        return Ok(());
    }
    Err(format!(
        "--shard {}/{} with filter {filter:?} owns no ordinary-tier tests after selection; \
         refusing a false-green shard run",
        shard.index, shard.count,
    ))
}

/// Whether a flag is followed by a separate word, which is then not the filter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Value {
    None,
    Required,
}

pub struct Flag {
    pub name: &'static str,
    pub value: Value,
}

const fn flag(name: &'static str, value: Value) -> Flag {
    Flag { name, value }
}

/// Every flag `tests/toyos.rs` reads, and nothing else.
pub const FLAGS: &[Flag] = &[
    flag("--debug", Value::None),
    flag("--list", Value::None),
    flag("--nocapture", Value::None),
    flag("--show-output", Value::None),
    flag("--audio-gate", Value::Required),
    flag("--jobs", Value::Required),
    flag("-j", Value::Required),
    flag("--host-slots", Value::Required),
    flag("--host-builds", Value::Required),
    flag("--shard", Value::Required),
    flag("--slow-usb", Value::None),
    flag("--nightly", Value::None),
];

fn accepted() -> String {
    FLAGS
        .iter()
        .map(|f| match f.value {
            Value::None => f.name.to_string(),
            Value::Required => format!("{} <value>", f.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validate the harness's argv and return the run's filter.
///
/// `Err` is a refusal to print and exit on. It is asked before the sysroot lock
/// and before anything is compiled, so a stale command line costs a message
/// rather than a queue behind it.
pub fn parse(args: &[String]) -> Result<Option<&str>, String> {
    let mut filter: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if !arg.starts_with('-') {
            if let Some(first) = filter {
                return Err(format!(
                    "{first:?} and {arg:?}: the suite takes one filter, and the second word \
                     would have been dropped in silence.\n\
                     A filter is a substring, so `{first}` and `{arg}` are one run only if one \
                     substring matches both."
                ));
            }
            filter = Some(arg);
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, _)) => (name, true),
            None => (arg, false),
        };
        let Some(f) = FLAGS.iter().find(|f| f.name == name) else {
            return Err(format!(
                "{arg}: the suite has no such flag, and an unknown flag's value becomes the \
                 run's filter — so this would have measured whatever one test it named.\n\
                 Flags it has: {}.",
                accepted()
            ));
        };
        if inline && f.value == Value::None {
            return Err(format!("{arg}: {name} takes no value.\nFlags it has: {}.", accepted()));
        }
        if !inline && f.value == Value::Required {
            i += 1;
        }
    }
    let has = |want: &str| {
        args.iter().any(|arg| arg == want || arg.strip_prefix(want).is_some_and(|v| v.starts_with('=')))
    };
    if has("--nightly") && has("--audio-gate") {
        return Err(
            "--nightly and --audio-gate are separate tiers and cannot be combined; run one \
             tier at a time"
                .to_string(),
        );
    }
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_owned(args: &[&str]) -> Result<Option<String>, String> {
        let owned: Vec<String> = args.iter().map(ToString::to_string).collect();
        parse(&owned).map(|f| f.map(ToString::to_string))
    }

    /// The incident: `--skip` was deleted with the expected-failure declaration,
    /// and every handover still carried it.
    #[test]
    fn a_deleted_flag_is_refused_rather_than_becoming_the_filter() {
        let refusal = parse_owned(&["--skip", "desktop_window_child"]).unwrap_err();
        assert!(refusal.starts_with("--skip:"), "{refusal}");
        assert!(refusal.contains("--jobs <value>"), "{refusal}");
    }

    #[test]
    fn a_flags_value_is_not_the_filter() {
        assert_eq!(parse_owned(&["--jobs", "4"]).unwrap(), None);
        assert_eq!(parse_owned(&["-j", "4"]).unwrap(), None);
        assert_eq!(parse_owned(&["--audio-gate", "30"]).unwrap(), None);
        assert_eq!(parse_owned(&["--host-slots", "0"]).unwrap(), None);
        assert_eq!(parse_owned(&["--host-builds", "0"]).unwrap(), None);
    }

    #[test]
    fn nightly_and_audio_gate_are_refused_by_the_argv_validator() {
        for argv in [
            vec!["--nightly", "--audio-gate", "30"],
            vec!["--audio-gate=30", "--nightly"],
        ] {
            let refusal = parse_owned(&argv).unwrap_err();
            assert!(refusal.contains("--nightly"), "{refusal}");
            assert!(refusal.contains("--audio-gate"), "{refusal}");
            assert!(refusal.contains("cannot be combined"), "{refusal}");
        }
    }

    #[test]
    fn the_filter_is_the_word_that_is_nobodys_value() {
        assert_eq!(parse_owned(&["process_stats"]).unwrap().as_deref(), Some("process_stats"));
        assert_eq!(
            parse_owned(&["--audio-gate", "30", "audio_tone", "--nocapture"]).unwrap().as_deref(),
            Some("audio_tone")
        );
        assert_eq!(
            parse_owned(&["--jobs=4", "futex", "--show-output"]).unwrap().as_deref(),
            Some("futex")
        );
    }

    #[test]
    fn two_filters_are_refused_because_only_one_would_run() {
        let refusal = parse_owned(&["futex", "dlopen"]).unwrap_err();
        assert!(refusal.contains("\"futex\"") && refusal.contains("\"dlopen\""), "{refusal}");
    }

    fn shard_of(args: &[&str]) -> Result<Option<Shard>, String> {
        parse_shard(&args.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn a_shard_is_index_and_count() {
        assert_eq!(shard_of(&["--shard", "2/4"]).unwrap(), Some(Shard { index: 2, count: 4 }));
        assert_eq!(shard_of(&["--shard=1/1"]).unwrap(), Some(Shard { index: 1, count: 1 }));
        assert_eq!(shard_of(&[]).unwrap(), None);
    }

    /// The failure with no symptom: a shard nobody owns runs nothing, and a run
    /// that ran nothing exits 0.
    #[test]
    fn a_shard_outside_its_range_is_refused() {
        for spec in ["0/4", "5/4", "2/0"] {
            let refusal = shard_of(&["--shard", spec]).unwrap_err();
            assert!(refusal.contains("green"), "{spec}: {refusal}");
        }
        assert!(shard_of(&["--shard", "half"]).is_err());
        assert!(shard_of(&["--shard", "x/4"]).is_err());
    }

    #[test]
    fn an_empty_selected_shard_is_a_named_false_green() {
        let shard = Some(Shard { index: 8, count: 12 });
        let refusal = validate_ordinary_shard(shard, Some("one_test"), 0).unwrap_err();
        assert!(refusal.contains("--shard 8/12"), "{refusal}");
        assert!(refusal.contains("filter Some(\"one_test\")"), "{refusal}");
        assert!(refusal.contains("false-green"), "{refusal}");

        assert!(validate_ordinary_shard(shard, None, 1).is_ok());
        assert!(validate_ordinary_shard(None, Some("nothing"), 0).is_ok());
    }

    /// The property every verdict rests on: the shards are a partition. Not one
    /// test may be dropped by all of them, and none may be run by two.
    #[test]
    fn every_item_lands_in_exactly_one_shard() {
        let items: Vec<u64> = (0..97).map(|i| (i * 37) % 23).collect();
        for count in 1..=8 {
            let mut seen: Vec<u64> = Vec::new();
            for index in 1..=count {
                let shard = Shard { index, count };
                let mut mine = items.clone();
                shard.keep(&mut mine, &mut shard.bins(), |&c| Some(Duration::from_secs(c)));
                seen.extend(mine);
            }
            seen.sort_unstable();
            let mut want = items.clone();
            want.sort_unstable();
            assert_eq!(seen, want, "count {count}");
        }
    }

    /// A shard's wall clock is its bin's total, so the split has to be by cost
    /// and not by position. Descending input is what the suite hands it.
    #[test]
    fn the_split_is_by_cost_and_not_by_position() {
        let items: Vec<u64> = vec![100, 90, 80, 70, 60, 50, 40, 30];
        let totals: Vec<u64> = (1..=4)
            .map(|index| {
                let shard = Shard { index, count: 4 };
                let mut mine = items.clone();
                shard.keep(&mut mine, &mut shard.bins(), |&c| Some(Duration::from_secs(c)));
                mine.iter().sum()
            })
            .collect();
        assert_eq!(totals, vec![130, 130, 130, 130], "{totals:?}");
    }

    /// **One run is one accumulator.** The suite partitions three pools — the
    /// parallel tasks, the serial tail, gate A's configs — and a shard runs all
    /// three, so the second call has to fill the bins the first left light. Two
    /// pools of `[3 s, 1 s]` across two shards is the smallest case that tells
    /// the two apart: threaded, both shards take 4 s; from a fresh accumulator
    /// each time, the heavy item lands on shard 1 twice and the widest bin is
    /// 6 s against an even split of 4 s.
    #[test]
    fn a_second_pool_fills_the_bins_the_first_left_light() {
        let cost = |&c: &u64| Some(Duration::from_secs(c));
        let (mut threaded, mut apart) = (Vec::new(), Vec::new());
        let (mut kept, mut kept_apart) = (Vec::new(), Vec::new());
        for index in 1..=2 {
            let shard = Shard { index, count: 2 };

            let (mut first, mut second) = (vec![3u64, 1], vec![3u64, 1]);
            let mut load = shard.bins();
            shard.keep(&mut first, &mut load, cost);
            shard.keep(&mut second, &mut load, cost);
            threaded.push(first.iter().chain(&second).sum::<u64>());
            kept.extend(first.iter().chain(&second).copied());

            // The defect, spelled out with the same function: a second
            // accumulator knows nothing about what the first one placed.
            let (mut first, mut second) = (vec![3u64, 1], vec![3u64, 1]);
            shard.keep(&mut first, &mut shard.bins(), cost);
            shard.keep(&mut second, &mut shard.bins(), cost);
            apart.push(first.iter().chain(&second).sum::<u64>());
            kept_apart.extend(first.iter().chain(&second).copied());
        }
        assert_eq!(apart, vec![6, 2], "the defect's own numbers: {apart:?}");
        assert_eq!(threaded, vec![4, 4], "one accumulator splits it evenly: {threaded:?}");
        assert!(
            threaded.iter().max() < apart.iter().max(),
            "widest bin threaded {threaded:?} against apart {apart:?}"
        );

        // And it is still a partition: threading changes which shard owns an
        // item, never how many own it.
        for mut got in [kept, kept_apart] {
            got.sort_unstable();
            assert_eq!(got, vec![1, 1, 3, 3], "every item exactly once");
        }
    }

    /// A test the profile has never seen costs `Duration::MAX` so that it sorts
    /// first, and a machine with no recorded profile at all — every runner's
    /// first run — has a whole suite of them. Plain addition panicked on the
    /// second item, which is what the first sharded CI run found.
    #[test]
    fn a_suite_with_no_measured_profile_still_splits_evenly() {
        let items: Vec<usize> = (0..10).collect();
        let mut seen: Vec<usize> = Vec::new();
        let mut sizes = Vec::new();
        for index in 1..=3 {
            let shard = Shard { index, count: 3 };
            let mut mine = items.clone();
            shard.keep(&mut mine, &mut shard.bins(), |_| None);
            sizes.push(mine.len());
            seen.extend(mine);
        }
        seen.sort_unstable();
        assert_eq!(seen, items);
        assert_eq!(sizes, vec![4, 3, 3], "{sizes:?}");
    }

    #[test]
    fn an_inline_value_on_a_flag_that_has_none_is_refused() {
        let refusal = parse_owned(&["--nocapture=1"]).unwrap_err();
        assert!(refusal.contains("--nocapture"), "{refusal}");
    }

    #[test]
    fn the_documented_command_lines_parse() {
        for argv in [
            vec![],
            vec!["--nocapture"],
            vec!["process_stats"],
            vec!["process_stats", "--nocapture"],
            vec!["--list"],
            vec!["--audio-gate", "30"],
            vec!["--jobs", "4"],
            vec!["--host-slots", "0"],
            vec!["--host-builds", "0"],
            vec!["--shard", "2/4"],
            vec!["--nightly"],
            vec!["--debug"],
        ] {
            assert!(parse_owned(&argv).is_ok(), "{argv:?}");
        }
    }
}
