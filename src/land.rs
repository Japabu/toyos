//! `cargo run -- --land` — the landing protocol of `specs/worktrees.md` §5, as
//! one command.
//!
//! Step 1 of that protocol is "take the integration lock", and it had no
//! tooling: macOS ships no `flock` CLI, so nothing outside this build system can
//! take a lock at all, and every landing before this one improvised one. What
//! the improvisations cost is in §5 — a landing whose gate ran while three
//! commits went onto main behind it, and a `--ff-only` doing the work of a lock
//! nobody was holding.
//!
//! Conflicts are left in the working tree rather than aborted. The index git has
//! already built and the markers it has already written are exactly what the
//! agent resolves against; aborting deletes them and the agent has to recreate
//! the same state by hand to get back to where it was. The lock goes down first
//! — a half-finished merge is local to one worktree and holds nothing back — and
//! the next `--land` finds `MERGE_HEAD` and says so instead of merging over it.
//!
//! Nothing here rewrites history and nothing pushes.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::buildlock;

/// The whole suite. A landing that runs less than this says so, twice: once
/// before the gate runs and once in the report, because the second is what an
/// agent pastes into its own summary.
const DEFAULT_GATE: [&str; 2] = ["cargo", "test"];

pub fn dispatch(root: &Path, args: &[String]) {
    let gate = parse_gate(args);
    match run(root, &gate) {
        Ok(report) => println!("{report}"),
        Err(refusal) => {
            eprintln!("{refusal}");
            std::process::exit(1);
        }
    }
}

/// `--gate <program> [args...]` takes the whole rest of the command line, so it
/// comes last.
///
/// One quoted string would have to be split, and splitting it means
/// reimplementing a shell's quoting rules or handing the string to a shell;
/// argv is already the thing being asked for.
fn parse_gate(args: &[String]) -> Vec<String> {
    let Some(pos) = args.iter().position(|a| a == "--gate") else {
        return DEFAULT_GATE.iter().map(ToString::to_string).collect();
    };
    let rest = args[pos + 1..].to_vec();
    assert!(!rest.is_empty(), "--gate needs a command: --land --gate <program> [args...]");
    rest
}

fn is_default(gate: &[String]) -> bool {
    gate.join(" ") == DEFAULT_GATE.join(" ")
}

/// Steps 1-5. `Err` is a refusal to print and exit on; every one of them leaves
/// main exactly where it was.
fn run(root: &Path, gate: &[String]) -> Result<String, String> {
    let primary = crate::primary_checkout(root);
    let branch = preflight(root, &primary)?;
    eprintln!("[land] landing {branch} into main at {}", primary.display());

    let _lock = buildlock::integration(root);

    let main_before = git(&primary, &["rev-parse", "main"])?;
    merge_main(root, &branch)?;

    let took = run_gate(root, gate)?;

    let dirty = git(&primary, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(format!(
            "[land] the gate passed, but {} has uncommitted work in it now and step 4 moves \
             its tree:\n{dirty}\n\
             [land] clean it and re-run `cargo run -- --land`. Nothing on main was touched.",
            primary.display()
        ));
    }
    let main_now = git(&primary, &["rev-parse", "main"])?;
    if main_now != main_before {
        return Err(bypassed(&primary, &main_before, &main_now));
    }
    git(&primary, &["merge", "--ff-only", &branch]).map_err(|e| {
        format!(
            "[land] main refused the fast-forward to {branch}, and main has not moved since this \
             branch merged it. That is not a case the protocol has a name for — report it.\n{e}"
        )
    })?;

    let landed = git(&primary, &["rev-parse", "--short", "main"])?;
    let before = git(&primary, &["rev-parse", "--short", &main_before])?;
    let mut report = format!(
        "[land] landed {branch} on main\n\
         [land]   main  {before} -> {landed}\n\
         [land]   gate  {}, {:.1?}",
        gate.join(" "),
        took
    );
    if !is_default(gate) {
        report.push_str(&format!(
            "\n[land]   the gate was NOT the default `{}`; whatever the default covers and \
             this did not is ungated on main now",
            DEFAULT_GATE.join(" ")
        ));
    }
    Ok(report)
}

/// Everything that would make the landing wrong, asked before the lock is taken
/// so a queue of landings does not form behind one that was never going to work.
///
/// The branch's own tree has to be clean for a reason the primary's does not:
/// the gate runs against the working tree and main gets the commits, so
/// uncommitted work would be gated and then not landed.
fn preflight(root: &Path, primary: &Path) -> Result<String, String> {
    if canonical(root) == canonical(primary) {
        return Err(format!(
            "[land] {} is the primary checkout. Landings arrive there; they are made in a \
             worktree.\n[land] `cargo run -- --worktree add <path>` makes one.",
            primary.display()
        ));
    }
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "main" {
        return Err("[land] this worktree is on main, so there is nothing to land.".to_string());
    }
    if branch == "HEAD" {
        return Err("[land] this worktree is on a detached HEAD; --land merges a branch."
            .to_string());
    }
    if merging(root) {
        return Err(format!(
            "[land] a merge of main into {branch} is still unresolved here.\n\
             [land] resolve it, `git add` the files, `git commit`, then re-run \
             `cargo run -- --land`."
        ));
    }
    let dirty = git(root, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(format!(
            "[land] this worktree has uncommitted work, and the gate would measure a tree main \
             is not going to get:\n{dirty}\n\
             [land] commit it — on your own branch that is free — then re-run \
             `cargo run -- --land`."
        ));
    }
    let on = git(primary, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if on != "main" {
        return Err(format!(
            "[land] {} is on {on}, not main. Step 4 fast-forwards whatever is checked out there.",
            primary.display()
        ));
    }
    let dirty = git(primary, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(format!(
            "[land] {} has uncommitted work in it and step 4 moves its tree:\n{dirty}\n\
             [land] the primary checkout is not a workspace (specs/worktrees.md §5).",
            primary.display()
        ));
    }
    Ok(branch)
}

/// Step 2. `--no-ff` so the landing record names both parents; `--no-edit`
/// because nothing here has a terminal to open an editor on.
fn merge_main(root: &Path, branch: &str) -> Result<(), String> {
    match git(root, &["merge", "--no-ff", "--no-edit", "main"]) {
        Ok(out) => {
            for line in out.lines() {
                eprintln!("[land] {line}");
            }
            Ok(())
        }
        Err(e) if merging(root) => Err(format!(
            "{e}\n\
             [land] merging main into {branch} conflicts. The merge is left in this worktree, \
             not aborted: its index and its markers are what you resolve against.\n\
             [land] resolve, `git add`, `git commit`, then re-run `cargo run -- --land` — it \
             will merge again (a no-op) and run the gate on the result.\n\
             [land] the integration lock is released and main was not touched."
        )),
        Err(e) => Err(format!("[land] `git merge --no-ff main` failed:\n{e}")),
    }
}

/// Step 3, with the gate's own output going straight to the terminal: it is the
/// long part, and an agent watching silence kills things.
fn run_gate(root: &Path, gate: &[String]) -> Result<std::time::Duration, String> {
    eprintln!("[land] gate: {}", gate.join(" "));
    if !is_default(gate) {
        eprintln!(
            "[land] that is NOT the default `{}` — the whole suite. Everything the default \
             covers and this does not will be ungated on main.",
            DEFAULT_GATE.join(" ")
        );
    }
    let started = Instant::now();
    let status = Command::new(&gate[0])
        .args(&gate[1..])
        .current_dir(root)
        .status()
        .map_err(|e| format!("[land] cannot run the gate {:?}: {e}", gate[0]))?;
    let took = started.elapsed();
    if !status.success() {
        return Err(format!(
            "[land] the gate failed ({status}) after {took:.1?}. main was not touched; the merge \
             of main into this branch stands, so fix it here and re-run `cargo run -- --land`."
        ));
    }
    Ok(took)
}

/// main moved while this process held the lock, and only `--land` takes it.
fn bypassed(primary: &Path, before: &str, now: &str) -> String {
    let between = git(primary, &["log", "--oneline", &format!("{before}..main")])
        .unwrap_or_else(|e| e);
    format!(
        "[land] LANDING BYPASSED — main moved while this landing held the integration lock.\n\
         [land]   main was {before}\n\
         [land]   main is  {now}\n\
         [land] what arrived in between:\n{between}\n\
         [land] only `--land` takes that lock, so whoever put those on main did not take it. \
         The gate that just passed measured a tree that is no longer what main would become.\n\
         [land] nothing was merged. Re-run `cargo run -- --land`: it merges the new main and \
         runs the gate again, which is what specs/worktrees.md §5 prescribes for exactly this."
    )
}

fn merging(root: &Path) -> bool {
    git(root, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).is_ok()
}

fn canonical(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|e| panic!("canonicalise {}: {e}", path.display()))
}

/// `Err` carries what git printed, both streams, because a refusal that hides
/// git's own message makes the agent run the command again by hand to see it.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("run git in {}: {e}", dir.display()));
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
    Err(format!("[land] git {} (in {})\n{stdout}\n{stderr}", args.join(" "), dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A primary checkout on `main` with one linked worktree on `wt`, which is
    /// the only shape `--land` runs in. Signing off and an identity on the
    /// repository itself: the host's global config signs every commit, and a
    /// test that waited on gpg would be a test that hangs.
    fn repo(name: &str) -> (PathBuf, PathBuf) {
        let primary = std::env::temp_dir().join(format!("toyos-land-{name}"));
        // Beside the primary, never inside it: a linked worktree under the
        // primary's root is an untracked directory in its `git status`, and
        // every one of these tests would then be testing the dirty-primary
        // refusal.
        let wt = std::env::temp_dir().join(format!("toyos-land-{name}-wt"));
        let _ = fs::remove_dir_all(&primary);
        let _ = fs::remove_dir_all(&wt);
        fs::create_dir_all(&primary).unwrap();
        sh(&primary, &["init", "-q", "-b", "main"]);
        sh(&primary, &["config", "user.email", "t@t"]);
        sh(&primary, &["config", "user.name", "t"]);
        sh(&primary, &["config", "commit.gpgsign", "false"]);
        fs::write(primary.join("f"), "base\n").unwrap();
        sh(&primary, &["add", "f"]);
        sh(&primary, &["commit", "-qm", "base"]);

        sh(&primary, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt", "main"]);
        (primary, wt)
    }

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git")
            .success();
        assert!(ok, "git {args:?} in {}", dir.display());
    }

    fn commit(dir: &Path, file: &str, text: &str, msg: &str) {
        fs::write(dir.join(file), text).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-qm", msg]);
    }

    fn head(dir: &Path, rev: &str) -> String {
        git(dir, &["rev-parse", rev]).unwrap()
    }

    /// `true(1)` as the gate: what step 3 runs is the argument, and the suite
    /// itself is not what these tests are about.
    fn pass() -> Vec<String> {
        vec!["true".to_string()]
    }

    #[test]
    fn a_landing_fast_forwards_main_to_the_branch() {
        let (primary, wt) = repo("happy");
        commit(&wt, "g", "mine\n", "work");
        let tip = head(&wt, "HEAD");

        let report = run(&wt, &pass()).expect("the landing should have gone through");
        assert!(report.contains("landed wt on main"), "{report}");
        assert_eq!(head(&primary, "main"), tip, "main is not at the branch tip");
        assert!(buildlock::integration_is_free(&primary), "the lock was left held");
    }

    /// Step 2 first: main's commits have to be *in* the branch before the branch
    /// goes onto main, or the fast-forward is not available.
    #[test]
    fn main_is_merged_into_the_branch_before_the_branch_lands() {
        let (primary, wt) = repo("merge-first");
        commit(&wt, "g", "mine\n", "work");
        commit(&primary, "h", "theirs\n", "meanwhile");
        let theirs = head(&primary, "main");

        run(&wt, &pass()).expect("the landing should have gone through");
        assert_eq!(head(&primary, "main"), head(&wt, "HEAD"));
        assert!(fs::read_to_string(primary.join("g")).is_ok(), "the branch's file is missing");
        let parents = git(&primary, &["rev-list", "--parents", "-n", "1", "main"]).unwrap();
        assert!(parents.contains(&theirs), "the landing commit does not name main's tip: {parents}");
    }

    /// The incident §5 records, staged: main moves during the gate. Only `--land`
    /// takes the integration lock, so the gate *is* where a bypass can happen —
    /// which makes a gate that commits on main the honest way to stage one.
    #[test]
    fn main_moving_during_the_gate_is_reported_as_a_bypass() {
        let (primary, wt) = repo("bypass");
        commit(&wt, "g", "mine\n", "work");
        let sneak = vec![
            "git".to_string(),
            "-C".to_string(),
            primary.to_string_lossy().to_string(),
            "commit".to_string(),
            "--allow-empty".to_string(),
            "-qm".to_string(),
            "landed without the lock".to_string(),
        ];
        let before = head(&primary, "main");

        let refusal = run(&wt, &sneak).expect_err("a bypassed landing must not merge");
        assert!(refusal.contains("LANDING BYPASSED"), "{refusal}");
        assert!(refusal.contains("landed without the lock"), "{refusal}");
        assert_ne!(head(&primary, "main"), before, "the staged bypass did not happen");
        assert_ne!(head(&primary, "main"), head(&wt, "HEAD"), "the branch landed anyway");
        assert!(buildlock::integration_is_free(&primary), "the lock was left held");
    }

    #[test]
    fn a_failing_gate_lands_nothing() {
        let (primary, wt) = repo("red-gate");
        commit(&wt, "g", "mine\n", "work");
        let before = head(&primary, "main");

        let refusal = run(&wt, &["false".to_string()]).expect_err("a red gate must not land");
        assert!(refusal.contains("the gate failed"), "{refusal}");
        assert_eq!(head(&primary, "main"), before);
        assert!(buildlock::integration_is_free(&primary), "the lock was left held");
    }

    /// The merge is left where the agent can resolve it, and the *next* `--land`
    /// finds it rather than merging over it. Both halves, because leaving it
    /// behind is only safe if something notices.
    #[test]
    fn a_conflict_is_left_in_the_worktree_and_recognised_next_time() {
        let (primary, wt) = repo("conflict");
        commit(&wt, "f", "mine\n", "work");
        commit(&primary, "f", "theirs\n", "meanwhile");
        let before = head(&primary, "main");

        let refusal = run(&wt, &pass()).expect_err("a conflicted merge must not land");
        assert!(refusal.contains("conflicts"), "{refusal}");
        assert!(merging(&wt), "the conflicted merge was thrown away");
        assert!(
            fs::read_to_string(wt.join("f")).unwrap().contains("<<<<<<<"),
            "the markers to resolve against are gone"
        );
        assert_eq!(head(&primary, "main"), before);
        assert!(buildlock::integration_is_free(&primary), "the lock was left held");

        let again = run(&wt, &pass()).expect_err("an unresolved merge must be refused");
        assert!(again.contains("still unresolved"), "{again}");

        sh(&wt, &["checkout", "--ours", "f"]);
        sh(&wt, &["add", "f"]);
        sh(&wt, &["commit", "-qm", "resolved"]);
        run(&wt, &pass()).expect("the documented path after a conflict must work");
        assert_eq!(head(&primary, "main"), head(&wt, "HEAD"));
    }

    #[test]
    fn a_dirty_primary_is_refused_by_name() {
        let (primary, wt) = repo("dirty-primary");
        commit(&wt, "g", "mine\n", "work");
        fs::write(primary.join("f"), "someone is working here\n").unwrap();

        let refusal = run(&wt, &pass()).expect_err("step 4 moves that tree");
        assert!(refusal.contains(&primary.display().to_string()), "{refusal}");
        assert!(refusal.contains("not a workspace"), "{refusal}");
        assert!(buildlock::integration_is_free(&primary), "the lock was left held");
    }

    /// Uncommitted work in the branch is the quiet one: the gate would pass on a
    /// tree containing it and main would get a tree without it.
    #[test]
    fn a_dirty_worktree_is_refused_before_the_gate_runs() {
        let (primary, wt) = repo("dirty-worktree");
        commit(&wt, "g", "mine\n", "work");
        fs::write(wt.join("g"), "not committed\n").unwrap();
        let before = head(&primary, "main");

        let refusal = run(&wt, &["false".to_string()]).expect_err("uncommitted work must refuse");
        assert!(refusal.contains("uncommitted work"), "{refusal}");
        assert_eq!(head(&primary, "main"), before);
    }

    #[test]
    fn the_primary_cannot_land_itself() {
        let (primary, _wt) = repo("primary");
        let refusal = run(&primary, &pass()).expect_err("the primary is where landings arrive");
        assert!(refusal.contains("is the primary checkout"), "{refusal}");
    }

    #[test]
    fn the_gate_defaults_to_the_whole_suite_and_an_override_says_so() {
        assert_eq!(parse_gate(&["toyos-build".to_string(), "--land".to_string()]), DEFAULT_GATE);

        let (primary, wt) = repo("override");
        commit(&wt, "g", "mine\n", "work");
        let report = run(&wt, &pass()).unwrap();
        assert!(report.contains("NOT the default"), "an override went unreported: {report}");
        assert!(buildlock::integration_is_free(&primary), "the lock was left held");
    }
}
