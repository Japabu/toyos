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
//! **One profile, one instrument, and the profile's instrument is the hosted
//! shards.** `tests/test-durations` holds what twelve GitHub-hosted shards
//! measured; a 1/1 partition on the T14 does not price the same tests alike —
//! `main`'s tip `13953023` measured `xhci_full_speed_device` at 6,845 ms hosted
//! and 12,156 ms on the T14 the same day, and twenty interleaved reps an arm
//! cannot tell that tip from the tree the profile was recorded on (p = 0.42).
//! So the tier verdict is rendered only where the profile was taken:
//! `.github/workflows/ci.yml`'s `durations` job matches
//! [`TIER_DISAGREEMENT`] on a T14 lane, prints it as a warning and exits 0.
//! From `985f3834` (2026-08-20) that softening covered every pull request,
//! because it had put every trusted event's guest lane on the T14; since
//! 2026-08-22 `route.yml` routes a pull request, a push, a merge-queue ref and
//! the nightly to the hosted shards, so the verdict renders for real on the run
//! that introduces a name and only a `workflow_dispatch` reaches the warning.
//! Every other refusal raised here is about the tree or the partition, not
//! about machine speed, and reds on either instrument.
//!
//! Why a command and not a `cat`: the shards are a *partition*, and that is the
//! property the merged file's usefulness rests on. A repeated name means two
//! shards claimed one test or one shard ran the same label twice — the first is
//! exactly the failure this has already produced: three shards of `nvme_` where
//! one test ran twice and one ran nowhere, and all three reported green. A
//! concatenation cannot see it; this refuses it by name.
//!
//! **A `durations` red is never ignorable on a PR**: the required `guest-suite`
//! check aggregates it, so a red here fails a required check transitively even
//! though `durations` itself is not on the required list. The usual cause is a
//! committed `UNMEASURED` marker past its one bought run — the cure is the
//! measured value from that run's own `test-durations-merged` artifact, never a
//! re-run. Learned on 2026-08-19, when three PRs stalled on exactly this while
//! everyone read the red as stale noise.
//!
//! **Two axes decide whether the price verdict is rendered, and they compose.**
//! *Which instrument* is the first, above: only the hosted twelve-shard shape
//! the profile was taken in may render it at all. *Which names* is the second,
//! and it is the owner's ruling of 2026-08-22: a run measuring a change renders
//! the verdict for the names that change registered or re-tiered, and prints
//! every other one as a `::warning::` naming the name, the price and why this
//! run does not enforce it. The nightly passes no base, so [`Enforced`] is
//! `Everything` there and the full verdict reds — fixed by a pull request the
//! next day like every other nightly red.
//!
//! The reason is measured. Over six hosted twelve-shard runs — 72 shard-runs,
//! 640 observations, 130 names — a per-shard common price factor explains 57%
//! of the run-to-run variance a name shows and spreads about 1.28x from p10 to
//! p90; it is *not* the shard's boot width (slope 0.014, R² 0.003 on the
//! merge-queue lane), so nothing normalises it away
//! (`issues/build/a-shards-boot-width-does-not-price-its-tests.md`). A name
//! priced anywhere near a line therefore reads over it on some runs by shard
//! luck, and under the required merge queue that red dequeues the composition —
//! every pull request behind it included, none of whose authors touched the
//! name. `xhci_full_speed_device` is the recorded case: six prices from 4,700
//! to 9,890 ms on one unchanged test, the last of them reding composition
//! 32550410305.
//!
//! Nothing about what a tier *is* moved: `src/tiers.rs`'s ceiling is still hard
//! and unmargined and its rule is unchanged. Nor is the tree's own bookkeeping
//! softened — a committed `UNMEASURED` marker past its bought run, a duplicate
//! execution label, a short shard set, an erased Fast label and every
//! declaration verdict [`crate::tiers::Verdict::priced`] marks `false` red on
//! every run at every base, because none of them is a fact about which shard ran
//! what.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

/// The file name a sharded run leaves its own measurement in.
const SHARD_PREFIX: &str = "test-durations.shard-";

/// The first line of the refusal this raises when the measured profile and
/// `src/tiers.rs` disagree about a tier — the price verdict, and the only
/// refusal here that a slower machine can manufacture on an innocent tree.
///
/// A `const` because `.github/workflows/ci.yml` matches on it to tell that one
/// verdict from every other way this command can refuse, and `src/ci.rs` holds
/// the two spellings together so the wording cannot move out from under the
/// workflow in silence.
pub const TIER_DISAGREEMENT: &str = "the merged CI profile and tier declaration disagree";

/// The flag a run measuring one change passes to say what it changed *from*.
///
/// A `const` for the same reason [`TIER_DISAGREEMENT`] is one: the value comes
/// out of `.github/workflows/ci.yml`'s event context — `merge_group.base_sha`
/// or `pull_request.base.sha` — and `src/ci.rs` holds the spelling in the
/// workflow against the spelling here. Drop the flag from the workflow and
/// every run silently becomes the nightly, reding compositions on names nobody
/// in them touched; misspell it and nothing at all changes, which is the same
/// failure with no line to read.
pub const TIER_BASE_FLAG: &str = "--tier-base";

/// Which names this run renders the tier's *price* verdict for.
///
/// Not which names it measures, and not which tests ran: every Fast test runs
/// on every run and every verdict is computed. This decides only which of them
/// may stop a landing.
pub enum Enforced {
    /// All of them. The nightly's twelve hosted shards, a push to `main`, a
    /// `workflow_dispatch`, a hand-run merge — anything that named no base.
    /// **A base that is absent or empty means this and never the reverse**: a
    /// workflow expression that evaluates to nothing must widen the gate, not
    /// silence it.
    Everything,
    /// Only the names the change under measurement registered, re-tiered, or
    /// gave a different `Why` in `src/tiers.rs`'s `RELEGATED`. Every other
    /// price verdict prints as a `::warning::` and the job exits 0.
    Touched { base: String, names: BTreeSet<String> },
}

impl Enforced {
    /// What this run passed, read from the command line.
    pub fn from_args(root: &Path, args: &[String]) -> Enforced {
        let Some(pos) = args.iter().position(|a| a == TIER_BASE_FLAG) else {
            return Enforced::Everything;
        };
        match args.get(pos + 1).map(String::as_str) {
            None | Some("") => Enforced::Everything,
            Some(base) => {
                Enforced::Touched { base: base.to_string(), names: touched_names(root, base) }
            }
        }
    }

    /// Whether a price verdict about `name` may stop this run.
    fn renders(&self, name: &str) -> bool {
        match self {
            Enforced::Everything => true,
            Enforced::Touched { names, .. } => names.contains(name),
        }
    }

    /// The `[durations]` line saying what this run's verdict is about, which is
    /// the line a reader of a green job needs in order to know it was green
    /// about the whole tree or about two names.
    fn scope(&self) -> String {
        match self {
            Enforced::Everything => "[durations] the tier verdict is rendered for every name: \
                 this run named no base, so it is the instrument of record"
                .to_string(),
            Enforced::Touched { base, names } if names.is_empty() => format!(
                "[durations] the tier verdict is rendered for no name: this change registered \
                 and re-tiered nothing against {base}, so every price verdict below is a \
                 warning and the nightly renders them"
            ),
            Enforced::Touched { base, names } => format!(
                "[durations] the tier verdict is rendered for the {} name(s) this change \
                 registered or re-tiered against {base}: {}",
                names.len(),
                names.iter().cloned().collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

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
    let enforced = Enforced::from_args(root, args);

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
    println!("{}", enforced.scope());

    let profile = merged_profile(&merged, &before);
    let body: String = profile.iter().map(|(n, ms)| format!("{n} {ms}\n")).collect();
    fs::write(&out, body).unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));

    let rendered = render_verdict(&profile, &before, &enforced);
    if !rendered.warned.is_empty() {
        println!(
            "[durations] {} price verdict(s) this run does not render, listed as warnings \
             above and refused by the nightly: {}",
            rendered.warned.len(),
            rendered.names.join(", ")
        );
        for warning in &rendered.warned {
            println!("::warning::{warning}");
        }
    }
    if !rendered.refused.is_empty() {
        panic!(
            "{TIER_DISAGREEMENT}:\n{}\n\
             The measured profile was written to {} for inspection",
            rendered.refused.join("\n"),
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

/// What the tier rule came to on this run: what it refuses, and what it would
/// have refused had this been the run that renders the whole verdict.
struct Rendered {
    refused: Vec<String>,
    warned: Vec<String>,
    /// The names behind `warned`, for the one summary line.
    names: Vec<String>,
}

/// The verdict issued only after the measured artifact has been written.
///
/// A new test's explicit UNMEASURED row buys exactly one KVM instrument run.
/// Even when that execution is fast, the commit carrying the marker stays red;
/// the next commit must replace it with the artifact's measured value. **That
/// refusal does not move with the base** — a committed marker is a row the
/// change itself put in the profile, so it is the change's own business on
/// every run — and neither does any verdict `tiers::Verdict::priced` marks
/// `false`. Only a measured price may be softened to a warning, and only for a
/// name this change left alone.
fn render_verdict(
    profile: &BTreeMap<String, u64>,
    before: &BTreeMap<String, u64>,
    enforced: &Enforced,
) -> Rendered {
    let mut out = Rendered { refused: Vec::new(), warned: Vec::new(), names: Vec::new() };

    let provisional: Vec<&str> = before
        .iter()
        .filter(|(_, ms)| **ms == crate::tiers::UNMEASURED_MS)
        .map(|(label, _)| label.as_str())
        .collect();
    if !provisional.is_empty() {
        out.refused.push(format!(
            "committed UNMEASURED profile marker(s) are provisional and may not land: {}. \
             Replace them with the values in the measured artifact and assign the final tier",
            provisional.join(", ")
        ));
        return out;
    }

    for verdict in crate::tiers::ci_profile_verdicts(profile) {
        if !verdict.priced {
            out.refused.push(verdict.message);
        } else if enforced.renders(&verdict.name) {
            match enforced {
                Enforced::Everything => out.refused.push(verdict.message),
                Enforced::Touched { .. } => out.refused.push(format!(
                    "{} [enforced on this run: this change registered or re-tiered {}]",
                    verdict.message, verdict.name
                )),
            }
        } else {
            out.warned.push(format!(
                "{} [not enforced on this run: this change did not register or re-tier {}, and \
                 a price near a line moves about 1.28x from p10 to p90 with the shard that ran \
                 it. The nightly's twelve hosted shards render the full verdict, and a nightly \
                 red on this name is fixed by a pull request the next day]",
                verdict.message, verdict.name
            ));
            out.names.push(verdict.name);
        }
    }
    out.names.sort();
    out.names.dedup();
    out
}

/// Every registered name this change registered, re-tiered, re-scheduled, or
/// moved into, out of, or across `src/tiers.rs`'s `RELEGATED`.
///
/// Two files answer it and no third: `tests/toyos.rs` is where a name's
/// `(name, Sched, Tier)` row lives, and `src/tiers.rs`'s `RELEGATED` is where
/// its `Why` does. A change to either is a change to what tier that name
/// claims, which is exactly the claim the price verdict grades. Everything else
/// a diff can touch — a kernel path, a test body, a `ci_ms` note — may well
/// move a price, and deliberately does not enter here: a name whose price moved
/// without its declaration moving is what the nightly is for.
fn touched_names(root: &Path, base: &str) -> BTreeSet<String> {
    let registered = changed(
        &registrations(&at_base(root, base, "tests/toyos.rs")),
        &registrations(&at_head(root, "tests/toyos.rs")),
    );
    let relegated = changed(
        &relegation_whys(&at_base(root, base, "src/tiers.rs")),
        &relegation_whys(&at_head(root, "src/tiers.rs")),
    );
    registered.union(&relegated).cloned().collect()
}

/// The keys the two sides disagree about, added and removed included.
fn changed(
    base: &BTreeMap<String, String>,
    head: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    base.keys()
        .chain(head.keys())
        .filter(|key| base.get(*key) != head.get(*key))
        .cloned()
        .collect()
}

/// One tracked file as this run's checkout has it.
fn at_head(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path))
        .unwrap_or_else(|e| panic!("reading {}: {e}", root.join(path).display()))
}

/// The same file at the base this run was asked to measure against.
///
/// **A base that was named and cannot be read is a refusal, never a silent
/// widening or a silent narrowing.** The usual cause is a depth-1 checkout: the
/// base commit is not in the clone, `git show` says so, and the fix is
/// `fetch-depth: 0` on the job rather than a guess about which names moved.
fn at_base(root: &Path, base: &str, path: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["show", &format!("{base}:{path}")])
        .output()
        .unwrap_or_else(|e| panic!("running git show {base}:{path}: {e}"));
    assert!(
        out.status.success(),
        "git show {base}:{path} failed ({}): {}. {TIER_BASE_FLAG} names the commit this change \
         is measured against, so it must be in this clone — a depth-1 checkout does not have \
         it, and `fetch-depth: 0` is what puts it there. Refusing rather than guessing which \
         names this change touched",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim(),
    );
    String::from_utf8(out.stdout).unwrap_or_else(|e| panic!("{base}:{path} is not UTF-8: {e}"))
}

/// Rust source with every `//`/`/* */` comment and every string literal's
/// *contents* blanked, byte for byte, so an offset into it is an offset into
/// the original.
///
/// Deliberately not a parser. The two tables this reads are plain data and the
/// whole job is to find brackets and keys that are code rather than prose — a
/// `guards:` string full of parentheses and a comment naming a test are exactly
/// what a `find` on the raw text gets wrong. Quotes survive so a name can still
/// be sliced out of the original by its delimiters.
fn mask(text: &str) -> Vec<u8> {
    let src = text.as_bytes();
    let mut out = vec![b' '; src.len()];
    let mut i = 0;
    while i < src.len() {
        match src[i] {
            b'/' if src.get(i + 1) == Some(&b'/') => {
                while i < src.len() && src[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if src.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < src.len() && !(src[i] == b'*' && src.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(src.len());
            }
            b'"' => {
                out[i] = b'"';
                i += 1;
                while i < src.len() && src[i] != b'"' {
                    // A string's contents become filler rather than blanks: a
                    // blank would let an empty literal and a full one look the
                    // same to a scan that only reads the mask.
                    out[i] = b'_';
                    if src[i] == b'\\' {
                        i += 1;
                        if i < src.len() {
                            out[i] = b'_';
                        }
                    }
                    i += 1;
                }
                if i < src.len() {
                    out[i] = b'"';
                    i += 1;
                }
            }
            // A char literal, told from a lifetime by what follows the quote:
            // an escape, or a single byte and a closing quote. `'"'` and `'}'`
            // are the ones that matter — either would desynchronise everything
            // after it, and `tests/toyos.rs` has a `trim_matches('"')` in it.
            // A multibyte char literal cannot hold a delimiter, so falling
            // through on one costs nothing.
            b'\'' if src.get(i + 1) == Some(&b'\\') || src.get(i + 2) == Some(&b'\'') => {
                out[i] = b'\'';
                i += 1;
                if src.get(i) == Some(&b'\\') {
                    i += 1;
                }
                i = (i + 2).min(src.len());
            }
            byte => {
                out[i] = byte;
                i += 1;
            }
        }
    }
    out
}

/// The half-open byte range of `const <name>: … = &[ … ]`'s body.
///
/// The `= ` matters: the *type* of every one of these tables starts `&[` too,
/// so a scan for the first `&[` after the name finds the declaration instead of
/// the data.
fn table_span(masked: &[u8], name: &str) -> Option<(usize, usize)> {
    let decl = find(masked, 0, &format!("const {name}:"))?;
    let eq = find(masked, decl, "] =")? + 3;
    let open = find(masked, eq, "&[")? + 2;
    Some((open, closer(masked, open, b'[', b']')?))
}

/// Where the delimiter opened just before `from` is closed, counting only
/// delimiters the mask says are code.
fn closer(masked: &[u8], from: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 1usize;
    for (offset, byte) in masked[from..].iter().enumerate() {
        if *byte == open {
            depth += 1;
        } else if *byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(from + offset);
            }
        }
    }
    None
}

/// Every top-level `open … close` span inside `[lo, hi)`.
fn spans(masked: &[u8], lo: usize, hi: usize, open: u8, close: u8) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = lo;
    for (i, byte) in masked.iter().enumerate().take(hi).skip(lo) {
        if *byte == open {
            depth += 1;
            if depth == 1 {
                start = i + 1;
            }
        } else if *byte == close && depth > 0 {
            depth -= 1;
            if depth == 0 {
                out.push((start, i));
            }
        }
    }
    out
}

/// `needle` at a code position at or after `from`.
fn find(masked: &[u8], from: usize, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    (from..masked.len().saturating_sub(needle.len() - 1))
        .find(|i| &masked[*i..*i + needle.len()] == needle)
}

/// The string literal starting at or after `from`, and where it ended.
fn literal(text: &str, masked: &[u8], from: usize, hi: usize) -> Option<(String, usize)> {
    let open = (from..hi).find(|i| masked[*i] == b'"')?;
    let close = (open + 1..hi).find(|i| masked[*i] == b'"')?;
    Some((text[open + 1..close].to_string(), close + 1))
}

/// Whitespace collapsed to single spaces, so a rewrapped line is not a change.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every registration row of `tests/toyos.rs`, name to what is declared about
/// it — the table it is in and the rest of its tuple, which is its `Sched` and
/// its `Tier`.
fn registrations(text: &str) -> BTreeMap<String, String> {
    let masked = mask(text);
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for table in ["MACHINE_TESTS", "SCREEN_TESTS", "AUDIO_TESTS"] {
        let (lo, hi) = table_span(&masked, table).unwrap_or_else(|| {
            panic!(
                "tests/toyos.rs no longer declares {table} as `const {table}: … = &[ … ]`, so \
                 which names a change registered or re-tiered cannot be read from it"
            )
        });
        for (rlo, rhi) in spans(&masked, lo, hi, b'(', b')') {
            let Some((name, after)) = literal(text, &masked, rlo, rhi) else { continue };
            let row = format!(
                "{table}({})",
                flat(text[after..rhi].trim_start().trim_start_matches(','))
            );
            // Accumulated rather than overwritten: one name declared in two
            // tables must read as two declarations, or moving it between them
            // would look like no change at all.
            out.entry(name).or_default().push_str(&row);
        }
    }
    out
}

/// Every `RELEGATED` row of `src/tiers.rs`, test name to its `Why`.
///
/// `ci_ms` and `guards` are deliberately not read: the first is a note about
/// the last measurement and the second is prose, and neither changes what tier
/// the name claims.
fn relegation_whys(text: &str) -> BTreeMap<String, String> {
    let masked = mask(text);
    let (lo, hi) = table_span(&masked, "RELEGATED").unwrap_or_else(|| {
        panic!(
            "src/tiers.rs no longer declares RELEGATED as `const RELEGATED: … = &[ … ]`, so \
             which names a change re-tiered cannot be read from it"
        )
    });
    let mut out = BTreeMap::new();
    for (rlo, rhi) in spans(&masked, lo, hi, b'{', b'}') {
        let Some(at) = find(&masked, rlo, "test:").filter(|at| *at < rhi) else { continue };
        let Some((name, _)) = literal(text, &masked, at, rhi) else { continue };
        let why = match find(&masked, rlo, "why:").filter(|at| *at < rhi) {
            // The value runs to the comma that ends the field, and
            // `Why::RidesTheBootOf("…")` has a paren pair inside it — so the
            // comma has to be one at depth zero.
            Some(at) => {
                let mut depth = 0usize;
                let end = (at + 4..rhi)
                    .find(|i| {
                        match masked[*i] {
                            b'(' => depth += 1,
                            b')' => depth = depth.saturating_sub(1),
                            b',' if depth == 0 => return true,
                            _ => {}
                        }
                        false
                    })
                    .unwrap_or(rhi);
                flat(&text[at + 4..end])
            }
            None => String::new(),
        };
        out.insert(name, why);
    }
    out
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
    use crate::tiers::{FAST_CEILING_MS, FAST_COMMIT_MS};
    use std::path::PathBuf;

    fn shards(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| PathBuf::from("/tmp").join(format!("{SHARD_PREFIX}{n}"))).collect()
    }

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn committed_profile() -> BTreeMap<String, u64> {
        read_profile(&root().join("tests/test-durations"))
    }

    /// A run measuring a change that touched exactly these names.
    fn touched(names: &[&str]) -> Enforced {
        Enforced::Touched {
            base: "0000000".to_string(),
            names: names.iter().map(|n| n.to_string()).collect(),
        }
    }

    /// The Fast name the tier tests price against. Ordinary, cheap, and not
    /// near either line, so putting it in the band is the whole mutation.
    const A_FAST_NAME: &str = "iommu_empty_domain";

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

    /// **The UNMEASURED rule does not move with the base.** A committed marker
    /// is a row the change itself put in the profile, so it is refused on the
    /// run that measures the change — including one whose change touched no
    /// name at all, which is every run where this filter could have swallowed
    /// it.
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

        for enforced in [Enforced::Everything, touched(&[]), touched(&["new_fast_test"])] {
            let rendered = render_verdict(&after, &before, &enforced);
            let refusal = rendered.refused.join("\n");
            assert!(rendered.warned.is_empty(), "{:?}", rendered.warned);
            assert!(refusal.contains("new_fast_test"), "{refusal}");
            assert!(refusal.contains("may not land"), "{refusal}");
            assert!(refusal.contains("measured artifact"), "{refusal}");
        }
    }

    /// **The name a change registered or re-tiered is the name it is refused
    /// for.** A pull request that priced its own new test in the band gets the
    /// verdict, in the same words the nightly would use, plus the reason this
    /// run is the one rendering it.
    #[test]
    fn a_changed_name_priced_without_margin_is_refused_on_a_pull_request_run() {
        let mut profile = committed_profile();
        profile.insert(A_FAST_NAME.to_string(), FAST_COMMIT_MS + 1);

        let rendered = render_verdict(&profile, &committed_profile(), &touched(&[A_FAST_NAME]));
        assert!(rendered.warned.is_empty(), "{:?}", rendered.warned);
        let refusal = rendered.refused.join("\n");
        assert!(refusal.contains(A_FAST_NAME), "{refusal}");
        assert!(refusal.contains("priced without margin"), "{refusal}");
        assert!(refusal.contains("enforced on this run"), "{refusal}");
    }

    /// **The negative control for the whole change, and the assertion that
    /// reds if the base-aware filter is removed:** `rendered.refused` is empty
    /// for a name over the commitment line that this change did not touch. The
    /// second half is the other direction — the same profile on the nightly,
    /// where the identical verdict is a refusal.
    ///
    /// This is `xhci_full_speed_device` at 9,890 ms in merge-queue composition
    /// 32550410305, in the shape a unit test can hold: a name nothing in the
    /// change under measurement went near.
    #[test]
    fn an_untouched_name_is_a_warning_on_a_landing_and_a_refusal_on_the_nightly() {
        let mut profile = committed_profile();
        profile.insert(A_FAST_NAME.to_string(), FAST_COMMIT_MS + 1);
        let before = committed_profile();

        let landing = render_verdict(&profile, &before, &touched(&["some_other_test"]));
        assert!(landing.refused.is_empty(), "{:?}", landing.refused);
        assert_eq!(landing.names, [A_FAST_NAME]);
        let warning = landing.warned.join("\n");
        assert!(warning.contains(A_FAST_NAME), "{warning}");
        assert!(warning.contains("priced without margin"), "{warning}");
        assert!(warning.contains("not enforced on this run"), "{warning}");
        assert!(warning.contains("The nightly's twelve hosted shards"), "{warning}");

        let nightly = render_verdict(&profile, &before, &Enforced::Everything);
        assert!(nightly.warned.is_empty(), "{:?}", nightly.warned);
        let refusal = nightly.refused.join("\n");
        assert!(refusal.contains(A_FAST_NAME), "{refusal}");
        assert!(refusal.contains("priced without margin"), "{refusal}");

        // The ceiling's own red is the same rule on the same axis: hard,
        // unmargined, and still the nightly's to render for a name a change
        // left alone.
        profile.insert(A_FAST_NAME.to_string(), FAST_CEILING_MS + 1);
        assert!(render_verdict(&profile, &before, &touched(&[])).refused.is_empty());
        assert!(
            render_verdict(&profile, &before, &Enforced::Everything)
                .refused
                .join("\n")
                .contains("remains Fast")
        );
    }

    /// A verdict about the *declaration* is true whoever measured it, so no
    /// base softens one. Missing evidence for a Nightly row is the case that
    /// would otherwise let a deleted test's profile row rot on every landing.
    #[test]
    fn a_declaration_verdict_is_refused_at_every_base() {
        let mut profile = committed_profile();
        profile.remove("desktop_window_child");
        let rendered = render_verdict(&profile, &committed_profile(), &touched(&[]));
        assert!(rendered.warned.is_empty(), "{:?}", rendered.warned);
        let refusal = rendered.refused.join("\n");
        assert!(refusal.contains("desktop_window_child"), "{refusal}");
        assert!(refusal.contains("missing CI evidence"), "{refusal}");
    }

    /// **A base nobody named enforces everything.** The nightly passes no
    /// flag; a workflow expression that evaluates to nothing passes an empty
    /// one; both are the instrument of record and neither may silence the
    /// gate. `--tier-base` with a real sha is the only way to narrow it, and
    /// that path asks git, so it is not exercised here.
    #[test]
    fn a_missing_or_empty_base_enforces_everything() {
        let renders = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            Enforced::from_args(&root(), &args).renders("any_name_at_all")
        };
        assert!(renders(&["--merge-durations", "/tmp/durations"]));
        assert!(renders(&["--merge-durations", "/tmp/durations", TIER_BASE_FLAG]));
        assert!(renders(&["--merge-durations", "/tmp/durations", TIER_BASE_FLAG, ""]));
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

    /// **The scan's view of `RELEGATED` against the compiler's.** `Why` derives
    /// `Debug`, so every row's parsed text can be checked against the value the
    /// compiler built from the same bytes — an independent reading of the one
    /// table, not a fixture that agrees with itself.
    #[test]
    fn the_relegation_scan_reads_what_the_compiler_reads() {
        let parsed = relegation_whys(&at_head(&root(), "src/tiers.rs"));
        let names: BTreeSet<String> =
            crate::tiers::relegated_names().iter().map(|n| n.to_string()).collect();
        assert_eq!(parsed.keys().cloned().collect::<BTreeSet<_>>(), names);
        for row in crate::tiers::RELEGATED {
            assert_eq!(
                parsed.get(row.test).map(String::as_str),
                Some(format!("Why::{:?}", row.why).as_str()),
                "{}",
                row.test
            );
        }
    }

    /// **The scan's view of `tests/toyos.rs` against `src/tiers.rs`.** Every
    /// relegated name must be registered `Tier::Nightly` and no other name may
    /// be — the same bidirectional agreement `tests/toyos.rs` gates itself on,
    /// asked here of the text this reads rather than of the values it compiles.
    /// A table that moved out from under the scan fails this rather than
    /// quietly reporting that a change touched nothing.
    #[test]
    fn the_registration_scan_agrees_with_the_relegation_table() {
        let parsed = registrations(&at_head(&root(), "tests/toyos.rs"));
        let declared_nightly: BTreeSet<&str> = parsed
            .iter()
            .filter(|(_, row)| row.contains("Tier::Nightly"))
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(declared_nightly, crate::tiers::relegated_names());
        // The three tables held 145 rows when this was written; the floor is
        // loose because the number moves with every registration, and the
        // agreement above is what actually has teeth.
        assert!(
            parsed.len() > 100,
            "{} registration row(s) found — the three tables are bigger than that",
            parsed.len()
        );
        assert_eq!(
            parsed.get("xhci_full_speed_device").map(String::as_str),
            Some("MACHINE_TESTS(Sched::Parallel, Tier::Fast)")
        );
        assert_eq!(
            parsed.get("audio_tone").map(String::as_str),
            Some("AUDIO_TESTS(Tier::Nightly)")
        );
    }

    /// **The whole path, git included**, in a repository this test builds: a
    /// base commit, an edit on top of it, and `--tier-base <sha>` naming the
    /// first. The two file paths and the `<sha>:<path>` spelling are what this
    /// has teeth on — the scan is checked against real tables elsewhere, and
    /// neither check sees the other's failure.
    #[test]
    fn the_base_a_run_names_is_read_out_of_git() {
        let dir = std::env::temp_dir().join("toyos-durations-base");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("tests")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        let registrations = |tier: &str| {
            format!(
                "const MACHINE_TESTS: &[(&str, Sched, Tier)] = &[\n    \
                 (\"kept\", Sched::Parallel, Tier::Fast),\n    \
                 (\"retiered\", Sched::Parallel, Tier::{tier}),\n];\n\
                 const SCREEN_TESTS: &[(&str, Sched, Tier)] = &[];\n\
                 const AUDIO_TESTS: &[(&str, Tier)] = &[];\n"
            )
        };
        let relegated = |rows: &str| {
            format!("pub const RELEGATED: &[Relegated] = &[\n{rows}];\n")
        };
        fs::write(dir.join("tests/toyos.rs"), registrations("Fast")).unwrap();
        fs::write(dir.join("src/tiers.rs"), relegated("")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "base"]);
        let base = String::from_utf8(
            Command::new("git").arg("-C").arg(&dir).args(["rev-parse", "HEAD"]).output().unwrap().stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        fs::write(dir.join("tests/toyos.rs"), registrations("Nightly")).unwrap();
        fs::write(
            dir.join("src/tiers.rs"),
            relegated(
                "    Relegated { test: \"retiered\", ci_ms: 9, why: Why::Cost, guards: \"g\" },\n",
            ),
        )
        .unwrap();

        let args: Vec<String> = vec!["--merge-durations".into(), "d".into(), TIER_BASE_FLAG.into(), base];
        let enforced = Enforced::from_args(&dir, &args);
        assert!(enforced.renders("retiered"));
        assert!(!enforced.renders("kept"));
        assert!(enforced.scope().contains("retiered"), "{}", enforced.scope());
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git")
            .success();
        assert!(ok, "git {args:?} in {}", dir.display());
    }

    /// What "the change touched this name" means, in the two files that answer
    /// it: a new registration, a re-tiering, a removal, a `Why` that moved, a
    /// row that appeared, a row that left — and nothing for a comment, a
    /// `ci_ms` note or a reworded `guards`.
    #[test]
    fn a_touched_name_is_one_whose_declaration_moved() {
        // The prelude is not decoration: a `'\"'` read as an opening quote, or
        // a `'}'` counted as a delimiter, desynchronises everything after it,
        // and both are in the real `tests/toyos.rs`.
        let base_reg = "\
fn quoted(s: &'static str) -> &str { s.trim_matches('\"').trim_matches('}') }
const MACHINE_TESTS: &[(&str, Sched, Tier)] = &[
    // A comment mentioning (\"ghost\", Sched::Serial, Tier::Fast).
    (\"kept\", Sched::Parallel, Tier::Fast),
    (\"retiered\", Sched::Parallel, Tier::Fast),
    (\"removed\", Sched::Parallel, Tier::Fast),
];
const SCREEN_TESTS: &[(&str, Sched, Tier)] = &[(\"rescheduled\", Sched::Parallel, Tier::Fast)];
const AUDIO_TESTS: &[(&str, Tier)] = &[(\"audio\", Tier::Nightly)];
";
        let head_reg = "\
const MACHINE_TESTS: &[(&str, Sched, Tier)] = &[
    (\"kept\", Sched::Parallel, Tier::Fast),
    (\"retiered\", Sched::Parallel, Tier::Nightly),
    (\"added\", Sched::Parallel, Tier::Fast),
];
const SCREEN_TESTS: &[(&str, Sched, Tier)] = &[(\"rescheduled\", Sched::Serial, Tier::Fast)];
const AUDIO_TESTS: &[(&str, Tier)] = &[(\"audio\", Tier::Nightly)];
";
        assert_eq!(
            changed(&registrations(base_reg), &registrations(head_reg)),
            ["added", "removed", "rescheduled", "retiered"]
                .iter()
                .map(|n| n.to_string())
                .collect::<BTreeSet<_>>()
        );

        let base_why = "\
fn quoted(s: &'static str) -> &str { s.trim_matches('\"').trim_matches('}') }
const RELEGATED: &[Relegated] = &[
    Relegated { test: \"stays\", ci_ms: 1, why: Why::Cost, guards: \"a { and a ,\" },
    Relegated { test: \"reclassified\", ci_ms: 2, why: Why::Cost, guards: \"g\" },
    Relegated { test: \"returns\", ci_ms: 3, why: Why::RidesTheBootOf(\"stays\"), guards: \"g\" },
];
";
        let head_why = "\
const RELEGATED: &[Relegated] = &[
    // 2026-08-22: `returns` left this table.
    Relegated { test: \"stays\", ci_ms: 99, why: Why::Cost, guards: \"reworded\" },
    Relegated { test: \"reclassified\", ci_ms: 2, why: Why::TimerAnchored, guards: \"g\" },
    Relegated { test: \"relegated\", ci_ms: 4, why: Why::Cost, guards: \"g\" },
];
";
        assert_eq!(
            changed(&relegation_whys(base_why), &relegation_whys(head_why)),
            ["reclassified", "relegated", "returns"]
                .iter()
                .map(|n| n.to_string())
                .collect::<BTreeSet<_>>()
        );
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
