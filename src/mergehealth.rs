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
//!
//! **The regime is read, not assumed.** The eased law was itself provisional
//! — `landing.yml`'s `gate-stage` job reads `main`'s branch protection back on
//! every push and already prints whether a `merge_queue` rule is present; this
//! file asks the same question through `gh api repos/{owner}/{repo}/rules/branches/main`,
//! the endpoint `gate-stage` reads, and looks for `merge_queue` among the rule
//! types exactly the way `gate-stage`'s own `queued=...` line does. If it is
//! there, the window is split at the instant the queue started, and the two
//! parts are reported (and verdicted) separately, then totalled as before. A
//! `gh` call that cannot answer at all (no network, no auth) is the only case
//! this falls back on: it says so and reports the window undivided, exactly
//! as this file did before it knew regimes existed — it never guesses one.
//!
//! **The queue's start is the earliest `merge_group`-triggered workflow run**
//! on `gh-readonly-queue/main/*` on record, not the ruleset's own `updated_at`:
//! `updated_at` moves on *any* later edit to *any* rule, an unrelated
//! required-check addition included, and would silently misdate the regime
//! rather than refusing; the first `merge_group` run is a fact about the queue
//! actually processing a landing and cannot be perturbed that way. (Read at
//! most the 500 most recent such runs — this instrument refuses rather than
//! silently under-counting if the queue ever outgrows that.) If the rule is
//! required but has never yet run one, the boundary is "now": nothing in any
//! window so far can be attributed to a queue nobody has used.
//!
//! **Under a queue, an incident is not the same finding it was under the
//! eased law, and a red on `main`'s tip is not by itself evidence the queue
//! failed.** The eased-part verdict keeps its original threshold and text —
//! it is the record of why the queue came, not a live gate any more. The
//! queue-part verdict has no threshold to breach, but it also does not treat
//! every push-triggered red as a queue failure: a merge queue validates a
//! commit in a separate run, on the `merge_group` event, *before* landing it
//! ("the composition run"), and that run is a different execution from the
//! push-triggered run ("the tip run") that fires after the merge on `main`'s
//! own history — so the tip run can still catch an ordinary flake the
//! composition run did not happen to hit, with no composition failure
//! involved at all. [`fetch_composition`]'s doc comment has the verified fact
//! this keys on: a `merge_group` run's `headSha` is the exact commit that
//! becomes `main`'s tip, so [`verdict_queued`] looks up each incident's own
//! composition run by that shared key rather than assuming guilt. Composition
//! green for every incident is `QUEUE HELD` — the tip run's red is a fact
//! about that run, adjudicated against `src/redlist.rs` like any other red,
//! never charged to the queue. Composition red or missing for even one is
//! `QUEUE DID NOT HOLD`, naming only those — a commit that reached `main`'s
//! tip without validating clean first is the specific failure a queue exists
//! to prevent, and root `CLAUDE.md`'s rule that a red on `main`'s tip is
//! adjudicated at once, not batched into a rolling-week rate, applies to it
//! directly.

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
    let report = match regime(root) {
        Regime::Unknown => render(&runs, since, now),
        Regime::Eased => render_split(&runs, since, now, Regime::Eased, &BTreeMap::new()),
        known @ Regime::Queued(_) => {
            let composition = composition_lookup(&fetch_composition(root, since));
            render_split(&runs, since, now, known, &composition)
        }
    };
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
    parse_runs_tsv(&String::from_utf8(output.stdout).expect("gh printed non-UTF-8"))
}

/// Every `merge_group`-triggered run on `main`'s queue since `since` — the
/// composition run the queue validates *before* landing a commit, as opposed
/// to [`fetch`]'s push-triggered run on `main`'s tip *after* landing it.
/// Filtered to `gh-readonly-queue/main/*` because `--event merge_group` has
/// no `--branch` filter of its own (a `merge_group` run's branch is the
/// synthetic queue ref, never `main`).
///
/// **Verified, not assumed, 2026-08-22:** a `merge_group` run's `headSha` is
/// the exact commit that becomes `main`'s tip — `gh run list --event
/// merge_group --json headSha,conclusion,databaseId,createdAt` against
/// `gh-readonly-queue/main/pr-174-abad07f3f8a77cf225309a3dcb487df9d7d994b3`
/// returned `headSha: 625afce1b444f08ad656babebce4b7fc154fde09`, the same sha
/// `fetch`'s push-triggered run for that landing carries — so a queue-regime
/// incident's push-triggered red is looked up in this table by the identical
/// `(head_sha, workflow)` key [`group`] already uses for push runs.
fn fetch_composition(root: &Path, since: i64) -> Vec<Run> {
    let since_text = format_instant(since);
    let filter = format!(">={since_text}");
    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--event",
            "merge_group",
            "--created",
            &filter,
            "--limit",
            "500",
            "--json",
            "headSha,workflowName,status,conclusion,createdAt,updatedAt,headBranch",
            "--jq",
            r#".[] | select(.headBranch | startswith("gh-readonly-queue/main/")) | [.headSha, .workflowName, .status, .conclusion, .createdAt, .updatedAt] | @tsv"#,
        ])
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| {
            panic!("{TAG} run `gh run list --event merge_group`: {e} — is `gh` on PATH and authenticated?")
        });
    assert!(
        output.status.success(),
        "{TAG} `gh run list --event merge_group` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_runs_tsv(&String::from_utf8(output.stdout).expect("gh printed non-UTF-8"))
}

/// The six-field TSV shape both [`fetch`] and [`fetch_composition`] ask `gh`
/// for, oldest first.
fn parse_runs_tsv(text: &str) -> Vec<Run> {
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

/// [`fetch_composition`]'s runs, grouped into one composition-validation
/// [`Push`] per merged commit and keyed by that commit's `head_sha` — the
/// lookup [`verdict_queued`] reads a queue-regime incident's composition
/// status from, by the same `(head_sha, workflow)` pair `group` already keys
/// push runs on.
fn composition_lookup(runs: &[Run]) -> BTreeMap<String, Push> {
    group(runs).into_iter().map(|p| (p.head_sha.clone(), p)).collect()
}

/// The composition run's conclusion for `head_sha`'s `workflow`, or `None` if
/// no `merge_group` run named that pair — a commit the queue never validated
/// before it reached `main`'s tip.
fn composition_conclusion<'a>(
    composition: &'a BTreeMap<String, Push>,
    head_sha: &str,
    workflow: &str,
) -> Option<&'a str> {
    composition.get(head_sha)?.by_workflow.get(workflow).map(|r| r.conclusion.as_str())
}

/// What `main`'s branch protection says right now, read the way
/// `landing.yml`'s `gate-stage` job reads it.
enum Regime {
    /// `gh` could not answer at all — no network, no auth. Never a guess:
    /// [`dispatch`] falls back to the undivided report this file always gave.
    Unknown,
    /// No `merge_queue` rule on `main`: the eased law, for the whole window.
    Eased,
    /// A `merge_queue` rule is required, in effect since this instant — the
    /// earliest `merge_group`-triggered run on record, or now if the rule is
    /// required but has never yet run one.
    Queued(i64),
}

/// Reads `main`'s ruleset through `gh api repos/{owner}/{repo}/rules/branches/main`
/// — the same endpoint `gate-stage` reads — and asks whether `merge_queue` is
/// among its rule types, the same test `gate-stage`'s own `queued=...` line
/// runs. This file's module header has why the queue's start comes from the
/// earliest `merge_group` run rather than the ruleset's `updated_at`.
fn regime(root: &Path) -> Regime {
    let Ok(rules_out) = Command::new("gh")
        .args(["api", "repos/{owner}/{repo}/rules/branches/main", "--jq", "[.[].type] | join(\" \")"])
        .current_dir(root)
        .output()
    else {
        return Regime::Unknown;
    };
    if !rules_out.status.success() {
        return Regime::Unknown;
    }
    let rules = String::from_utf8_lossy(&rules_out.stdout);
    if !rules.split_whitespace().any(|r| r == "merge_queue") {
        return Regime::Eased;
    }

    let Ok(mg_out) = Command::new("gh")
        .args([
            "run",
            "list",
            "--event",
            "merge_group",
            "--limit",
            "500",
            "--json",
            "createdAt,headBranch",
            "--jq",
            r#"[.[] | select(.headBranch | startswith("gh-readonly-queue/main/"))] | sort_by(.createdAt) | [length, (.[0].createdAt // "")] | @tsv"#,
        ])
        .current_dir(root)
        .output()
    else {
        return Regime::Unknown;
    };
    if !mg_out.status.success() {
        return Regime::Unknown;
    }
    let text = String::from_utf8_lossy(&mg_out.stdout);
    let mut fields = text.trim().split('\t');
    let count: usize = fields
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("{TAG} `gh run list --event merge_group` printed no count row"));
    assert!(
        count < 500,
        "{TAG} 500 merge_group runs on gh-readonly-queue/main/* is this instrument's ceiling for \
         finding the earliest one — the queue has outgrown it, widen the query rather than trust \
         a possibly-truncated \"earliest\""
    );
    let earliest = fields.next().unwrap_or("");
    if earliest.is_empty() {
        // Required, but the queue has never processed a landing: nothing in
        // any window so far can be "queued" either.
        Regime::Queued(now_epoch_secs())
    } else {
        Regime::Queued(parse_instant(earliest))
    }
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

fn header_line(pushes: &[Push], since: i64, now: i64) -> String {
    let window_days = (now - since) as f64 / 86_400.0;
    format!(
        "{TAG} window {} .. {} ({:.2} days), {} push(es) to main\n",
        format_instant(since),
        format_instant(now),
        window_days,
        pushes.len()
    )
}

/// One `(push, required-workflow)` pair that went red — the unit
/// [`verdict_queued`] cross-checks against the composition run that
/// validated (or did not validate) the same commit before it landed.
struct RedIncident {
    push_created_at: i64,
    head_sha: String,
    workflow: &'static str,
    run_updated_at: i64,
}

/// One partition's counts and detail rows — red incidents (with which
/// required check and which push each one named), preempted and
/// still-validating pushes, and the closed/open red-time intervals within
/// just this slice of pushes. Computed the same way whether the partition is
/// the whole window or one side of a regime split.
struct Tally {
    red_pushes: u32,
    preempted_pushes: u32,
    still_validating: u32,
    red_by_workflow: BTreeMap<&'static str, u32>,
    red_rows: Vec<String>,
    red_incidents: Vec<RedIncident>,
    intervals: Vec<RedInterval>,
}

fn tally(pushes: &[Push]) -> Tally {
    let mut red_by_workflow: BTreeMap<&str, u32> = BTreeMap::new();
    let mut red_pushes = 0u32;
    let mut preempted_pushes = 0u32;
    let mut still_validating = 0u32;
    let mut red_incidents: Vec<RedIncident> = Vec::new();
    for push in pushes {
        let mut this_red = false;
        let mut this_preempted = false;
        let mut this_pending = false;
        for (workflow, _) in REQUIRED_WORKFLOWS {
            let run = push.by_workflow.get(*workflow);
            match run.map(|r| r.conclusion.as_str()) {
                Some("failure") => {
                    *red_by_workflow.entry(workflow).or_default() += 1;
                    this_red = true;
                    red_incidents.push(RedIncident {
                        push_created_at: push.created_at,
                        head_sha: push.head_sha.clone(),
                        workflow,
                        run_updated_at: run.map(|r| r.updated_at).unwrap_or(push.created_at),
                    });
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
    let red_rows: Vec<String> = red_incidents
        .iter()
        .map(|i| {
            format!(
                "{} {} ({}) — {}",
                format_instant(i.push_created_at),
                short(&i.head_sha),
                i.workflow,
                format_instant(i.run_updated_at)
            )
        })
        .collect();
    let intervals = red_intervals(pushes);
    Tally { red_pushes, preempted_pushes, still_validating, red_by_workflow, red_rows, red_incidents, intervals }
}

/// The stat-line block `render` always printed after the header — pulled out
/// so a partition's block and the aggregate "totals" block share one
/// implementation instead of three copies of the same arithmetic.
fn format_tally(out: &mut String, pushes_len: usize, t: &Tally, now: i64) {
    let pct = |n: u32| if pushes_len == 0 { 0.0 } else { 100.0 * n as f64 / pushes_len as f64 };
    *out += &format!(
        "{TAG} red-main incidents: {} of {pushes_len} push(es) ({:.1} %)\n",
        t.red_pushes,
        pct(t.red_pushes)
    );
    for (workflow, n) in &t.red_by_workflow {
        let check = REQUIRED_WORKFLOWS.iter().find(|(w, _)| w == workflow).map(|(_, c)| *c).unwrap_or("?");
        *out += &format!("{TAG}   {workflow} ({check}): {n}\n");
    }
    for row in &t.red_rows {
        *out += &format!("{TAG}     {row}\n");
    }
    *out += &format!(
        "{TAG} validation preempted before completion: {} of {pushes_len} push(es) ({:.1} %) \
         — a later push superseded this one's required-check run before it finished\n",
        t.preempted_pushes,
        pct(t.preempted_pushes)
    );
    if t.still_validating > 0 {
        *out += &format!(
            "{TAG} still validating at snapshot time: {} push(es), not yet counted \
             above either way\n",
            t.still_validating
        );
    }

    let closed_minutes: Vec<f64> =
        t.intervals.iter().filter_map(|i| i.ended.map(|e| (e - i.started) as f64 / 60.0)).collect();
    let total: f64 = closed_minutes.iter().sum();
    *out += &format!(
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
    for i in t.intervals.iter().filter(|i| i.ended.is_none()) {
        *out += &format!(
            "{TAG}   STILL RED: {} since {} ({:.1} minutes and counting)\n",
            i.workflow,
            format_instant(i.started),
            (now - i.started) as f64 / 60.0
        );
    }
}

/// The eased-law verdict text — unchanged by the regime split, on purpose:
/// per this file's module header it is the record of why the queue came, not
/// a live gate any more.
fn verdict_eased(red_pushes: u32) -> String {
    let breach = red_pushes > 1;
    format!(
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
    )
}

/// The queue-regime verdict: no rolling-week threshold, because a merge
/// queue's whole point is pre-merge composition testing. A red on `main`'s
/// tip alone does not mean the queue failed at that job — [`fetch_composition`]'s
/// module header has the verified fact this keys on: the `merge_group` run
/// that validated the same commit shares its `head_sha`, so each incident is
/// rendered against that run's own conclusion rather than assumed guilty.
/// Composition green for every incident means the queue did exactly what it
/// exists to do and the push-triggered red is a fact about *that* execution,
/// adjudicated against `src/redlist.rs` like any other red — not a
/// composition failure and not a rate question. Composition red or missing
/// for even one is the failure a queue exists to prevent, named at once per
/// root `CLAUDE.md`.
fn verdict_queued(t: &Tally, pushes_len: usize, composition: &BTreeMap<String, Push>) -> String {
    if t.red_pushes == 0 {
        return format!("{TAG} verdict: QUEUE HELD — {pushes_len} queue landing(s), 0 red-main incidents.\n");
    }

    let mut out = String::new();
    // Per tip (head_sha) rather than per red row: one commit can name more
    // than one required workflow red, and the tip only held if every one of
    // its own rows validated clean before landing.
    let mut held_by_tip: BTreeMap<&str, bool> = BTreeMap::new();
    for i in &t.red_incidents {
        let comp = composition_conclusion(composition, &i.head_sha, i.workflow);
        out += &format!(
            "{TAG}   {} {} ({}): composition {}, main's tip red at {}\n",
            format_instant(i.push_created_at),
            short(&i.head_sha),
            i.workflow,
            comp.unwrap_or("none on record"),
            format_instant(i.run_updated_at)
        );
        let held_here = comp == Some("success");
        held_by_tip.entry(i.head_sha.as_str()).and_modify(|ok| *ok = *ok && held_here).or_insert(held_here);
    }

    let not_held: Vec<&str> = held_by_tip.iter().filter(|(_, ok)| !**ok).map(|(sha, _)| *sha).collect();
    out += &if not_held.is_empty() {
        format!(
            "{TAG} verdict: QUEUE HELD — {} tip(s) went red on the post-merge push run only; \
             adjudicate each against src/redlist.rs (a red not on the list is a defect at its \
             owner, never the queue's).\n",
            held_by_tip.len()
        )
    } else {
        format!(
            "{TAG} verdict: QUEUE DID NOT HOLD — {} of {} tip(s) had a red or missing composition \
             run before landing: {}. Per root CLAUDE.md, a red on main's tip is adjudicated at \
             once — file each of the above.\n",
            not_held.len(),
            held_by_tip.len(),
            not_held.iter().map(|s| short(s)).collect::<Vec<_>>().join(", ")
        )
    };
    out
}

fn render(runs: &[Run], since: i64, now: i64) -> String {
    let pushes = group(runs);
    let mut out = header_line(&pushes, since, now);
    let t = tally(&pushes);
    format_tally(&mut out, pushes.len(), &t, now);
    out += &verdict_eased(t.red_pushes);
    out
}

/// The report once the regime is known (`regime` returned other than
/// [`Regime::Unknown`]): the window split at the queue's start, each part
/// reported and verdicted on its own terms, then the totals in the same shape
/// [`render`] always gave. `Regime::Eased` and a not-yet-used `Regime::Queued`
/// both degenerate correctly — the boundary clamps into the window and one
/// side ends up empty — rather than needing a separate no-split path.
/// `composition` is [`fetch_composition`]'s lookup ([`Regime::Eased`] passes
/// an empty one — nothing in the queue part to cross-check when there is no
/// queue part).
fn render_split(runs: &[Run], since: i64, now: i64, regime: Regime, composition: &BTreeMap<String, Push>) -> String {
    let pushes = group(runs);
    let mut out = header_line(&pushes, since, now);

    let (boundary, regime_line) = match regime {
        Regime::Unknown => {
            unreachable!("dispatch routes Regime::Unknown to render, never render_split")
        }
        Regime::Eased => {
            (now, format!("{TAG} regime: no merge queue required on main — eased law throughout\n"))
        }
        Regime::Queued(since_instant) => (
            since_instant,
            format!(
                "{TAG} regime: merge queue required on main since {} (earliest \
                 gh-readonly-queue/main run on record)\n",
                format_instant(since_instant)
            ),
        ),
    };
    out += &regime_line;
    let boundary = boundary.clamp(since, now);

    let split_at = pushes.partition_point(|p| p.created_at < boundary);
    let (eased_pushes, queued_pushes) = pushes.split_at(split_at);

    out += &format!("{TAG}\n{TAG} -- eased-law part: {} .. {} --\n", format_instant(since), format_instant(boundary));
    let eased_tally = tally(eased_pushes);
    format_tally(&mut out, eased_pushes.len(), &eased_tally, now);
    out += &verdict_eased(eased_tally.red_pushes);

    out += &format!("{TAG}\n{TAG} -- queue-regime part: {} .. {} --\n", format_instant(boundary), format_instant(now));
    let queued_tally = tally(queued_pushes);
    format_tally(&mut out, queued_pushes.len(), &queued_tally, now);
    out += &verdict_queued(&queued_tally, queued_pushes.len(), composition);

    out += &format!("{TAG}\n{TAG} -- totals, both regimes --\n");
    let total_tally = tally(&pushes);
    format_tally(&mut out, pushes.len(), &total_tally, now);

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

    #[test]
    fn a_window_straddling_a_regime_start_splits_into_two_partitions_and_verdicts() {
        let runs = vec![
            // eased part: two clean pushes.
            run("a", "ci", "success", "2026-08-20T00:00:00Z", "2026-08-20T00:05:00Z"),
            run("b", "ci", "success", "2026-08-20T00:20:00Z", "2026-08-20T00:25:00Z"),
            // queue part, starting 2026-08-20T01:00:00Z: one red landing among three,
            // and its composition run validated clean — the ordinary post-merge
            // flake this split exists to tell apart from a real queue failure.
            run("c", "ci", "success", "2026-08-20T01:05:00Z", "2026-08-20T01:10:00Z"),
            run("d", "ci", "failure", "2026-08-20T01:20:00Z", "2026-08-20T01:25:00Z"),
            run("e", "ci", "success", "2026-08-20T01:40:00Z", "2026-08-20T01:45:00Z"),
        ];
        let composition = composition_lookup(&[run("d", "ci", "success", "2026-08-20T01:12:00Z", "2026-08-20T01:15:00Z")]);
        let since = parse_instant("2026-08-20T00:00:00Z");
        let now = parse_instant("2026-08-20T02:00:00Z");
        let boundary = parse_instant("2026-08-20T01:00:00Z");
        let report = render_split(&runs, since, now, Regime::Queued(boundary), &composition);

        assert!(
            report.contains("regime: merge queue required on main since 2026-08-20T01:00:00Z"),
            "{report}"
        );
        assert!(
            report.contains("-- eased-law part: 2026-08-20T00:00:00Z .. 2026-08-20T01:00:00Z --"),
            "{report}"
        );
        assert!(
            report.contains("-- queue-regime part: 2026-08-20T01:00:00Z .. 2026-08-20T02:00:00Z --"),
            "{report}"
        );
        assert!(report.contains("-- totals, both regimes --"), "{report}");

        // eased part: 0 of 2, within threshold — a and b never appear in queue's row.
        assert!(report.contains("red-main incidents: 0 of 2"), "{report}");
        // queue part: 1 of 3, naming the one that went red.
        assert!(report.contains("red-main incidents: 1 of 3"), "{report}");
        // totals: 1 of 5, exactly as an undivided report would have counted it.
        assert!(report.contains("red-main incidents: 1 of 5"), "{report}");

        assert!(report.contains("within threshold"), "{report}");
        // d's composition run validated clean, so the tip run's red does not
        // indict the queue.
        assert!(report.contains("d (ci): composition success, main's tip red at 2026-08-20T01:25:00Z"), "{report}");
        assert!(report.contains("QUEUE HELD — 1 tip(s) went red on the post-merge push run only"), "{report}");
        assert!(!report.contains("QUEUE DID NOT HOLD"), "{report}");
    }

    #[test]
    fn a_queue_part_with_no_incidents_reports_the_queue_held() {
        let runs = vec![
            run("a", "ci", "success", "2026-08-20T01:05:00Z", "2026-08-20T01:10:00Z"),
            run("b", "ci", "success", "2026-08-20T01:20:00Z", "2026-08-20T01:25:00Z"),
        ];
        let since = parse_instant("2026-08-20T01:00:00Z");
        let now = parse_instant("2026-08-20T02:00:00Z");
        let report = render_split(&runs, since, now, Regime::Queued(since), &BTreeMap::new());
        assert!(report.contains("QUEUE HELD — 2 queue landing(s), 0 red-main incidents."), "{report}");
        assert!(!report.contains("QUEUE DID NOT HOLD"), "{report}");
    }

    #[test]
    fn regime_eased_puts_the_whole_window_on_the_eased_side() {
        let runs = vec![run("a", "ci", "failure", "2026-08-20T00:00:00Z", "2026-08-20T00:10:00Z")];
        let since = parse_instant("2026-08-20T00:00:00Z");
        let now = parse_instant("2026-08-20T01:00:00Z");
        let report = render_split(&runs, since, now, Regime::Eased, &BTreeMap::new());
        assert!(report.contains("regime: no merge queue required on main"), "{report}");
        assert!(report.contains("QUEUE HELD — 0 queue landing(s), 0 red-main incidents."), "{report}");
        // One red push is within the eased threshold (it takes more than one).
        assert!(report.contains("within threshold"), "{report}");
    }

    /// One of each composition kind — green, red and missing entirely — a
    /// straight run of the queue-regime verdict this coordinator asked for:
    /// only the non-green tips may be named `QUEUE DID NOT HOLD`. Every
    /// `head_sha` here stays at or under [`short`]'s nine-character width so
    /// the assertions below match what the report actually prints.
    #[test]
    fn composition_status_decides_which_tips_the_queue_is_charged_for() {
        let runs = vec![
            run("held", "ci", "failure", "2026-08-20T01:10:00Z", "2026-08-20T01:12:00Z"),
            run("redcomp", "ci", "failure", "2026-08-20T01:20:00Z", "2026-08-20T01:22:00Z"),
            run("nocomp", "ci", "failure", "2026-08-20T01:30:00Z", "2026-08-20T01:32:00Z"),
        ];
        let composition = composition_lookup(&[
            run("held", "ci", "success", "2026-08-20T01:00:00Z", "2026-08-20T01:05:00Z"),
            run("redcomp", "ci", "failure", "2026-08-20T01:00:00Z", "2026-08-20T01:05:00Z"),
            // "nocomp" never appears: the queue never validated it.
        ]);
        let since = parse_instant("2026-08-20T01:00:00Z");
        let now = parse_instant("2026-08-20T02:00:00Z");
        let report = render_split(&runs, since, now, Regime::Queued(since), &composition);

        assert!(report.contains("held (ci): composition success, main's tip red at 2026-08-20T01:12:00Z"), "{report}");
        assert!(
            report.contains("redcomp (ci): composition failure, main's tip red at 2026-08-20T01:22:00Z"),
            "{report}"
        );
        assert!(
            report.contains("nocomp (ci): composition none on record, main's tip red at 2026-08-20T01:32:00Z"),
            "{report}"
        );

        assert!(
            report.contains(
                "QUEUE DID NOT HOLD — 2 of 3 tip(s) had a red or missing composition run before \
                 landing: nocomp, redcomp."
            ),
            "{report}"
        );
    }

    #[test]
    fn composition_green_for_every_incident_reports_queue_held_despite_a_tip_going_red() {
        let runs = vec![run("a", "ci", "failure", "2026-08-20T01:10:00Z", "2026-08-20T01:12:00Z")];
        let composition = composition_lookup(&[run("a", "ci", "success", "2026-08-20T01:00:00Z", "2026-08-20T01:05:00Z")]);
        let since = parse_instant("2026-08-20T01:00:00Z");
        let now = parse_instant("2026-08-20T02:00:00Z");
        let report = render_split(&runs, since, now, Regime::Queued(since), &composition);
        assert!(report.contains("a (ci): composition success, main's tip red at 2026-08-20T01:12:00Z"), "{report}");
        assert!(
            report.contains(
                "QUEUE HELD — 1 tip(s) went red on the post-merge push run only; adjudicate each \
                 against src/redlist.rs"
            ),
            "{report}"
        );
        assert!(!report.contains("QUEUE DID NOT HOLD"), "{report}");
    }
}
