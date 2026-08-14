//! The kernel's only log producer.
//!
//! `log!`, `alert!` and `boot_phase!` all expand to [`emit`], which takes
//! `fmt::Arguments` and nothing else. **There is no byte-oriented entry point**,
//! and that is what makes "half a record" untypeable: the smallest thing this
//! module accepts is a whole one.
//!
//! `specs/log-architecture-spec.md` §2.

pub mod console;
pub mod elide;
pub mod read;
pub mod registry;
pub mod shard;

use core::sync::atomic::{AtomicBool, Ordering};

use toyos_abi::log::{LogRecord, FLAG_EARLY, MAX_LOG_SHARDS, MAX_RECORD_MESSAGE};

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

/// One format pass, two sinks: the record's bounded message and — until L3
/// deletes it — the byte ring.
///
/// **The byte ring gets every byte and the record gets the first
/// [`MAX_RECORD_MESSAGE`].** They have to differ, and finding out why is what
/// L1's "nothing observable changes" claim was for: `screen_late_panic`'s
/// stimulus is `late_panic::Nest`, a symbol demangled deliberately wider than
/// any console grid, and it is far past the record bound. Rendering the byte
/// ring *from the truncated record* dropped its tail and reddened the gate that
/// exists to prove the panel wraps rather than clips. Feeding both from one
/// pass costs nothing extra — the formatter runs once either way — and keeps
/// this chunk byte-identical on the wire.
///
/// **The record bound is still too small and that is not fixed here**;
/// `specs/issues/diagnostics/a-record-cannot-hold-a-demangled-frame.md` is the
/// entry, and it has to be answered before L2 makes records what the panel
/// renders.
struct Tee<'a> {
    /// The record's own message bytes, written in place. **Borrowed rather than
    /// owned**: a second buffer here and a copy out of it put two message-sized
    /// arrays on `emit`'s frame, and `emit` runs on the double-fault stack.
    msg: &'a mut [u8; MAX_RECORD_MESSAGE],
    len: usize,
    /// Bytes past the record's bound, saturating. **Counted rather than
    /// dropped** — the difference between a bound and a lie.
    elided: usize,
    wire: crate::drivers::serial::SerialWriter,
}

impl<'a> Tee<'a> {
    /// The legacy prefix, byte for byte what `log!` produced before this chunk.
    ///
    /// Its `cpu` and `tid` come from an unbracketed `gs:` read, exactly as the
    /// old macro's did — a migration between here and the reservation would
    /// mislabel this line and not the record. That is today's behaviour, it
    /// goes away with the byte ring at L3, and the record's own identity is
    /// read inside the bracket where it cannot be wrong.
    fn open(at_ns: u64, msg: &'a mut [u8; MAX_RECORD_MESSAGE]) -> Self {
        use core::fmt::Write;
        let mut wire = crate::drivers::serial::SerialWriter::lock();
        let secs = at_ns / 1_000_000_000;
        let millis = at_ns % 1_000_000_000 / 1_000_000;
        if PERCPU_READY.load(Ordering::Relaxed) {
            let (cpu, tid) = crate::arch::percpu::log_identity();
            if tid == u32::MAX {
                let _ = write!(wire, "[kernel {secs}.{millis:03} cpu{cpu}] ");
            } else {
                let _ = write!(wire, "[kernel {secs}.{millis:03} cpu{cpu} tid={tid}] ");
            }
        } else {
            let _ = write!(wire, "[kernel {secs}.{millis:03} boot] ");
        }
        Self { msg, len: 0, elided: 0, wire }
    }

    fn finish(mut self) {
        use core::fmt::Write;
        let _ = self.wire.write_str("\n");
    }
}

impl core::fmt::Write for Tee<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.wire.write_bytes(s.as_bytes());

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
/// `specs/issues/diagnostics/a-record-cannot-name-thread-zero.md` is the entry,
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

    // The byte ring's line is finished before the bracket opens, which is what
    // keeps the serial lock *out* of the interrupts-off window rather than
    // putting one inside it. Both sinks are fed from the one format pass either
    // way; L3 deletes this half.
    //
    // **Its timestamp is a second reading and is deliberately not the
    // record's.** A prefix has to be written before the message it introduces,
    // so this one is taken before the format pass — where the record's is taken
    // inside the bracket, one instruction from the reservation it has to agree
    // with. Today's prefix already reads its `cpu` and `tid` unbracketed for the
    // same reason, and all three go with the byte ring at L3.
    let mut tee = Tee::open(crate::clock::nanos_since_boot(), &mut record.msg);
    let _ = core::fmt::Write::write_fmt(&mut tee, args);
    record.len = tee.len as u16;
    record.elided = tee.elided.min(u16::MAX as usize) as u16;
    tee.finish();

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
