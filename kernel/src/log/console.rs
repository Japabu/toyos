//! The kernel's console sink, the two drain modes, and `klogd`.
//!
//! One thread where every idle CPU used to drain, and that is a reduction this
//! design accepts and names. Three
//! things bound it: boot does not need a thread at all, the panic and shutdown
//! paths drain inline and never depend on `klogd` being schedulable, and
//! **`klogd`'s own death is not survivable quietly** — its row in
//! `sched::kthread` is [`OnPanic::Halt`], because a machine whose only console
//! drainer has been killed goes silent with nothing left able to say so.
//!
//! **Its death is survivable by design, which is why its panic may not be.**
//! Records keep committing into the shards whatever happens here: the oldest
//! are dropped and counted, `lost` derives from `head` and `next` rather than
//! from a counter, and `snapshot_committed` reads the shards directly — so the
//! panic path is unaffected by `klogd` being gone. What is lost is the live
//! console, which is exactly the thing nothing else can report.
//!
//! **[`Drain::Inline`] and [`Drain::Thread`] are phases and not fallbacks.**
//! Exactly one is active, the transition is a single statement — [`start`] —
//! and there is no second flag to disagree with the first about which one the
//! machine is in: `Drain::Inline` *is* [`KLOGD`] being null.

use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, Ordering};

use alloc::sync::Arc;

use toyos_abi::log::LogRecord;
use toyos_sched::task::{WakeCause, WakeReason};
use toyos_sched::waitq::wake_direct;

use crate::drivers::serial::{self, BackendGuard};
use crate::hw::HW;
use crate::sched::driver::{cpus, irq_off};
use crate::sched::kthread::{self, OnPanic};
use crate::sched::payload::KShared;
use crate::scheduler;

use super::read::{drain_ordered, Published, RecordSink};
use super::shard;

/// The name `sched::dump`, `ps` and a crash report use.
///
/// **`klogd` and not `logd` from the first line, and no rename is owed.**
/// `/bin/logd` is a userland program in the same machine from L6, and two
/// things with one name in one machine is a collision a dump report cannot
/// survive.
const NAME: &str = "klogd";

/// `klogd`'s rendezvous word, or null before it is spawned.
///
/// **`emit` finds `klogd` through this and never through the process table**:
/// `wake_task`'s `process::thread_sched` lookup takes a lock, and `log!` runs
/// inside `sync.rs`, inside IRQ handlers, inside the scheduler and inside every
/// syscall's locked region. The `Arc` is leaked once at spawn and read
/// `Acquire`, which is the shape `driver::CPUS` already has in the same tree.
///
/// **Null is `Drain::Inline`'s whole state and needs no branch of its own.**
/// There is no second flag to disagree with this one about which mode the
/// machine is in.
static KLOGD: AtomicPtr<Arc<KShared>> = AtomicPtr::new(core::ptr::null_mut());

/// Who puts a committed record on the wire.
///
/// A fallback is a path taken when another one fails; these are phases. Exactly
/// one is active, the transition is [`start`], it happens once, and it is
/// logged.
pub enum Drain {
    /// The producer itself, on its own stack, immediately after committing.
    /// Nothing else can run yet: there is no thread until `klogd`'s spawn, and
    /// no CPU reaches a scheduler pass before the two statements after it.
    Inline,
    /// `klogd`, made runnable at the commit of the record it will drain, by the
    /// producer's own `wake_direct` (§2.6a).
    Thread,
}

/// Which phase this machine is in. One load, of the same word the wake post
/// reads.
pub fn mode() -> Drain {
    if KLOGD.load(Ordering::Acquire).is_null() {
        Drain::Inline
    } else {
        Drain::Thread
    }
}

/// Where the console's drain has got to.
///
/// One position for every context that drains, which is what stops a record
/// reaching the wire twice however the machine happens to be running when it is
/// committed. `read::Published` carries the argument for the shape.
static DRAINED: Published = Published::new();

/// Start the thread. Called once, from `kernel_main`, immediately before the
/// machine hands itself to the scheduler.
///
/// **That placement is the whole of the `Drain::Inline` → `Drain::Thread`
/// transition and it is later than §4.2's first draft said.** The APs spin on
/// `SMP_READY` until the second-to-last statement of `kernel_main` and the BSP
/// reaches no pass before `enter_idle_loop`, so a `klogd` spawned at
/// `scheduler::init` cannot run for the whole of phases 5, 6 and 7 — which is
/// the window a machine with no console wedges in, and §4.1's second constraint
/// says that window may not get quieter.
pub fn start() {
    let sched = kthread::spawn(NAME, body, 0, OnPanic::Halt);
    // Leaked deliberately: `klogd` never exits, and a producer reading this
    // pointer from inside a locked region may not touch a refcount that could
    // reach zero under it.
    let shared: &'static Arc<KShared> = alloc::boxed::Box::leak(alloc::boxed::Box::new(sched.shared));
    KLOGD.store(shared as *const _ as *mut _, Ordering::Release);
}

/// Post the wake `shard::signal_after_commit` said this producer owns.
///
/// Called from `emit`, after the publication bracket has closed and the
/// caller's RFLAGS are back — so this is a *second* bracket of the same kind
/// and not the one §2.3a argues for.
pub fn post_wake() {
    let ptr = KLOGD.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    // SAFETY: written once from a leaked `Box`, never cleared, so the pointer
    // is live for the rest of the machine's life.
    let shared = unsafe { &*ptr };
    irq_off(|guard| {
        wake_direct(shared, WakeCause::new(WakeReason::Woken), cpus(), &HW, guard);
    });
}

/// Put every committed record this machine has not yet spoken on the wire.
///
/// **Three callers — `emit` in `Drain::Inline`, `klogd`, and a console arriving
/// — and one function rather than a mode of anything.** What differs between
/// the phases is who calls it, never what it does.
///
/// **One acquisition per chunk, not one per drain**, so the interrupts-off
/// window is bounded by [`CHUNK_RECORDS`] however long the backlog is.
///
/// **`try_lock` and not `lock`, and the reason is a deadlock rather than
/// latency.** `BackendGuard::lock` clears IF and then spins, so it is not
/// re-entrant on its own CPU — and in `Drain::Inline` the caller is an
/// arbitrary producer. A Ring 0 exception taken inside the backend write whose
/// handler logs would spin there forever with interrupts off, on the one path
/// that exists to report it. Declining costs nothing: the record stays
/// committed in its shard, the position is shared, and whoever holds the
/// backend re-scans every shard before it releases — so the holder drains the
/// decliner's record too, in `at_ns` order, and the next `emit` or `klogd` wake
/// catches whatever was committed after that last scan.
pub fn drain_inline() {
    if !serial::has_console() {
        return;
    }
    loop {
        let Some(mut guard) = BackendGuard::try_lock() else { return };
        let records = drain_bounded(&mut guard, CHUNK_RECORDS);
        drop(guard);
        if records < CHUNK_RECORDS {
            return;
        }
    }
}

/// Records per backend acquisition on the live paths.
///
/// **`BackendGuard` holds interrupts off for its whole life, so this number is
/// an interrupt latency and not a batch size.** Draining the whole backlog under
/// one guard is what the code did until 2026-08-15 and it is measurable from
/// outside: `i8042_undecoded_bytes` red 2 times in 5 full suites, on a
/// controller whose byte arrived while a drain had interrupts masked, and
/// `71_macro_empty_arg` red 3 in 5 because a daemon's own `write` waited behind
/// the same guard and landed after a marker it was written before. Neither reds
/// on the byte-ring commit, so both were this window.
///
/// Eight is the byte ring's 512-byte `DRAIN_CHUNK` in the unit this drain moves:
/// the corpus's mean rendered line is 89.4 bytes, so a chunk was five or six
/// lines. The outer loop re-acquires, so a backlog still drains in one visit —
/// it just lets an interrupt in between chunks, which is the whole point.
const CHUNK_RECORDS: u64 = 8;

/// The whole backlog under one guard, for a caller that already holds it: the
/// panic flush and the shutdown flush. **Interrupt latency is not a
/// consideration on either** — one is halting the machine and the other is
/// cutting the power — and both would rather have the report whole than let
/// anything in between its lines.
pub fn drain_locked(guard: &mut BackendGuard) {
    drain_bounded(guard, u64::MAX);
}

/// Advance the console's position over records nothing can carry.
///
/// **`klogd`'s, and nobody else's.** A machine with no backend has nowhere to
/// put a record, and until 2026-08-15 `klogd` answered that by parking
/// *unarmed*: with the position standing still, an armed waiter would find a
/// committed record on every rescan and spin for the life of the machine. That
/// was right about the spin and wrong about the consequence — an unarmed
/// `klogd` never wakes, so it never reaches `user::post_readiness`, so **the one
/// machine shape this whole design exists for posts no log readiness at all**
/// and `/bin/logd` parks for ever with `/log` unwritten
/// (`issues/diagnostics/a-console-less-machine-posts-no-log-readiness.md`).
///
/// Advancing costs nothing that machine had: the records stay in their shards
/// for the panel, which reads them through `snapshot_committed` and not through
/// this position; `panic_flush` refuses on `has_console()` before it looks at
/// it; and a backend arriving later rewinds it whole ([`backend_changed`]).
/// What it buys is a `klogd` that can park armed, so a commit wakes it, so a
/// reader watching `Source::Log` hears about records on a machine with no
/// console — which is every T14 and every `--diag-boot` image.
///
/// It is deliberately **not** in [`drain_inline`], whose other two callers are a
/// producer mid-`emit` and a panicking machine: a `Drain::Inline` boot with no
/// console would then walk every shard per record, which is exactly the cost
/// §4.2 gates that mode on `has_console()` to avoid.
fn discard_pending() {
    let mut cursor = DRAINED.take();
    let mut sink = Discard;
    drain_ordered(&mut cursor, &mut sink);
    DRAINED.put(&cursor);
    LOST.store(DRAINED.lost(), Ordering::Relaxed);
}

/// At most `budget` records to a held backend. Returns how many went.
fn drain_bounded(guard: &mut BackendGuard, budget: u64) -> u64 {
    let mut cursor = DRAINED.take();
    let mut sink = Wire { out: guard, records: 0, budget };
    drain_ordered(&mut cursor, &mut sink);
    let records = sink.records;
    DRAINED.put(&cursor);
    RECORDS.fetch_add(records, Ordering::Relaxed);
    LOST.store(DRAINED.lost(), Ordering::Relaxed);
    records
}

/// Drain with no lock at all, straight to the 16550.
///
/// # Safety
/// Panic path only, and only once a bounded wait for a clean `BackendGuard`
/// handoff has failed (`serial::panic_flush`). What is unsynchronised is the
/// *position*: a wedged holder may be between its walk and its publication, so
/// a record can reach the wire twice. Twice is the right side of that trade on
/// a machine that is halting — the alternative is a report that never arrives
/// because the CPU holding the backend died holding it.
pub unsafe fn drain_bypassed() {
    let mut cursor = DRAINED.take();
    let mut sink = Raw;
    drain_ordered(&mut cursor, &mut sink);
    DRAINED.put(&cursor);
}

/// Which backend the drain has already spoken to, as [`serial::Backend`]'s
/// discriminant.
static SPOKEN_TO: AtomicU8 = AtomicU8::new(serial::Backend::None as u8);

/// A backend has appeared, or the machine has switched to a better one. Say the
/// whole boot again, into the one that is current now.
///
/// **The rewind is what `log_ring::set_serial_sink`'s re-seed from `retained`
/// was**, and it is exact where that was a 64 KiB window. A record written
/// while the only backend was a 16550 went to the 16550; when virtio-console
/// comes up in phase 6 the machine has a *different* channel that has heard
/// none of it, and on the harness's shape that channel is the only one anybody
/// reads. Rewinding to the oldest record each shard still holds and draining
/// again is what puts the boot on it.
///
/// **Both channels then carry the early boot and neither carries it twice**,
/// because `BackendGuard::write_raw` writes to exactly one backend and this
/// only fires when that choice changes. A machine whose backend never changes —
/// metal-sim, or a T14 with no console at all — replays nothing.
pub fn backend_changed() {
    let now = serial::backend() as u8;
    if SPOKEN_TO.swap(now, Ordering::Relaxed) != now {
        DRAINED.rewind();
    }
    drain_inline();
    // The rewind above moved the position backwards under a parked `klogd`,
    // and nothing commits a record to wake it: the whole boot is now pending
    // and the producer that would have posted has long since returned.
    post_wake();
}

/// The longest line a record renders to: the tag, the ABI's bracket at its
/// widest, the message, and the elision note.
///
/// It is a buffer and not a bound. A line that somehow ran past it spills to
/// the backend and carries on under the one guard, so it is still whole on the
/// wire — where a bound would have to choose between truncating and lying.
const LINE_BYTES: usize = toyos_abi::log::MAX_RECORD_MESSAGE + 160;

/// One rendered line on its way to the backend.
///
/// **Buffered, and that is a device fact rather than tidiness.**
/// `virtio_console::write_bytes_locked` is one host round trip per call and a
/// record's `Display` is eight or nine fragments, so writing them straight
/// through would pay eight vmexits a line for the whole boot.
struct Line<F: FnMut(&[u8])> {
    emit: F,
    buf: [u8; LINE_BYTES],
    len: usize,
    /// The ABI's line opens with the bracket the tag has to sit inside, and
    /// this says that first fragment has been dealt with.
    tagged: bool,
}

impl<F: FnMut(&[u8])> Line<F> {
    fn new(emit: F) -> Self {
        Self { emit, buf: [0; LINE_BYTES], len: 0, tagged: false }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            (self.emit)(&self.buf[..self.len]);
            self.len = 0;
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(LINE_BYTES) {
            if self.len + chunk.len() > LINE_BYTES {
                self.flush();
            }
            self.buf[self.len..self.len + chunk.len()].copy_from_slice(chunk);
            self.len += chunk.len();
        }
    }

    fn finish(mut self) {
        self.push(b"\n");
        self.flush();
    }
}

impl<F: FnMut(&[u8])> core::fmt::Write for Line<F> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if !self.tagged {
            self.tagged = true;
            // **The tag is composed with the ABI's line rather than derived
            // from a second copy of its fields.** `LogRecord`'s `Display` opens
            // with the bracket the tag belongs inside, so the tag replaces that
            // one byte and everything that varies — the timestamp, the origin,
            // the `boot` label, the tid, the elision note — is rendered once,
            // in `toyos-abi`, for this sink and the panel and `logd` alike. A
            // `LogRecord::tagged(&str)` beside `Display` would be tidier and is
            // a *sysroot* change: §11 lands the ABI alone and it has already
            // landed, so this branch composes instead of reopening it.
            // `issues/diagnostics/a-console-tag-is-composed-by-replacing-a-bracket.md`.
            //
            // If that leading bracket ever goes, the fragment passes through
            // whole: a visible `[kernel [0.1 …` beats a line silently missing
            // its first character.
            let rest = s.strip_prefix('[').unwrap_or(s);
            self.push(rest.as_bytes());
            return Ok(());
        }
        self.push(s.as_bytes());
        Ok(())
    }
}

/// Render one record as the console line — byte for byte what the byte ring
/// carried, `[kernel ` and all.
///
/// **Public because `/log`'s sink renders the same line**, and a second
/// implementation of it there would be a second thing to keep agreeing with the
/// panel. It goes when `logd` does the rendering (L6), which is also when the
/// wall-clock prefix stops being this one.
pub fn write_line(record: &LogRecord, emit: impl FnMut(&[u8])) {
    use core::fmt::Write;
    let mut line = Line::new(emit);
    line.push(b"[kernel ");
    let _ = write!(line, "{record}");
    line.finish();
}

/// Records through a backend the caller holds, up to a budget.
///
/// **`false` before the record rather than after it**, which is what
/// `RecordSink` means by it: `drain_ordered` leaves the cursor *at* the record
/// it was refused, so the next acquisition starts exactly there and the budget
/// costs nothing but a re-scan.
struct Wire<'a> {
    out: &'a mut BackendGuard,
    records: u64,
    budget: u64,
}

impl RecordSink for Wire<'_> {
    fn put(&mut self, record: &LogRecord) -> bool {
        if self.records >= self.budget {
            return false;
        }
        write_line(record, |bytes| self.out.write_raw(bytes));
        self.records += 1;
        true
    }
}

/// Records nothing can carry, on a machine with no backend. The position moves
/// and the bytes are never built.
struct Discard;

impl RecordSink for Discard {
    fn put(&mut self, _record: &LogRecord) -> bool {
        true
    }
}

/// Records straight to the 16550, for the bypass. No lock, bounded per byte.
struct Raw;

impl RecordSink for Raw {
    fn put(&mut self, record: &LogRecord) -> bool {
        write_line(record, serial::panic_raw);
        true
    }
}

extern "C" fn body(_arg: u64) -> ! {
    // Deliberately the first thing, before any drain: what this stages is a
    // panic *inside a kernel thread*, and the whole question is which branch
    // the panic handler takes.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::klogd_panic() {
        panic!("klogd-panic: the console drainer died");
    }

    loop {
        // One bounded chunk per backend acquisition, exactly as
        // `drain_chunk_to_serial` was, so an interrupts-off window is never
        // longer than `CHUNK_RECORDS` lines. With no backend the position moves
        // and nothing is rendered — `discard_pending` says why that is this
        // thread's job and not `drain_inline`'s.
        if serial::has_console() {
            drain_inline();
        } else {
            discard_pending();
        }

        // **The one context in the machine that has just observed committed
        // records and may take a lock**, which is why the readiness post is
        // here and not in `emit`: each per-source watcher list is a `Lock<Vec<_>>`
        // the post clones under the lock, and taking a lock is the one thing
        // `emit` may not do. One
        // post per batch rather than one per record, and none at all while
        // nothing is watching.
        //
        // It is outside `drain_inline` deliberately: that function's other two
        // callers are a producer mid-`emit` and a panicking machine, and
        // neither may touch `IO_URINGS`.
        super::user::post_readiness();

        // **Register, then arm, then park — in that order, and the order is the
        // lost wake's other half.** `prepare_wait` moves the word to
        // `Committing`, so a producer that wins the swap from here on takes
        // `Claim::PrePark` and this thread's own commit refuses to park. Arming
        // first would leave a window where the producer claims a still-`Running`
        // `klogd`, takes `Claim::Lost`, drops the wake, and `klogd` parks on a
        // committed record.
        let ticket = scheduler::prepare_wait(scheduler::park_lot());
        // **A machine with no console arms exactly like one that has a
        // console, and it did not until 2026-08-15.** It parked unarmed then,
        // on the reasoning that an armed waiter with a standing position would
        // find a committed record on every rescan and spin — true of the
        // position standing still, which `discard_pending` is what fixes. The
        // consequence of the unarmed park was that this thread never woke, so
        // it never reached `post_readiness`, so on the one machine shape this
        // design exists for a userland reader was never told records had moved.
        if shard::arm_waiter(shard::log_waiter(), || DRAINED.any_pending()) {
            ticket.cancel();
            continue;
        }
        // No deadline. A spurious wake is legal and costs one re-drain; a
        // missing one is what W3's two fences exist to make impossible, and a
        // timeout here would hide exactly that.
        PARKS.fetch_add(1, Ordering::Relaxed);
        scheduler::block_on(ticket, 0);
    }
}

/// What the console drain has done, for a machine that has gone quiet and is
/// being asked why.
///
/// **Three numbers rather than a heartbeat**, and each answers a different
/// question the console alone cannot: `records` says the drain is running at
/// all, `parks` says `klogd` is parking rather than spinning, and `lost` says
/// whether a producer outran it — which is the one number a reader of the
/// console can never derive, because what it names is the lines that are not
/// there. Read only by `sched::dump`.
static RECORDS: AtomicU64 = AtomicU64::new(0);
static LOST: AtomicU64 = AtomicU64::new(0);
static PARKS: AtomicU64 = AtomicU64::new(0);

/// `(records drained, records lost, parks)`. Three relaxed loads: the dump may
/// take no lock.
pub fn stats() -> (u64, u64, u64) {
    (
        RECORDS.load(Ordering::Relaxed),
        LOST.load(Ordering::Relaxed),
        PARKS.load(Ordering::Relaxed),
    )
}
