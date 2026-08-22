//! The kernel's only log producer.
//!
//! `log!`, `alert!` and `boot_phase!` all expand to [`emit`], which takes
//! `fmt::Arguments` and nothing else. **There is no byte-oriented entry point**,
//! and that is what makes "half a record" untypeable: the smallest thing this
//! module accepts is a whole one.

// Every `unsafe` block under `log::` has either stopped existing or carries a
// `SAFETY:` saying why it could not — the reduction-before-documentation sweep
// `issues/build/clippy-has-never-run-here.md` records. `host-tests.yml`'s two
// kernel clippy invocations both run with `-D warnings`, so `warn` here is what
// gates: a new undocumented block anywhere in this module tree fails CI.
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod console;
pub mod nested;
pub mod read;
pub mod registry;
pub mod shard;
#[cfg(feature = "boot-actuators")]
pub mod storm;
pub mod user;

use core::sync::atomic::{AtomicBool, Ordering};

use toyos_abi::log::{LogRecord, FLAG_EARLY, MAX_LOG_SHARDS, MAX_RECORD_MESSAGE};
use crate::time::{Budget, Duration};

pub use shard::Shard;
pub use toyos_abi::log::Level;

/// Set to true by `percpu::init_bsp` once GS base is valid.
/// Before this, reading `gs:` would fault on a garbage GS base.
pub static PERCPU_READY: AtomicBool = AtomicBool::new(false);

/// cpu0's shard, and the boot shard, because they are the same thing.
///
/// A `static` rather than a heap allocation because `log!` runs before the heap
/// exists and before `PERCPU_READY`. **There is no boot-shard-to-cpu0-shard
/// handoff to get wrong**, and today's `boot` label in the prefix becomes
/// [`FLAG_EARLY`] on a record in this same shard.
///
/// Zeroed, so it costs `.bss` and not 512 KiB of kernel image.
pub static BOOT_SHARD: Shard = Shard::new();

/// The ABI fixes how many shards a cursor can name, so the kernel is what must
/// agree with it rather than the other way round.
const _: () = assert!(crate::sched::MAX_CPUS <= MAX_LOG_SHARDS);

/// Make an AP's shard reachable to a reader. `registry` is the mechanism and
/// carries the argument; this is the kernel's registry bound to it.
///
/// # Safety
/// `shard` must be a live, initialised [`Shard`] that is never freed.
pub unsafe fn publish_ap_shard(cpu: u32, shard: *mut Shard) {
    // SAFETY: the caller's contract is this one.
    unsafe { registry::publish(registry::kernel_slots(), cpu, shard) };
}

/// How long `SYS_SHUTDOWN` gives `/bin/logd` to make the last records durable.
///
/// **Not `apic`'s `LOG_FILE_DRAIN`, and the difference is what each one
/// bounds.** That one is a *panicking* machine, where the scheduler may be
/// unable to pick logd at all and erring long costs the panel on a machine that
/// is already lost. This is an orderly shutdown: every thread is healthy, the
/// caller yields rather than spins, and what it waits for is one wake, one
/// `SYS_LOG_READ`, a page-cache write-back, a FAT append and a device cache
/// flush on the log volume — tens of milliseconds on an ordinary stick and
/// hundreds under `--slow-usb`. Two seconds is the same order as the
/// transport's own `USB_TIMEOUT_NS` for one transfer, which is what puts a floor
/// under how slow "answering, slowly" is allowed to look.
const SHUTDOWN_DURABLE: Budget = Budget::of(
    Duration::from_secs(2),
    "the shutdown's last lines are on the console only, and it says so",
);

/// Wait, bounded, for `/bin/logd` to put everything committed so far on the
/// device.
///
/// One caller, `SYS_SHUTDOWN` (§6.3), and it is why §5.2's `Sync` frame is
/// struck: the asker is the kernel, and a kernel opening an IPC connection to a
/// userland server to ask it a question is the inversion this architecture
/// exists to remove. `LogCursor::durable` already travels the other way on a
/// call logd makes every loop, so the answer is a word rather than a protocol —
/// and the panic path's wait and this one become one mechanism seen from two
/// contexts, instead of two mechanisms that have to be kept agreeing.
///
/// **It yields and does not spin.** This runs on an ordinary thread with the
/// VFS lock already released, and at `--smp 1` the CPU it is on is the only one
/// logd can run on: a spin would make the bound expire on every single-CPU
/// shutdown, which is the width most of the suite boots at.
pub fn wait_for_durable() {
    let want = read::newest_committed_at_ns();
    let deadline = crate::clock::nanos_since_boot().saturating_add(SHUTDOWN_DURABLE.nanos());
    while user::durable_ns() < want {
        if crate::clock::nanos_since_boot() >= deadline {
            crate::log!(
                "shutdown: /log did not answer in {}ms, so this shutdown's last lines are on the \
                 console only",
                SHUTDOWN_DURABLE.duration().millis()
            );
            return;
        }
        crate::scheduler::yield_now();
    }
}

/// Every shard a reader can reach, cpu0 first. `None` is a CPU this machine
/// does not have.
pub fn shards() -> [Option<&'static Shard>; MAX_LOG_SHARDS] {
    let mut out = [None; MAX_LOG_SHARDS];
    out[0] = Some(&BOOT_SHARD);
    for (ap, slot) in out[1..].iter_mut().enumerate() {
        *slot = registry::published(registry::kernel_slots(), ap);
    }
    out
}

/// How many shards a reader can be answered from right now.
///
/// **Counted rather than read off `MAX_CPUS`**, because a shard exists when the
/// BSP has published it and not when a CPU is declared: a reader that sized its
/// buffer by the declaration would be asking for room for shards this machine
/// has not brought up. It only ever grows, and never past [`MAX_LOG_SHARDS`].
pub fn shard_count() -> u32 {
    shards().iter().filter(|shard| shard.is_some()).count() as u32
}

/// The formatter `emit` runs, writing the message into the record in place.
///
/// **One pass and one sink.** It was a `Tee` until L3 — the record's bounded
/// message *and* a rendered line into the byte ring — because L1 and L2 had to
/// leave the wire byte-identical while the record ring grew up beside it. The
/// byte ring is gone, and the line the console carries is rendered from the
/// record by `log::console`, through the one formatter in `toyos-abi`.
struct Message<'a> {
    /// The record's own message bytes, written in place. **Borrowed rather than
    /// owned**: a second buffer here and a copy out of it put two message-sized
    /// arrays on `emit`'s frame, and `emit` runs on the double-fault stack.
    msg: &'a mut [u8; MAX_RECORD_MESSAGE],
    len: usize,
    /// Bytes past the record's bound, saturating. **Counted rather than
    /// dropped** — the difference between a bound and a lie.
    elided: usize,
}

impl core::fmt::Write for Message<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = MAX_RECORD_MESSAGE - self.len;
        let bytes = s.as_bytes();
        if bytes.len() <= room {
            self.msg[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
            return Ok(());
        }
        // Split on a character boundary: a record whose tail is half a UTF-8
        // sequence renders as mojibake for every consumer, and the one that
        // matters paints a panel.
        let mut fit = room;
        while fit > 0 && !s.is_char_boundary(fit) {
            fit -= 1;
        }
        self.msg[self.len..self.len + fit].copy_from_slice(&bytes[..fit]);
        self.len += fit;
        self.elided = self.elided.saturating_add(bytes.len() - fit);
        Ok(())
    }
}

/// Where this record goes and who is writing it.
struct Origin {
    shard: &'static Shard,
    cpu: u16,
    tid: u32,
    pid: u32,
    flags: u8,
}

/// This CPU's shard, its identity, and the sequence number this record owns.
///
/// **The bracket is not optional, and it ends at publication rather than at the
/// reservation.** A non-`lock`-prefixed `xadd` is atomic against an interrupt
/// on its own CPU and not against another CPU, so the design is sound only
/// while the CPU executing it owns the shard — and work stealing is on. The
/// same bracket must cover the body: a writer preempted there can resume after
/// a whole newer generation committed into its slot and overwrite that live
/// record before any final re-check can help.
///
/// **What the bracket buys that a reader can see is the *order*, and that is
/// what is gated.** `emit` stamps `at_ns` on the line above this call, so an
/// interrupt that logs from inside the window takes the lower sequence numbers
/// under the later timestamps and the producer it interrupted lands above them
/// carrying an earlier one — which is what `read.rs`'s `Descent::advance` may
/// not survive. `log-nested-reserve` puts an interrupt there and
/// `log_reserve_window_negative` reads the result with the `cli` removed. The
/// mid-body half is real and is *not* observable from any reader: a lapped
/// writer republishes the previous generation's number, which is exactly what
/// an unpublished slot looks like, and the record it destroyed is already below
/// `Shard::oldest_readable`.
///
/// `preempt::disable` would buy migration exclusion for two locked
/// read-modify-writes per record and would still leave single-step #DB enabled.
/// That is the cost this whole design exists to avoid — one `fetch_add` per line
/// cost 350 ms of boot under TCG — without buying the full property. On the
/// dominant path IF and TF were already clear, because IF is clear for the whole
/// of every syscall and TF is normally clear machine-wide.
fn reserve(guard: &crate::arch::LogCommitGuard) -> (Origin, u64) {
    if !PERCPU_READY.load(Ordering::Relaxed) {
        // One CPU, no scheduler, no GS base to read. An interrupt can still
        // land, and the `xadd` is still what makes that safe.
        //
        // SAFETY: nothing else is running, so this CPU owns the boot shard.
        let seq = unsafe { BOOT_SHARD.reserve(guard) };
        let origin = Origin { shard: &BOOT_SHARD, cpu: 0, tid: 0, pid: 0, flags: FLAG_EARLY };
        return (origin, seq);
    }

    let (shard, seq, cpu, tid, pid) = crate::arch::percpu::reserve_log_slot(guard);
    // SAFETY: `reserve_log_slot` read this pointer out of this CPU's own
    // `PerCpu`, with IF and TF masked through the eventual commit, and
    // `alloc_percpu` gives every CPU a shard before that CPU executes an
    // instruction — so this is a live `Shard` and it is ours.
    let shard: &'static Shard = unsafe { &*shard };
    (Origin { shard, cpu: cpu as u16, tid: on_a_thread(tid), pid: on_a_thread(pid), flags: 0 }, seq)
}

/// `PerCpu`'s "no thread here" is `u32::MAX`; a record's is zero, because that
/// is what the ABI's one formatter renders as absent.
///
/// **Translated at the boundary rather than carried inward**, so no consumer
/// has to know what an idle CPU looks like from inside the kernel — a panel
/// that rendered the raw sentinel would print `tid=4294967295` on every line a
/// kernel thread logged.
///
/// It costs the tid of a process's *first* thread, which is `Tid(0)` and
/// therefore also renders as absent:
/// `issues/diagnostics/a-record-cannot-name-thread-zero.md` is the entry,
/// and its fix is in the ABI's formatter rather than here.
fn on_a_thread(id: u32) -> u32 {
    if id == u32::MAX { 0 } else { id }
}

/// The only producer.
///
/// Steps, in order: format on the stack, prepare the body, then stamp, reserve
/// and publish under one IF/TF-off bracket.
pub fn emit(level: Level, args: core::fmt::Arguments) {
    let mut record = LogRecord { level: level as u8, ..LogRecord::EMPTY };

    // **Formatting is outside every critical section**, which is the one thing
    // it was never inside the old serial writer's. Nothing here takes a lock,
    // touches a device or reads `gs:`: it fills a stack record and counts what
    // did not fit.
    let mut message = Message { msg: &mut record.msg, len: 0, elided: 0 };
    let _ = core::fmt::Write::write_fmt(&mut message, args);
    record.len = message.len as u16;
    record.elided = message.elided.min(u16::MAX as usize) as u16;

    let guard = crate::arch::LogCommitGuard::close();
    // **Stamped inside the bracket, and that is what makes a shard's records
    // ordered by their timestamps at all.**
    //
    // Read outside it, a producer could be interrupted between the clock and
    // the `xadd`: the handler's record then takes the *lower* sequence number
    // and carries the *later* timestamp, so a descent by sequence number is not
    // a descent by `at_ns` and `read.rs`'s reader would stop a shard on a record
    // older than its window with live records still below it. IF and TF are
    // clear across both here, so nothing on this CPU can come between them and
    // the two orders are the same one. The NMI handler does not log (its own
    // gate) and #MC halts rather than returning, which is what closes the two
    // paths a bracket cannot.
    //
    // The cost is one `rdtsc` and one `__udivti3` inside the window, against a
    // 1 KiB publication that is already in it.
    record.at_ns = crate::clock::nanos_since_boot();
    let (origin, seq) = reserve(&guard);
    record.seq = seq;
    record.pid = origin.pid;
    record.tid = origin.tid;
    record.cpu = origin.cpu;
    record.flags = origin.flags;

    // SAFETY: `seq` came from this shard's own `reserve`, on this CPU, and is
    // published exactly once while the same guard keeps that CPU and its trap
    // state unchanged.
    unsafe { origin.shard.commit(seq, &record, &guard) };
    drop(guard);

    // **Who speaks this record, after the publication bracket has closed.**
    // The two modes are phases and not fallbacks (§4.2), and the mode is the
    // one word `console::mode` reads rather than a flag beside it.
    match console::mode() {
        // Nothing else can run yet, so the producer is the drainer. It costs
        // one `BackendGuard` CAS and a synchronous backend write for the boot's
        // ~185 records on one CPU, and it is what makes a boot that wedges
        // before the idle loop say everything it logged.
        console::Drain::Inline => console::drain_inline(),
        // **In a bracket of its own.** The producer's whole contribution to the
        // wake is a `SeqCst` fence and a relaxed load; the five locked
        // operations of the post are paid at most once per `klogd` park, by
        // whichever producer wins the swap. `emit` may take no lock — it runs
        // inside `sync.rs`, inside IRQ handlers, inside the scheduler and
        // inside every syscall's locked region — which is why the post is
        // `wake_direct` and not an ordinary queue wake.
        console::Drain::Thread => {
            if shard::signal_after_commit(shard::log_waiter()) {
                console::post_wake();
            }
        }
    }
}

/// A line of ordinary kernel log. 658 sites.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Info, format_args!($($arg)*))
    };
}

/// A refusal, a corruption, or a fault. **The panel paints the row red**, and
/// it does so because of this rather than because the message happens to
/// contain three exclamation marks.
#[macro_export]
macro_rules! alert {
    ($($arg:tt)*) => {
        $crate::log::emit($crate::log::Level::Alert, format_args!($($arg)*))
    };
}

/// Announce a boot phase boundary: log how long it took, and repaint the
/// on-screen console so the last completed phase stays visible.
///
/// The two belong together. A machine that wedges without panicking calls
/// nothing, so the only thing that can distinguish "hung in xHCI" from "black
/// screen, no idea" is a checkpoint painted before it hung — and a checkpoint
/// nobody can see is not a checkpoint.
///
/// `$since` is the phase's start timestamp; pass 0 to measure from boot.
#[macro_export]
macro_rules! boot_phase {
    ($name:literal, $since:expr) => {{
        $crate::log::emit(
            $crate::log::Level::Phase,
            format_args!(
                "Boot: {} ({}ms)",
                $name,
                ($crate::clock::nanos_since_boot() - $since) / 1_000_000
            ),
        );
        $crate::drivers::panic_console::boot_checkpoint();
    }};
}
