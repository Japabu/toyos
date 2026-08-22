//! `/bin/logd` — the machine's log, written to a file by a process that can be
//! killed without taking the kernel with it.
//!
//! What this program replaces is the kernel's own file sink,
//! `kernel/src/log_file.rs`: a kernel module that appended the log ring to a
//! FAT volume **from the idle loop**, which is why an idle CPU on this machine
//! could be found four spinlocks deep inside a USB transfer with a userland
//! `println!` behind it. The kernel keeps the record ring and the console;
//! every policy about files — where they go, what they are called, how many
//! there are, what happens when the stick stops answering — is here.
//!
//! # Its whole authority
//!
//! One `SysCap` duplicate carrying `Rights::LOG | Rights::WAIT`, which its
//! manifest row asks for by the name `logread`. With it, it may read every
//! record every CPU wrote and park on the readiness source when there is
//! nothing new. It claims no device, opens no compositor connection and can
//! name no process. Writing files is ambient — a known residual of the
//! capability endowment, and not this program's to close.
//!
//! # What it does not do, and why the port is not here
//!
//! Its design carries a `log` port with two frame kinds, and **neither has a
//! caller on this tree**:
//!
//! - `Register` carries the read ends of a child's stdout and stderr pipes.
//!   Those pipes do not exist yet — until they do, every program's stdio is a
//!   console object minted at spawn, and nothing sends this frame.
//! - `Sync` was the shutdown path asking for durability. It is **struck**: the
//!   asker is `SYS_SHUTDOWN`, which runs in the *kernel*, and a kernel that
//!   opens an IPC connection to a userland server to ask it a question is the
//!   inversion this architecture exists to avoid. `LogCursor::durable` already
//!   travels the other way on a call this program makes every loop, so the
//!   kernel reads a word instead — shutdown and panic are one mechanism now,
//!   not two.
//!
//! So `serves = ["log"]` is not on its manifest row yet, by the same rule that
//! keeps `logread` off `/bin/console`'s: *a right with no caller is a
//! capability handed out for a plan*. The acceptor arrives with the first
//! `Register`.
//!
//! # Durability, which is a contract and not a hope
//!
//! Every batch is written, `fsync`ed and only then published: `LogTail::
//! publish_durable` carries the `at_ns` of the newest record now **on the
//! device**, the kernel clamps it and keeps the maximum in `LOG_DURABLE_NS`,
//! and a panicking kernel waits on that word for its own report to land.
//! Publishing before the sync would make the word a lie in exactly the case it
//! exists for, so the order here is load-bearing: write, sync, publish, never
//! two of the three.
//!
//! `SYS_FSYNC` reaches the device's own cache flush — before it did, it stopped
//! at the page cache, and this program calling the result durable would have
//! been a claim of durability that was not one.
//!
//! **A flush that would block is not a flush that failed**, and since
//! 2026-08-22 this program can tell them apart: `io::ErrorKind::WouldBlock`
//! from `sync_all` is `kernel/src/block.rs`'s `BlockError::BudgetExpired`,
//! which means the kernel declined to *start* the operation on the caller's own
//! clock — nothing was issued, the device is untouched, and the bytes are still
//! in the file waiting for the next batch's flush. Keeping the volume across
//! one loses nothing and publishes nothing; ending it on one was a boot's log
//! thrown away for a stick that answered every transfer. `policy::fate` is the
//! whole decision, and `policy`'s own header is the argument.
//!
//! # The console is the kernel's and stays the kernel's
//!
//! This program does **not** write kernel records to the console. `klogd` does,
//! at the commit, and a second copy from here would double every line on the
//! wire. What it writes to its own console is what only it knows: where the log
//! is going, and when it has stopped going there — the kernel keeping the
//! console and giving up the filesystem, taken literally.

mod policy;
mod store;
mod wall;

use std::time::Instant;

use toyos::endow::{Endowments, SYSCAP_LABEL};
use toyos::log::{LogTail, Record};
use toyos::poller::{Poller, READABLE};
use toyos::syscap::SysCap;
use toyos_wallclock::Civil;

use policy::{fate, Fate, Step, LOG_WRITE_BUDGET};
use store::{Volume, DIR, MAX_LOG_BYTES, ROTATE_FAST_BYTES};
use wall::Wall;

/// Records per `SYS_LOG_READ`.
///
/// Above `MAX_LOG_SHARDS`, which the call refuses below, and large enough that
/// an ordinary boot's burst is a handful of syscalls rather than one per line.
/// A `LogRecord` is a kilobyte, so this is 64 KiB of stack-adjacent buffer held
/// for the life of the process — allocated once, never grown.
const BATCH: usize = 64;


/// How long a park on the log's readiness source waits before looking again.
///
/// The wake is what normally ends the park — `klogd` posts it after each drain
/// batch — so this is the bound on a machine that has posted nothing and not
/// the pacing. It is also what keeps this program's own rotation and retention
/// from being deferred forever on a silent machine.
const IDLE_NANOS: u64 = 100_000_000;

/// The poll's token. One handle is watched, so it identifies the round rather
/// than the source.
const LOG_TOKEN: u64 = 1;

/// One `write_all`, so a line of this program's own reaches the console as one
/// `SYS_WRITE`.
///
/// The console object buffers a holder's line and emits it whole since L5, so
/// this is about the *count* of syscalls rather than about atomicity — the same
/// reason `init`, `soundd` and `netd` each carry one.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let mut line = format!($($arg)*);
        line.push('\n');
        let _ = std::io::stderr().write_all(line.as_bytes());
    }};
}

fn main() {
    let Some(cap) = Endowments::get().take::<SysCap>(SYSCAP_LABEL) else {
        // A logd with no capability cannot read one record, so it says which
        // manifest row is missing rather than running as a process that does
        // nothing.
        say!("logd: this program holds no system capability, so it holds no `logread`");
        std::process::exit(1);
    };

    let rotate_at = if std::env::args().any(|a| a == "--rotate-fast") {
        ROTATE_FAST_BYTES
    } else {
        MAX_LOG_BYTES
    };

    // The wall clock, read once. The kernel reads the RTC once too, so a second
    // reading later in the boot would answer out of the same anchor and tell
    // this program nothing new.
    let (stem, boot_local, zone) = boot_stamp();

    let mut volume = Volume::open(stem, rotate_at, |line| say!("{line}"));
    match &volume {
        // §5.6's half, **in one line**: the kernel says whether it has a
        // console, this program says whether it has a volume and what the name
        // it chose was decided by, and the two lines are the four-way table
        // split between the two things that know. One and not two, because
        // every line a daemon writes on a shared console is a line that can land
        // inside a test's window — the C family removes it by this program's
        // own name now (`tests/common/console.rs`), which is a reason to write
        // few rather than a licence to write any number — and because §5.6 says
        // "once".
        Some(v) => say!("logd: this boot's kernel log is {} ({zone})", v.path()),
        None => say!(
            "logd: no {DIR} on this machine - this boot's kernel log is on the console only \
             ({zone})"
        ),
    }

    let mut tail = LogTail::new();
    let mut buf = vec![Record::EMPTY; BATCH];
    let poller = Poller::new(1);
    // Armed before the first read, in the shape §3.2 requires of every reader:
    // the readiness is an edge, so the window is closed by reading once more
    // after arming rather than by asking the kernel a question about a cursor
    // it does not hold. `min_complete` 0 with no timeout submits the entry and
    // returns — one `wait` per `watch`, which is what the ring's own
    // capacity accounting requires.
    poller.watch(&cap, READABLE, LOG_TOKEN);
    poller.wait(0, 0, |_| {});

    let mut lost = 0u64;
    // When the current run of consecutive retries began, or `None` when the
    // last batch was answered. `policy::fate` bounds the run and not the round.
    let mut retrying_since: Option<Instant> = None;
    loop {
        let batch = match tail.read(&cap, &mut buf) {
            Ok(batch) => batch,
            Err(e) => {
                // The one call this program is built around. A refusal is not
                // survivable by retrying — the buffer and the rights are the
                // same every time — so it says so and stops.
                say!("logd: SYS_LOG_READ refused a {BATCH}-record buffer ({e:?})");
                std::process::exit(1);
            }
        };

        if tail.lost() > lost {
            // One line per hole rather than one per read, and it goes in the
            // file it is a hole in: the next batch carries it.
            say!(
                "logd: {} record(s) were overwritten in a shard before this reader got to them",
                tail.lost() - lost
            );
            lost = tail.lost();
        }

        if batch.is_empty() {
            // **Nothing new, so park on the readiness source rather than spin.**
            // `SYS_LOG_READ` never blocks by design; this is the other half of
            // that design.
            poller.watch(&cap, READABLE, LOG_TOKEN);
            poller.wait(1, IDLE_NANOS, |_| {});
            continue;
        }

        let Some(v) = volume.as_mut() else { continue };

        let newest = batch.last().map_or(0, |r| r.at_ns);
        let began = Instant::now();
        let mut refused: Option<(Step, std::io::ErrorKind, String)> = None;
        for record in batch.iter() {
            let line = format!("{} {record}\n", stamp(boot_local, record.at_ns));
            if let Err(e) = v.write(line.as_bytes()) {
                refused = Some((Step::Append, e.kind(), e.to_string()));
                break;
            }
        }
        if refused.is_none() {
            if let Err(e) = v.sync() {
                refused = Some((Step::Flush, e.kind(), e.to_string()));
            }
        }
        // A volume that answered, and took longer than a log is worth doing it.
        // Checked after the write rather than before, because there is nothing
        // to cancel: `SYS_WRITE` and `SYS_FSYNC` do not come back until the
        // transport's own bound has expired, so the only place to notice is
        // here.
        if refused.is_none() && began.elapsed() > LOG_WRITE_BUDGET {
            refused = Some((
                Step::TooSlow,
                std::io::ErrorKind::Other,
                format!("it took {:?}", began.elapsed()),
            ));
        }

        match refused {
            None => {
                // **After the sync and never before it.** This word is what a
                // panicking kernel waits on, so publishing it for a record that
                // is only in the page cache would lose the report in exactly
                // the case the wait exists for.
                tail.publish_durable(newest);
                retrying_since = None;
                if v.full() {
                    if let Err(e) = v.rotate(|line| say!("{line}")) {
                        say!("logd: {DIR} would not take a continuation ({e}) - {}", v.path());
                        volume = None;
                    }
                }
            }
            Some((step, kind, why)) => {
                // The run of consecutive retries, which is what
                // `LOG_WRITE_BUDGET` bounds. `began` and not `Instant::now()`:
                // the run starts when the first refused round started, so the
                // time this batch spent being refused is inside it.
                let first = retrying_since.is_none();
                let since = *retrying_since.get_or_insert(began);
                match fate(step, kind, since.elapsed()) {
                    // §5.4, in order: stop feeding the volume, say so once, and
                    // keep running. It does not exit and does not queue for a
                    // device that is not answering — "I stop waiting for this
                    // stick and say so" is the whole policy for a device fact.
                    Fate::GiveUp => {
                        say!(
                            "logd: {DIR} has not answered ({}: {why}) - this boot's log is on \
                             the console only from {}",
                            step.as_str(),
                            v.path()
                        );
                        volume = None;
                    }
                    // **Nothing is published**, because nothing is durable: the
                    // bytes are in the file and the next batch's flush covers
                    // them as well as its own, so the kernel's `LOG_DURABLE_NS`
                    // stays a word that is true.
                    //
                    // One line per *run* and not per round: a loaded host
                    // refuses several batches in a row, and a line each is the
                    // feedback loop `LOG_WRITE_BUDGET`'s own doc measures.
                    Fate::Retry => {
                        if first {
                            say!(
                                "logd: {DIR} would not start ({}: {why}) - nothing was lost, so \
                                 {} is still this boot's log and the next batch is a retry",
                                step.as_str(),
                                v.path()
                            );
                        }
                    }
                }
            }
        }
    }
}

/// This boot's file stem, and the local epoch second the machine booted at.
///
/// `None` for the stem is a boot that cannot be placed in time, which takes an
/// `unknown-NN` name — and the two ways to get there are named separately,
/// because "this machine has no clock" and "this machine has a clock whose zone
/// two readings cannot separate" are different facts about the machine.
fn boot_stamp() -> (Option<String>, Option<u64>, String) {
    match wall::local_now() {
        Wall::Local { secs, offset_secs } => {
            let civil = Civil::from_unix_secs(secs);
            let uptime_secs = uptime_nanos() / 1_000_000_000;
            (
                Some(format!("{}", civil.stem())),
                Some(secs.saturating_sub(uptime_secs)),
                format!("{civil} at UTC{:+} recovered from two readings", offset_secs / 3_600),
            )
        }
        Wall::Unknown => (
            None,
            None,
            "undated: this machine will not say what time it is".into(),
        ),
        // Named rather than guessed. The two candidates are the same time of day
        // on different days, so a file named from either is a day wrong half the
        // time; `wall`'s module header is the argument.
        Wall::Ambiguous { east, west } => (
            None,
            None,
            format!(
                "undated: the clock is UTC{:+} or UTC{:+} on these two readings and nothing \
                 separates them",
                east / 3_600,
                west / 3_600
            ),
        ),
    }
}

/// A record's wall-clock stamp: the local second the machine booted at, plus
/// the record's own monotonic offset.
///
/// **The record's `at_ns` stays in the line too** — `LogRecord`'s `Display`
/// renders it, and §3.3 is why: this program writes a *prefix* and the body is
/// byte-identical to what the console and the panel carry. So `/log` holds both
/// clocks, and a line in the file can be matched against the same line on the
/// wire without arithmetic.
fn stamp(boot_local: Option<u64>, at_ns: u64) -> String {
    match boot_local {
        Some(base) => format!("{}", Civil::from_unix_secs(base + at_ns / 1_000_000_000)),
        // An undated boot writes the space the stamp would have taken, so the
        // columns line up and nothing has to be re-parsed to notice that a
        // machine had no clock.
        None => "---------- --------".into(),
    }
}

/// Nanoseconds since boot, on the same monotonic clock every record's `at_ns`
/// is stamped from.
///
/// **Through `toyos-abi` rather than through the SDK, and it is a deviation
/// with a reason.** `toyos::system` is where this belongs and it does not have
/// it; adding it there edits a path dependency of `rust/library/std`, which
/// makes this branch claim the machine-global sysroot and blocks every other
/// worktree until it lands. Root `CLAUDE.md` puts an ABI or SDK change on its
/// own pull request first for exactly that reason, and this chunk is not one.
/// `test-runner` names `toyos-abi` for the same kind of reason, so the shape is
/// not new. It should become `toyos::system::clock_nanos` on the next landing
/// that touches the SDK anyway.
fn uptime_nanos() -> u64 {
    toyos_abi::syscall::clock_nanos()
}
