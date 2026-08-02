//! Serialising the build system's stateful phases across the agents that share
//! this tree.
//!
//! Cargo's own build lock cannot do this job. `ensure_fresh` runs `cargo clean`,
//! which removes the whole `target/`, and cargo's lock lives inside it at
//! `target/<profile>/.cargo-lock` — the clean deletes the file the other
//! process's lock is on. So these files live in `.build-locks/` at the repo
//! root, which nothing in the build system removes: a lock on an inode that can
//! be unlinked and recreated under a waiter is not a lock.
//!
//! Two modes, because two plain `cargo build`s of different packages are
//! cargo's business and serialising those would destroy the parallelism the
//! agents sharing this tree depend on:
//!
//! - **shared** — "I am building against the toolchain and the crate target
//!   directories as they stand". Any number at once.
//! - **exclusive** — "I am replacing shared build state": the rust bootstrap,
//!   the sysroot writes, the `cargo clean`s. One at a time, and never while a
//!   build holds the shared mode.
//!
//! Holder death: `flock` is released by the kernel when the open file
//! description closes, so a builder that is SIGKILLed mid-phase — routine here
//! — strands nothing, which a lock file with a pid in it could not promise.
//! Established on this host (Darwin 25.5.0) rather than assumed, and
//! `killed_holder_releases_the_lock` keeps it that way.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
}

const LOCK_SH: i32 = 1;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

const LOCK_DIR: &str = ".build-locks";

fn lock_path(root: &Path, name: &str) -> PathBuf {
    root.join(LOCK_DIR).join(name)
}

/// A held lock. Releasing it is closing the file.
#[must_use]
pub struct Guard {
    file: fs::File,
    /// Exclusive holders record who they are, and clear it on the way out so a
    /// waiter never names a process that has already finished.
    records_holder: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.records_holder {
            write_note(&mut self.file, "");
        }
    }
}

/// The build lock, held in shared mode for the length of one build.
pub struct Held {
    root: PathBuf,
    what: String,
    /// `None` only for the duration of an [`Held::act_if`] escalation, which is
    /// the whole reason this is an `Option`: the shared lock has to be put down
    /// before the exclusive one can be taken.
    guard: Option<Guard>,
}

/// Take the build lock in shared mode for `what`, and hold it until the
/// returned value is dropped — which must be after the last artifact the build
/// reads back, not merely after the last thing it writes: a clean landing
/// between a `cargo build` and the read of what it built is the same defect.
pub fn shared(root: &Path, what: &str) -> Held {
    Held {
        root: root.to_path_buf(),
        what: what.to_string(),
        guard: Some(acquire(root, LOCK_SH, what)),
    }
}

impl Held {
    /// Ask `decide`, and if it reports work, do that work under the exclusive
    /// lock.
    ///
    /// `decide` runs first under the shared lock this value holds, so a phase
    /// with nothing to do costs no serialisation at all. When it does report
    /// work the shared lock is dropped, the exclusive one taken, and `decide`
    /// asked **again**: whatever it saw a moment ago may have been done by the
    /// process that held the lock in between, and only this second answer is
    /// acted on. Serialising the action alone would still double-clean.
    pub fn act_if<W>(&mut self, phase: &str, decide: impl Fn() -> Option<W>, act: impl FnOnce(W)) {
        if decide().is_none() {
            return;
        }
        self.guard = None;
        {
            let _exclusive = acquire(&self.root, LOCK_EX, phase);
            if let Some(work) = decide() {
                act(work);
            }
        }
        self.guard = Some(acquire(&self.root, LOCK_SH, &self.what));
    }
}

/// Exclusive lock over the shared cargo artifact paths.
///
/// Cargo keys an artifact path on (crate, target, profile) and nothing else, so
/// every config writes and reads one path; this is held across each build→stage
/// pair so the staged copy is of what this build produced. Separate from the
/// build lock proper because every builder needs it and builders hold the build
/// lock in *shared* mode by design.
pub fn artifact(root: &Path) -> Guard {
    let path = lock_path(root, "artifact");
    let file = open_lock_file(&path);
    let start = Instant::now();
    let mut waited = false;
    if !try_lock(&file, LOCK_EX) {
        announce("artifact staging", &describe_holder(&path));
        waited = true;
        take_lock(&file, LOCK_EX, &path);
    }
    if waited {
        eprintln!("[build-lock] artifact staging acquired after {:.1?}", start.elapsed());
    }
    let mut guard = Guard { file, records_holder: true };
    write_note(&mut guard.file, &note_text("artifact staging"));
    guard
}

/// Acquire one mode of the build lock.
///
/// Two files, not one. `flock` has no writer preference — measured on this
/// host, four shared churners kept an exclusive waiter out for the whole 5.5 s
/// they ran — and the exclusive phases are exactly the long, silent ones an
/// agent kills and retries. So an exclusive acquirer holds `intent` while it
/// queues for `state`, which makes later shared acquirers line up behind it
/// instead of overtaking it. `intent` is always taken before `state` and
/// dropped as soon as `state` is held, so nothing ever waits on `intent` while
/// holding `state`.
fn acquire(root: &Path, op: i32, what: &str) -> Guard {
    let intent_path = lock_path(root, "intent");
    let state_path = lock_path(root, "state");
    let intent = open_lock_file(&intent_path);
    let state = open_lock_file(&state_path);
    let label = format!("{}, {what}", if op == LOCK_EX { "exclusive" } else { "shared" });

    let start = Instant::now();
    let mut waited = false;

    if !try_lock(&intent, op) {
        announce(&label, "an exclusive phase is queued ahead of it");
        waited = true;
        take_lock(&intent, op, &intent_path);
    }
    if !try_lock(&state, op) {
        if !waited {
            announce(&label, &describe_holder(&state_path));
            waited = true;
        }
        take_lock(&state, op, &state_path);
    }
    drop(intent);

    if waited {
        eprintln!("[build-lock] acquired ({label}) after {:.1?}", start.elapsed());
    }

    let records_holder = op == LOCK_EX;
    let mut guard = Guard { file: state, records_holder };
    if records_holder {
        write_note(&mut guard.file, &note_text(what));
    }
    guard
}

/// An agent staring at silence kills and retries, which is the pathology this
/// module exists to remove — so a wait says what it is waiting for and, when
/// that can be established, who has it.
fn announce(label: &str, holder: &str) {
    eprintln!("[build-lock] waiting for the build lock ({label}) — {holder}");
}

/// What the last exclusive holder of `path` recorded, if it is still running.
///
/// A killed holder leaves its note behind, so the pid is checked before it is
/// named: telling a waiting agent to go look at a dead pid is worse than
/// telling it nothing.
fn describe_holder(path: &Path) -> String {
    let unknown = "held by other builds in this tree".to_string();
    let Ok(mut file) = fs::File::open(path) else {
        return unknown;
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return unknown;
    }
    let mut parts = text.trim().splitn(3, ' ');
    let (Some(pid), Some(since), Some(what)) = (parts.next(), parts.next(), parts.next()) else {
        return unknown;
    };
    let (Ok(pid), Ok(since)) = (pid.parse::<i32>(), since.parse::<u64>()) else {
        return unknown;
    };
    if !alive(pid) {
        return unknown;
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(since))
        .unwrap_or(0);
    format!("held by pid {pid} ({what}), {secs}s so far")
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 runs the existence and permission checks and delivers
    // nothing.
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied
}

fn note_text(what: &str) -> String {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{} {since} {what}", std::process::id())
}

/// The note is advisory — it names a holder in a waiter's message and nothing
/// reads it to decide anything — so failing to write it must not fail a build.
fn write_note(file: &mut fs::File, text: &str) {
    let _ = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| file.set_len(0))
        .and_then(|_| file.write_all(text.as_bytes()))
        .and_then(|_| file.flush());
}

fn open_lock_file(path: &Path) -> fs::File {
    let dir = path.parent().expect("lock path has a parent");
    fs::create_dir_all(dir).unwrap_or_else(|e| panic!("build lock: create {}: {e}", dir.display()));
    // Never truncating: the file carries the holder note, and `File::create`
    // would wipe a live holder's.
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap_or_else(|e| panic!("build lock: open {}: {e}", path.display()))
}

fn take_lock(file: &fs::File, op: i32, path: &Path) {
    loop {
        // SAFETY: `file` owns the fd for the duration of the call and of the
        // guard the caller builds from it.
        if unsafe { flock(file.as_raw_fd(), op) } == 0 {
            return;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        panic!("build lock: flock on {}: {err}", path.display());
    }
}

fn try_lock(file: &fs::File, op: i32) -> bool {
    loop {
        // SAFETY: as in `take_lock`.
        if unsafe { flock(file.as_raw_fd(), op | LOCK_NB) } == 0 {
            return true;
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return false,
            _ => panic!("build lock: flock: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::Duration;

    // Two processes are the point. `flock` is per open file description, so a
    // single-process test would prove nothing about the thing that actually
    // races in this tree. The child is this same test binary, re-run with one
    // `#[ignore]`d test selected by name and its role in the environment, so an
    // ordinary `cargo test` never runs the child half on its own.
    const ROLE: &str = "TOYOS_BUILDLOCK_TEST_ROLE";
    const ROOT: &str = "TOYOS_BUILDLOCK_TEST_ROOT";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("toyos-buildlock-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn child(root: &Path, role: &str) -> Child {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "buildlock::tests::child_role", "--include-ignored", "--nocapture"])
            .env(ROLE, role)
            .env(ROOT, root)
            .spawn()
            .expect("spawn the competing process")
    }

    fn appeared(path: &Path, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while !path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        path.exists()
    }

    fn touch(path: &Path) {
        fs::write(path, b"").unwrap();
    }

    /// A fresh fd per probe: a successful `try_lock` *holds* what it took, and
    /// polling on one fd would itself be the thing keeping the writer out.
    fn intent_is_taken(root: &Path) -> bool {
        !try_lock(&open_lock_file(&lock_path(root, "intent")), LOCK_SH)
    }

    fn note(root: &Path, line: &str) {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("order.log"))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    #[test]
    #[ignore = "the competing process for the tests below; never runs on its own"]
    fn child_role() {
        let role = std::env::var(ROLE)
            .unwrap_or_else(|_| panic!("child_role ran without {ROLE}; it is not a test"));
        let root = PathBuf::from(std::env::var(ROOT).unwrap());
        match role.as_str() {
            "hold-exclusive" => {
                let mut held = shared(&root, "child");
                held.act_if(
                    "child exclusive phase",
                    || (!root.join("release").exists()).then_some(()),
                    |()| {
                        touch(&root.join("held"));
                        appeared(&root.join("release"), Duration::from_secs(20));
                    },
                );
            }
            "hold-exclusive-forever" => {
                let mut held = shared(&root, "child");
                held.act_if(
                    "child exclusive phase",
                    || Some(()),
                    |()| {
                        touch(&root.join("held"));
                        std::thread::sleep(Duration::from_secs(600));
                    },
                );
            }
            "want-exclusive" => {
                let mut held = shared(&root, "child");
                held.act_if("queued exclusive phase", || Some(()), |()| note(&root, "ex"));
            }
            "want-shared" => {
                let _held = shared(&root, "child");
                note(&root, "sh");
            }
            "clean" | "clean-unlocked" => {
                touch(&root.join("cleaner-ready"));
                assert!(appeared(&root.join("builder-mid"), Duration::from_secs(20)));
                let target = root.join("crate/target");
                if role == "clean" {
                    let mut held = shared(&root, "child");
                    held.act_if(
                        "clean the crate target",
                        || target.exists().then_some(()),
                        |()| fs::remove_dir_all(&target).unwrap(),
                    );
                } else {
                    fs::remove_dir_all(&target).unwrap();
                }
                touch(&root.join("cleaner-done"));
            }
            other => panic!("unknown child role {other}"),
        }
    }

    #[test]
    fn exclusive_excludes_every_other_acquirer() {
        let root = scratch("exclusive");
        let mut kid = child(&root, "hold-exclusive");
        assert!(appeared(&root.join("held"), Duration::from_secs(20)), "child never acquired");

        let state = open_lock_file(&lock_path(&root, "state"));
        assert!(!try_lock(&state, LOCK_SH), "a build got in while an exclusive phase ran");
        assert!(!try_lock(&state, LOCK_EX), "two exclusive phases at once");
        let holder = describe_holder(&lock_path(&root, "state"));
        assert!(
            holder.starts_with(&format!("held by pid {} ", kid.id())),
            "the waiting side cannot name the holder: {holder}"
        );

        touch(&root.join("release"));
        assert!(kid.wait().unwrap().success());
        drop(state);
        let _mine = shared(&root, "parent");
    }

    #[test]
    fn killed_holder_releases_the_lock() {
        let root = scratch("killed");
        let mut kid = child(&root, "hold-exclusive-forever");
        assert!(appeared(&root.join("held"), Duration::from_secs(20)), "child never acquired");

        let state = open_lock_file(&lock_path(&root, "state"));
        assert!(!try_lock(&state, LOCK_EX), "the lock was not actually held");

        kid.kill().unwrap();
        kid.wait().unwrap();

        assert!(try_lock(&state, LOCK_EX), "a SIGKILLed holder stranded the lock");
        // And the note it left behind names a pid that is gone, so nobody is
        // sent to wait on it.
        assert_eq!(
            describe_holder(&lock_path(&root, "state")),
            "held by other builds in this tree"
        );
    }

    #[test]
    fn shared_admits_shared() {
        let root = scratch("shared");
        let _mine = shared(&root, "parent");
        let second = open_lock_file(&lock_path(&root, "state"));
        assert!(try_lock(&second, LOCK_SH), "two builds cannot run at once");
        drop(second);
        let third = open_lock_file(&lock_path(&root, "state"));
        assert!(!try_lock(&third, LOCK_EX), "a clean got in while a build was running");
    }

    /// `flock` alone would let a stream of builds starve the rebuild they are
    /// all waiting for; the `intent` file is what stops that, and this is the
    /// gate on it.
    #[test]
    fn a_queued_exclusive_phase_goes_first() {
        let root = scratch("preference");
        let mine = shared(&root, "parent");

        let mut writer = child(&root, "want-exclusive");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !intent_is_taken(&root) {
            assert!(Instant::now() < deadline, "the exclusive child never queued");
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut reader = child(&root, "want-shared");
        assert!(
            !appeared(&root.join("order.log"), Duration::from_millis(300)),
            "a build overtook a queued exclusive phase"
        );

        drop(mine);
        assert!(writer.wait().unwrap().success());
        assert!(reader.wait().unwrap().success());
        assert_eq!(fs::read_to_string(root.join("order.log")).unwrap(), "ex\nsh\n");
    }

    /// The defect this module exists for, staged so that it is not itself a
    /// race: a clean lands in the middle of a build in the same target
    /// directory, and the build's next write finds the directory gone. Run once
    /// without the lock to show the ENOENT, once with it to show the clean
    /// waiting its turn.
    #[test]
    fn a_clean_cannot_land_inside_a_build() {
        let unlocked = clean_racing_a_build(false);
        assert_eq!(
            unlocked.unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "unlocked, the clean was expected to pull the target dir out from under the build"
        );
        clean_racing_a_build(true)
            .expect("locked, the build's write must not land in a cleaned directory");
    }

    fn clean_racing_a_build(locked: bool) -> io::Result<()> {
        let root = scratch(if locked { "race-locked" } else { "race-unlocked" });
        let target = root.join("crate/target");
        fs::create_dir_all(&target).unwrap();

        let mut kid = child(&root, if locked { "clean" } else { "clean-unlocked" });
        assert!(appeared(&root.join("cleaner-ready"), Duration::from_secs(20)));
        let guard = locked.then(|| shared(&root, "parent build"));

        fs::write(target.join("a.o"), b"a").unwrap();
        touch(&root.join("builder-mid"));
        let cleaned = appeared(&root.join("cleaner-done"), Duration::from_millis(700));
        assert_eq!(cleaned, !locked, "the clean's turn came at the wrong time");

        let outcome = fs::write(target.join("b.o"), b"b");

        drop(guard);
        assert!(kid.wait().unwrap().success());
        // Delayed, never dropped: the clean still happens, after the build.
        assert!(root.join("cleaner-done").exists());
        assert!(!target.exists());
        outcome
    }
}
