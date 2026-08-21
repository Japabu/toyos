//! `cargo run -- --merge-health` — what the eased merge law is trading, measured.
//!
//! `issues/build/the-eased-merge-law-carries-a-threshold.md` names the trade the
//! owner made on 2026-08-20: `main`'s checks are its own branch's, not the
//! merged result's, and a red-`main` incident is adjudicated after the fact
//! rather than refused before it. That is a deliberate correctness/throughput
//! trade, not a free one, and the same file fixes a threshold for it in
//! advance — near-zero expected, one interaction failure or more than one
//! red-`main` incident in a rolling week breaches it. This is the instrument
//! that reads the rate back, on demand and from the nightly schedule alike, so
//! the threshold is checked against measurement rather than memory.
//!
//! **What "red" means here is the four push-triggered workflows** that fire on
//! every push to `main` — `ci` (whose `guest-suite` job is the required
//! check), `host tests` (`host`), `toolchain` (`build`), `landing`
//! (`gate-stage`; `abi-split` is a no-op on a push event) — read at the
//! workflow level. That is a coarser reading than the required *check* names
//! `landing.yml`'s `gate-stage` job enforces, but a push-triggered run has no
//! other job competing to fail it, so workflow conclusion and required-check
//! conclusion agree in practice; the first backfill (this file's own dated
//! report) verified that by hand against the job-level breakdown.
//!
//! **What this does not do**, on purpose: it does not read job logs to name
//! *which test* failed, and it does not classify a red as an interaction
//! failure versus an ordinary flake — both are judgment calls a human or an
//! agent makes by reading the run, the way the first backfill did. This prints
//! the raw counts, the check each red named, and the mechanical half of the
//! verdict (incident count against the threshold); a breach on interaction
//! failures specifically is never something a rate alone can say.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::day::Day;

const TAG: &str = "[merge-health]";

/// The four workflows that trigger on every push to `main`
/// (`.github/workflows/{ci,host-tests,toolchain,landing}.yml`), and the
/// required check each one's push-triggered run stands in for.
const REQUIRED_WORKFLOWS: &[(&str, &str)] =
    &[("ci", "guest-suite"), ("host tests", "host"), ("toolchain", "build"), ("landing", "gate-stage")];

/// `cargo run -- --merge-health [--since <RFC3339>] [--days N]`.
///
/// `--since` pins the window's start exactly, which is how the dated report
/// in `the-eased-merge-law-carries-a-threshold.md` was reproduced. With
/// neither flag the window is the rolling week the threshold itself is stated
/// against, ending now.
pub fn dispatch(root: &Path, args: &[String]) {
    let now = now_epoch_secs();
    let since = if let Some(pos) = args.iter().position(|a| a == "--since") {
        let text = args
            .get(pos + 1)
            .unwrap_or_else(|| panic!("--since needs an RFC3339 instant: --since <YYYY-MM-DDTHH:MM:SSZ>"));
        parse_instant(text)
    } else if let Some(pos) = args.iter().position(|a| a == "--days") {
        let text = args.get(pos + 1).unwrap_or_else(|| panic!("--days needs a count"));
        let days: i64 = text.parse().unwrap_or_else(|_| panic!("--days: {text:?} is not a count"));
        assert!(days >= 1, "--days must be at least 1");
        now - days * 86_400
    } else {
        now - 7 * 86_400
    };
    assert!(since < now, "the window's start must be before now");

    let runs = fetch(root, since);
    let report = render(&runs, since, now);
    print!("{report}");
}

/// One push-triggered workflow run, as `gh` reported it.
#[derive(Clone)]
struct Run {
    head_sha: String,
    workflow: String,
    status: String,
    conclusion: String,
    created_at: i64,
    updated_at: i64,
}

/// Every push-triggered run on `main` since `since`, oldest first.
///
/// `gh`'s own `--jq` does the JSON-to-text extraction — no JSON parser needed
/// here, the same reason `forkcheck.rs` parses `git ls-remote`'s plain output
/// rather than asking git for structured data.
fn fetch(root: &Path, since: i64) -> Vec<Run> {
    let since_text = format_instant(since);
    let filter = format!(">={since_text}");
    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--branch",
            "main",
            "--event",
            "push",
            "--created",
            &filter,
            "--limit",
            "500",
            "--json",
            "headSha,workflowName,status,conclusion,createdAt,updatedAt",
            "--jq",
            ".[] | [.headSha, .workflowName, .status, .conclusion, .createdAt, .updatedAt] | @tsv",
        ])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("{TAG} run `gh run list`: {e} — is `gh` on PATH and authenticated?"));
    assert!(
        output.status.success(),
        "{TAG} `gh run list` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("gh printed non-UTF-8");
    let mut runs: Vec<Run> = text
        .lines()
        .map(|line| {
            let mut f = line.split('\t');
            let mut next = || f.next().unwrap_or_else(|| panic!("{TAG} short TSV row: {line:?}"));
            let head_sha = next().to_string();
            let workflow = next().to_string();
            let status = next().to_string();
            let conclusion = next().to_string();
            let created_at = parse_instant(next());
            let updated_at = parse_instant(next());
            Run { head_sha, workflow, status, conclusion, created_at, updated_at }
        })
        .collect();
    runs.sort_by_key(|r| r.created_at);
    runs
}

/// One push's four required-workflow runs, keyed by workflow name.
struct Push {
    head_sha: String,
    created_at: i64,
    by_workflow: BTreeMap<String, Run>,
}

/// Every run into one push per `headSha`, in the order the pushes landed.
///
/// **Not a partition and does not refuse an incomplete one** — unlike
/// `durations.rs`'s shard merge, a live window's newest push may still have
/// runs in flight, and that is expected, not a defect. A push missing an
/// expected workflow's row entirely (as opposed to it being present but
/// `in_progress`) is refused: the four workflows fire on every push and a
/// permanently absent row means this tool misread `gh`, not that the push is
/// exempt.
fn group(runs: &[Run]) -> Vec<Push> {
    let mut order: Vec<String> = Vec::new();
    let mut by_sha: BTreeMap<String, Push> = BTreeMap::new();
    for run in runs {
        if !by_sha.contains_key(&run.head_sha) {
            order.push(run.head_sha.clone());
            by_sha.insert(
                run.head_sha.clone(),
                Push { head_sha: run.head_sha.clone(), created_at: run.created_at, by_workflow: BTreeMap::new() },
            );
        }
        let push = by_sha.get_mut(&run.head_sha).expect("just inserted");
        let prior = push.by_workflow.insert(run.workflow.clone(), run.clone());
        assert!(
            prior.is_none(),
            "{TAG} {} ran {} more than once in one push — `gh run list` returned a duplicate, \
             or two pushes share a headSha",
            run.head_sha,
            run.workflow
        );
    }
    order
        .into_iter()
        .map(|sha| by_sha.remove(&sha).expect("inserted above"))
        .collect()
}

/// One continuous stretch of a required check reporting red on `main`'s tip.
struct RedInterval {
    workflow: &'static str,
    started: i64,
    /// `None` while the check has not yet reported green again — still red
    /// as of this run of the tool.
    ended: Option<i64>,
}

/// The red/green history of every required workflow, walked once across the
/// pushes in order.
///
/// A `cancelled` run (superseded before it could run or finish) says nothing
/// about the check and neither opens nor closes an interval — the same
/// reading `landing.yml`'s own gate gives a skipped job.
fn red_intervals(pushes: &[Push]) -> Vec<RedInterval> {
    let mut open: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut closed = Vec::new();
    for push in pushes {
        for (workflow, _check) in REQUIRED_WORKFLOWS {
            let Some(run) = push.by_workflow.get(*workflow) else { continue };
            match run.conclusion.as_str() {
                "failure" => {
                    open.entry(workflow).or_insert(run.updated_at);
                }
                "success" => {
                    if let Some(started) = open.remove(workflow) {
                        closed.push(RedInterval { workflow, started, ended: Some(run.updated_at) });
                    }
                }
                _ => {} // cancelled, in_progress, queued, pending: silent on the check's state
            }
        }
    }
    for (workflow, started) in open {
        closed.push(RedInterval { workflow, started, ended: None });
    }
    closed
}

/// `p` of `sorted`, nearest-rank: `sorted[ceil(p * n) - 1]`. Matches what the
/// dated backfill computed by hand; documented rather than imported because
/// nothing else in this crate needs a percentile yet.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    assert!(!sorted.is_empty(), "percentile of nothing");
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn render(runs: &[Run], since: i64, now: i64) -> String {
    let pushes = group(runs);
    let mut out = String::new();
    let window_days = (now - since) as f64 / 86_400.0;
    out += &format!(
        "{TAG} window {} .. {} ({:.2} days), {} push(es) to main\n",
        format_instant(since),
        format_instant(now),
        window_days,
        pushes.len()
    );

    let mut red_by_workflow: BTreeMap<&str, u32> = BTreeMap::new();
    let mut red_pushes = 0u32;
    let mut preempted_pushes = 0u32;
    let mut still_validating = 0u32;
    let mut red_rows: Vec<String> = Vec::new();
    for push in &pushes {
        let mut this_red = false;
        let mut this_preempted = false;
        let mut this_pending = false;
        for (workflow, _) in REQUIRED_WORKFLOWS {
            let run = push.by_workflow.get(*workflow);
            match run.map(|r| r.conclusion.as_str()) {
                Some("failure") => {
                    *red_by_workflow.entry(workflow).or_default() += 1;
                    this_red = true;
                    red_rows.push(format!(
                        "{} {} ({}) — {}",
                        format_instant(push.created_at),
                        short(&push.head_sha),
                        workflow,
                        run.map(|r| r.updated_at).map(format_instant).unwrap_or_default()
                    ));
                }
                Some("cancelled") => this_preempted = true,
                Some(_) | None => {} // success, or missing: neither red nor preempted
            }
            if run.is_none_or(|r| r.conclusion.is_empty() && r.status != "completed") {
                this_pending = true;
            }
        }
        red_pushes += this_red as u32;
        preempted_pushes += this_preempted as u32;
        still_validating += this_pending as u32;
    }

    let pct = |n: u32| if pushes.is_empty() { 0.0 } else { 100.0 * n as f64 / pushes.len() as f64 };
    out += &format!(
        "{TAG} red-main incidents: {red_pushes} of {} push(es) ({:.1} %)\n",
        pushes.len(),
        pct(red_pushes)
    );
    for (workflow, n) in &red_by_workflow {
        let check = REQUIRED_WORKFLOWS.iter().find(|(w, _)| w == workflow).map(|(_, c)| *c).unwrap_or("?");
        out += &format!("{TAG}   {workflow} ({check}): {n}\n");
    }
    for row in &red_rows {
        out += &format!("{TAG}     {row}\n");
    }
    out += &format!(
        "{TAG} validation preempted before completion: {preempted_pushes} of {} push(es) ({:.1} %) \
         — a later push superseded this one's required-check run before it finished\n",
        pushes.len(),
        pct(preempted_pushes)
    );
    if still_validating > 0 {
        out += &format!(
            "{TAG} still validating at snapshot time: {still_validating} push(es), not yet counted \
             above either way\n"
        );
    }

    let intervals = red_intervals(&pushes);
    let closed_minutes: Vec<f64> =
        intervals.iter().filter_map(|i| i.ended.map(|e| (e - i.started) as f64 / 60.0)).collect();
    let total: f64 = closed_minutes.iter().sum();
    out += &format!(
        "{TAG} red-main minutes: total {total:.1}, p95 {:.1} (over {} closed interval(s))\n",
        if closed_minutes.is_empty() {
            0.0
        } else {
            let mut sorted = closed_minutes.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            percentile(&sorted, 0.95)
        },
        closed_minutes.len()
    );
    for i in intervals.iter().filter(|i| i.ended.is_none()) {
        out += &format!(
            "{TAG}   STILL RED: {} since {} ({:.1} minutes and counting)\n",
            i.workflow,
            format_instant(i.started),
            (now - i.started) as f64 / 60.0
        );
    }

    let breach = red_pushes > 1;
    out += &format!(
        "{TAG} verdict: {}\n",
        if breach {
            "THRESHOLD BREACHED — more than one red-main incident in this window. Per \
             issues/build/the-eased-merge-law-carries-a-threshold.md, the stronger \
             serialization is now the mandatory response: batch landings under the \
             orchestrator, or the organization move that unlocks GitHub's merge queue. \
             (Interaction-failure classification is not automated here — read the reds \
             above before deciding whether any is one; the incident-count breach stands \
             regardless.)"
        } else {
            "within threshold — 0 or 1 red-main incident(s) in this window, and this tool \
             does not classify interaction failures. Near-zero is what the same issue file \
             expects; a rolling week with more than one still ends this line differently."
        }
    );
    out
}

/// The first nine hex digits of a commit, the width `git`'s own short form
/// settles on for a repository this size.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a host clock before 1970 is a host to fix, not a date to guess at")
        .as_secs() as i64
}

/// Seconds since the epoch, from an RFC3339 UTC instant exactly as GitHub's
/// API writes one: `YYYY-MM-DDTHH:MM:SSZ`, no fractional seconds, no offset.
fn parse_instant(text: &str) -> i64 {
    let bytes = text.as_bytes();
    assert!(
        text.len() == 20
            && bytes.get(4) == Some(&b'-')
            && bytes.get(7) == Some(&b'-')
            && bytes.get(10) == Some(&b'T')
            && bytes.get(13) == Some(&b':')
            && bytes.get(16) == Some(&b':')
            && bytes.get(19) == Some(&b'Z'),
        "{text:?} is not YYYY-MM-DDTHH:MM:SSZ, the only shape gh's API and this tool's own \
         --since accept"
    );
    let field = |r: std::ops::Range<usize>| {
        text.get(r.clone())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or_else(|| panic!("{text:?}: {r:?} is not a number"))
    };
    let day = Day::parse(&text[0..10]).unwrap_or_else(|| panic!("{text:?}: not a calendar day"));
    let epoch = Day::parse("1970-01-01").expect("the epoch parses");
    let (h, mi, s) = (field(11..13), field(14..16), field(17..19));
    assert!(h < 24 && mi < 60 && s < 60, "{text:?}: {h:02}:{mi:02}:{s:02} is not a time of day");
    epoch.until(day) * 86_400 + h * 3600 + mi * 60 + s
}

/// The inverse of [`parse_instant`]: an epoch-seconds instant as
/// `YYYY-MM-DDTHH:MM:SSZ`.
///
/// `day.rs` is deliberately day-only ("the resolution of every question asked
/// of it here") and does not carry a formatter; this needs one, so it is
/// local, and [`tests::civil_round_trips_through_day_parse`] checks it against
/// `Day::parse` rather than trusting the arithmetic on its own.
fn format_instant(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let rem = epoch_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`, the exact inverse of the
/// `days_from_civil` [`Day::parse`] runs — proleptic Gregorian, every date the
/// calendar has.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_through_day_parse() {
        for text in ["1970-01-01", "2026-08-19", "2026-08-20", "2024-02-29", "2000-03-01", "1999-12-31"] {
            let day = Day::parse(text).unwrap();
            let epoch = Day::parse("1970-01-01").unwrap();
            let (y, m, d) = civil_from_days(epoch.until(day));
            assert_eq!(format!("{y:04}-{m:02}-{d:02}"), text, "{text}");
        }
    }

    #[test]
    fn an_instant_round_trips() {
        for text in ["2026-08-19T23:14:24Z", "2026-01-01T00:00:00Z", "2026-12-31T23:59:59Z"] {
            assert_eq!(format_instant(parse_instant(text)), text);
        }
    }

    #[test]
    fn parse_instant_rejects_the_wrong_shape() {
        for bad in ["2026-08-19 23:14:24Z", "2026-08-19T23:14:24+02:00", "not a date", ""] {
            let ok = std::panic::catch_unwind(|| parse_instant(bad)).is_ok();
            assert!(!ok, "{bad:?} should have been refused");
        }
    }

    fn run(head_sha: &str, workflow: &str, conclusion: &str, created: &str, updated: &str) -> Run {
        Run {
            head_sha: head_sha.to_string(),
            workflow: workflow.to_string(),
            status: "completed".to_string(),
            conclusion: conclusion.to_string(),
            created_at: parse_instant(created),
            updated_at: parse_instant(updated),
        }
    }

    #[test]
    fn a_red_then_green_run_closes_one_interval() {
        let runs = vec![
            run("a", "ci", "failure", "2026-08-20T00:00:00Z", "2026-08-20T00:10:00Z"),
            run("b", "ci", "success", "2026-08-20T00:20:00Z", "2026-08-20T00:30:00Z"),
        ];
        let pushes = group(&runs);
        let intervals = red_intervals(&pushes);
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0].workflow, "ci");
        assert_eq!(intervals[0].ended, Some(parse_instant("2026-08-20T00:30:00Z")));
        assert_eq!((intervals[0].ended.unwrap() - intervals[0].started) as f64 / 60.0, 20.0);
    }

    #[test]
    fn a_cancelled_run_neither_opens_nor_closes_an_interval() {
        let runs = vec![
            run("a", "ci", "failure", "2026-08-20T00:00:00Z", "2026-08-20T00:10:00Z"),
            run("b", "ci", "cancelled", "2026-08-20T00:11:00Z", "2026-08-20T00:11:05Z"),
            run("c", "ci", "success", "2026-08-20T00:20:00Z", "2026-08-20T00:30:00Z"),
        ];
        let intervals = red_intervals(&group(&runs));
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0].ended, Some(parse_instant("2026-08-20T00:30:00Z")));
    }

    #[test]
    fn a_red_that_never_recovers_is_still_red_and_open() {
        let runs = vec![run("a", "ci", "failure", "2026-08-20T00:00:00Z", "2026-08-20T00:10:00Z")];
        let intervals = red_intervals(&group(&runs));
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0].ended, None);
    }

    #[test]
    fn two_pushes_sharing_one_workflow_twice_is_refused() {
        let runs = vec![
            run("a", "ci", "success", "2026-08-20T00:00:00Z", "2026-08-20T00:10:00Z"),
            run("a", "ci", "success", "2026-08-20T00:00:00Z", "2026-08-20T00:11:00Z"),
        ];
        let ok = std::panic::catch_unwind(|| group(&runs)).is_ok();
        assert!(!ok, "one push naming one workflow twice should be refused");
    }

    #[test]
    fn percentile_nearest_rank_matches_the_hand_backfill() {
        let sorted = [7.0, 10.4, 24.3, 297.4];
        assert_eq!(percentile(&sorted, 0.95), 297.4);
    }

    #[test]
    fn more_than_one_red_push_breaches_the_threshold() {
        let runs = vec![
            run("a", "ci", "failure", "2026-08-20T00:00:00Z", "2026-08-20T00:10:00Z"),
            run("a", "host tests", "success", "2026-08-20T00:00:00Z", "2026-08-20T00:05:00Z"),
            run("b", "ci", "success", "2026-08-20T00:20:00Z", "2026-08-20T00:30:00Z"),
            run("c", "landing", "failure", "2026-08-20T00:40:00Z", "2026-08-20T00:41:00Z"),
        ];
        let report = render(&runs, parse_instant("2026-08-20T00:00:00Z"), parse_instant("2026-08-20T01:00:00Z"));
        assert!(report.contains("red-main incidents: 2 of 3"), "{report}");
        assert!(report.contains("THRESHOLD BREACHED"), "{report}");
    }

    #[test]
    fn one_or_zero_red_pushes_stays_within_threshold() {
        let runs = vec![
            run("a", "ci", "failure", "2026-08-20T00:00:00Z", "2026-08-20T00:10:00Z"),
            run("b", "ci", "success", "2026-08-20T00:20:00Z", "2026-08-20T00:30:00Z"),
        ];
        let report = render(&runs, parse_instant("2026-08-20T00:00:00Z"), parse_instant("2026-08-20T01:00:00Z"));
        assert!(report.contains("red-main incidents: 1 of 2"), "{report}");
        assert!(report.contains("within threshold"), "{report}");
    }
}
