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

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::buildlock;

/// This landing's own log, named after this landing and nothing else.
///
/// **The caller does not get to choose the path, and that is the point.** On
/// 2026-08-07 two agents watched a *peer's* `--land` output arrive in their own
/// redirected capture — foreign `Compiling …` lines, foreign test failures —
/// because the scratch directory an agent redirects into is shared between
/// concurrent sessions. One chased two phantom red tests; one lost three
/// landing attempts and ended up piping its stream through `sed` to prefix every
/// line with its own name. A shell redirect cannot fix that, because the two
/// shells agree on the path. A file named for the process that writes it can.
struct LandLog {
    path: PathBuf,
    file: Mutex<fs::File>,
}

impl LandLog {
    fn open(root: &Path) -> Self {
        let dir = root.join("target/landings");
        fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("[land] create {}: {e}", dir.display()));
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let pid = std::process::id();
        // `create_new`, and a suffix if the name is taken: the pid separates two
        // landings on the host and nothing separates two in one process within
        // one second. A name that is unique because collisions are unlikely is
        // the defect this exists to remove, not a smaller version of it.
        for attempt in 1.. {
            let name = match attempt {
                1 => format!("landing-{stamp}-{pid}.log"),
                n => format!("landing-{stamp}-{pid}-{n}.log"),
            };
            let path = dir.join(name);
            match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Self { path, file: Mutex::new(file) },
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("[land] create {}: {e}", path.display()),
            }
        }
        unreachable!("the loop above returns or panics")
    }

    /// To the terminal and to the file, because an agent that watches silence
    /// kills things and an agent that has to reconstruct what happened reads
    /// the file.
    fn say(&self, line: &str) {
        eprintln!("{line}");
        self.raw(line.as_bytes());
        self.raw(b"\n");
    }

    fn raw(&self, bytes: &[u8]) {
        let mut file = self.file.lock().expect("the landing log's writer panicked");
        let _ = file.write_all(bytes);
        let _ = file.flush();
    }
}

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
    // Refused by name, because the failure otherwise arrives as `No such file or
    // directory` naming the whole command line — which reads as a broken
    // toolchain rather than as a quoting mistake. Two agents were given the
    // quoted form in writing on one day.
    assert!(
        !rest[0].contains(char::is_whitespace),
        "--gate takes a command and its arguments unquoted, not one quoted string: \
         `--gate cargo test -- foo`, never `--gate \"cargo test -- foo\"`. \
         Splitting {:?} would mean reimplementing a shell's quoting rules.",
        rest[0]
    );
    rest
}

fn is_default(gate: &[String]) -> bool {
    gate.join(" ") == DEFAULT_GATE.join(" ")
}

/// Steps 1-5. `Err` is a refusal to print and exit on; every one of them leaves
/// main exactly where it was.
fn run(root: &Path, gate: &[String]) -> Result<String, String> {
    let primary = crate::primary_checkout(root);
    // After preflight, which is instantaneous and prints its own refusal: the
    // log is for the part that takes minutes.
    let branch = preflight(root, &primary)?;
    let log = Arc::new(LandLog::open(root));
    log.say(&format!("[land] landing {branch} into main at {}", primary.display()));
    log.say(&format!(
        "[land] this landing's log: {}\n\
         [land] it is named for this process, so do not redirect this command into a path of \
         your own — a scratch directory is shared between concurrent sessions and a peer's \
         output lands in it.",
        log.path.display()
    ));

    // Every outcome goes into the log, refusals included: a refusal is the whole
    // product of a failed landing, and reading it back out of a scrollback that
    // eight other landings' output went through was the thing that could not be
    // done on 2026-08-07.
    let outcome = steps(root, &primary, &branch, gate, &log);
    match &outcome {
        Ok(report) | Err(report) => {
            log.raw(report.as_bytes());
            log.raw(b"\n");
        }
    }
    outcome
}

fn steps(
    root: &Path,
    primary: &Path,
    branch: &str,
    gate: &[String],
    log: &Arc<LandLog>,
) -> Result<String, String> {
    let _lock = buildlock::integration(root);

    let main_before = git(primary, &["rev-parse", "main"])?;
    merge_main(root, branch, log)?;

    let took = run_gate(root, gate, log)?;

    let dirty = git(primary, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        return Err(format!(
            "[land] the gate passed, but {} has uncommitted work in it now and step 4 moves \
             its tree:\n{dirty}\n\
             [land] clean it and re-run `cargo run -- --land`. Nothing on main was touched.",
            primary.display()
        ));
    }
    let main_now = git(primary, &["rev-parse", "main"])?;
    if main_now != main_before {
        return Err(bypassed(primary, &main_before, &main_now));
    }
    git(primary, &["merge", "--ff-only", branch]).map_err(|e| {
        format!(
            "[land] main refused the fast-forward to {branch}, and main has not moved since this \
             branch merged it. That is not a case the protocol has a name for — report it.\n{e}"
        )
    })?;

    let landed = git(primary, &["rev-parse", "--short", "main"])?;
    let before = git(primary, &["rev-parse", "--short", &main_before])?;
    let mut report = format!(
        "[land] landed {branch} on main\n\
         [land]   main  {before} -> {landed}\n\
         [land]   gate  {}, {:.1?}\n\
         [land]   log   {}",
        gate.join(" "),
        took,
        log.path.display(),
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
fn merge_main(root: &Path, branch: &str, log: &LandLog) -> Result<(), String> {
    match git(root, &["merge", "--no-ff", "--no-edit", "main"]) {
        Ok(out) => {
            for line in out.lines() {
                log.say(&format!("[land] {line}"));
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
fn run_gate(
    root: &Path,
    gate: &[String],
    log: &Arc<LandLog>,
) -> Result<std::time::Duration, String> {
    log.say(&format!("[land] gate: {}", gate.join(" ")));
    if !is_default(gate) {
        log.say(&format!(
            "[land] that is NOT the default `{}` — the whole suite. Everything the default \
             covers and this does not will be ungated on main.",
            DEFAULT_GATE.join(" ")
        ));
    }
    let started = Instant::now();
    let mut child = Command::new(&gate[0])
        .args(&gate[1..])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("[land] cannot run the gate {:?}: {e}", gate[0]))?;
    // Piped rather than inherited, and copied straight back out: the terminal
    // sees the gate live, as it must, and the log gets the same bytes without
    // the agent having to choose a path that another session is also writing.
    //
    // **Detached, and waited for with a deadline rather than joined.** A pipe
    // reads EOF when the last writer closes it, and the gate boots guests — one
    // stray QEMU inheriting the fd would hold the read open after the gate
    // itself had exited, and a `thread::scope` would then hang the landing on
    // its own log. The log is advisory; the landing is the product.
    let out = child.stdout.take().expect("the gate's stdout was piped");
    let err = child.stderr.take().expect("the gate's stderr was piped");
    let (done, drained) = std::sync::mpsc::channel::<()>();
    {
        let (log, done) = (Arc::clone(log), done.clone());
        std::thread::spawn(move || {
            tee(out, &log, false);
            let _ = done.send(());
        });
    }
    {
        let (log, done) = (Arc::clone(log), done.clone());
        std::thread::spawn(move || {
            tee(err, &log, true);
            let _ = done.send(());
        });
    }
    drop(done);
    let status = child
        .wait()
        .map_err(|e| format!("[land] the gate {:?} could not be waited on: {e}", gate[0]))?;
    for _ in 0..2 {
        if drained.recv_timeout(std::time::Duration::from_secs(5)).is_err() {
            break;
        }
    }
    let took = started.elapsed();
    // 2 is the suite's "this run established nothing" — the host was suspended
    // in the middle of it (`tests/toyos.rs`, and cargo propagates a test
    // binary's own code, measured). It is not a red, and telling an agent to
    // "fix it here" would send it hunting a defect that is not in the tree.
    if status.code() == Some(2) {
        return Err(format!(
            "[land] the gate did not finish a measurement after {took:.1?} — the host was \
             suspended while it ran, so its verdicts are of nothing. main was not touched \
             and nothing is wrong with this branch; re-run `cargo run -- --land`."
        ));
    }
    if !status.success() {
        return Err(format!(
            "[land] the gate failed ({status}) after {took:.1?}. main was not touched; the merge \
             of main into this branch stands, so fix it here and re-run `cargo run -- --land`."
        ));
    }
    Ok(took)
}

/// Copy `from` to this process's own stream and to the log, in chunks rather
/// than lines: `cargo test` writes progress without a newline, and a line
/// reader would hold it back until the next one arrived.
fn tee(mut from: impl Read, log: &LandLog, is_stderr: bool) {
    let mut buf = [0u8; 8192];
    while let Ok(n) = from.read(&mut buf) {
        if n == 0 {
            return;
        }
        let chunk = &buf[..n];
        if is_stderr {
            let mut out = io::stderr();
            let _ = out.write_all(chunk);
            let _ = out.flush();
        } else {
            let mut out = io::stdout();
            let _ = out.write_all(chunk);
            let _ = out.flush();
        }
        log.raw(chunk);
    }
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
        // As the real repository does, and for the reason `--land` now needs it
        // to: the landing writes its own log under `target/`, and an untracked
        // one there would make every one of these tests the dirty-worktree
        // refusal instead of what it is about.
        fs::write(primary.join(".gitignore"), "target/\n").unwrap();
        sh(&primary, &["add", "f", ".gitignore"]);
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

    /// The whole of the shared-scratchpad defect: two landings must not be able
    /// to write one file, and neither may be told where to write by its caller.
    ///
    /// Both halves matter. A unique name that did not carry the gate's output
    /// would leave the agent redirecting anyway, which is the thing that
    /// collided; a captured gate under a caller-chosen name collides just the
    /// same.
    #[test]
    fn each_landing_writes_its_own_log_and_the_gate_is_in_it() {
        let (_primary, wt) = repo("log");
        commit(&wt, "g", "mine\n", "work");
        // Assembled by the gate rather than quoted in its argv, so the `[land]
        // gate: …` header cannot satisfy the assertion the gate's own output is
        // supposed to.
        let shout =
            vec!["sh".to_string(), "-c".to_string(), r"printf 'gate-said-%s\n' hello".to_string()];

        let report = run(&wt, &shout).expect("the landing should have gone through");
        let named = report
            .lines()
            .find_map(|l| l.trim().strip_prefix("[land]   log   "))
            .expect("the report does not name the log");
        let text = fs::read_to_string(named).expect("the named log is not there");
        assert!(text.contains("gate-said-hello"), "the gate's own output is not in the log:\n{text}");
        assert!(text.contains("landed wt on main"), "the outcome is not in the log:\n{text}");

        commit(&wt, "g", "more\n", "again");
        let second = run(&wt, &shout).expect("the second landing should have gone through");
        let also = second
            .lines()
            .find_map(|l| l.trim().strip_prefix("[land]   log   "))
            .expect("the second report does not name the log");
        assert_ne!(named, also, "two landings wrote one file");
    }

    /// A refusal is the whole product of a failed landing, and reading one back
    /// out of a scrollback that eight other landings went through is what could
    /// not be done.
    #[test]
    fn a_refused_landing_leaves_its_refusal_in_the_log() {
        let (_primary, wt) = repo("log-refusal");
        commit(&wt, "g", "mine\n", "work");

        let refusal = run(&wt, &["false".to_string()]).expect_err("a red gate must not land");
        let logs: Vec<PathBuf> = fs::read_dir(wt.join("target/landings"))
            .expect("no landings directory")
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(logs.len(), 1, "{logs:?}");
        let text = fs::read_to_string(&logs[0]).unwrap();
        assert!(text.contains("the gate failed"), "{text}");
        assert!(refusal.contains("the gate failed"), "{refusal}");
    }

    /// The unquoted form is the whole contract of `--gate`, and the quoted one
    /// used to fail as `No such file or directory` naming the entire command.
    #[test]
    fn a_quoted_gate_is_refused_by_name() {
        let unquoted = ["--land", "--gate", "cargo", "test", "--", "foo"];
        assert_eq!(
            parse_gate(&unquoted.map(ToString::to_string)),
            ["cargo", "test", "--", "foo"],
        );
        let quoted = ["--land".to_string(), "--gate".to_string(), "cargo test -- foo".to_string()];
        let refusal = std::panic::catch_unwind(|| parse_gate(&quoted))
            .expect_err("one quoted string is not a command");
        let refusal = refusal.downcast_ref::<String>().expect("an assert's message");
        assert!(refusal.contains("unquoted"), "{refusal}");
    }
}
