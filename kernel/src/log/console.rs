//! `klogd` — the kernel thread that drains committed records, and the wake that
//! makes it runnable at the commit of the record it will drain.
//!
//! One thread where every idle CPU used to drain, and that is a reduction this
//! design accepts and names (`specs/log-architecture-spec.md` §4.3). Three
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

use core::sync::atomic::{AtomicPtr, Ordering};

use alloc::sync::Arc;

use toyos_sched::task::{WakeCause, WakeReason};
use toyos_sched::waitq::wake_direct;

use crate::hw::HW;
use crate::sched::driver::{cpus, irq_off};
use crate::sched::kthread::{self, OnPanic};
use crate::sched::payload::KShared;
use crate::scheduler;

use super::read::{drain_ordered, Cursor, RecordSink};
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

extern "C" fn body(_arg: u64) -> ! {
    // Deliberately the first thing, before any drain: what this stages is a
    // panic *inside a kernel thread*, and the whole question is which branch
    // the panic handler takes.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::klogd_panic() {
        panic!("klogd-panic: the console drainer died");
    }

    let mut cursor = Cursor::new();
    let mut sink = Drained::default();
    loop {
        drain_ordered(&mut cursor, &mut sink);
        DRAINED.store(sink.records, Ordering::Relaxed);
        LOST.store(cursor.lost(), Ordering::Relaxed);

        // **Register, then arm, then park — in that order, and the order is the
        // lost wake's other half.** `prepare_wait` moves the word to
        // `Committing`, so a producer that wins the swap from here on takes
        // `Claim::PrePark` and this thread's own commit refuses to park. Arming
        // first would leave a window where the producer claims a still-`Running`
        // `klogd`, takes `Claim::Lost`, drops the wake, and `klogd` parks on a
        // committed record.
        let ticket = scheduler::prepare_wait(scheduler::park_lot());
        if shard::arm_waiter(shard::log_waiter(), || super::read::any_committed(&cursor)) {
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

/// Where `klogd`'s records go until the chunk that points it at the backend.
///
/// It counts rather than renders, which is what keeps this chunk's claim —
/// nothing observable changes — true while the byte ring still owns the wire.
/// A line printed from here would be a record committed from inside the drain
/// that produced it, so the counts go into the three words below and
/// `sched::dump` is what reads them.
#[derive(Default)]
struct Drained {
    records: u64,
}

impl RecordSink for Drained {
    fn put(&mut self, _record: &toyos_abi::log::LogRecord) -> bool {
        self.records += 1;
        true
    }
}

/// What `klogd` has done, for a machine that has gone quiet and is being asked
/// why.
///
/// **Three numbers rather than a heartbeat**, and each answers a different
/// question the console alone cannot: `drained` says the thread is running at
/// all, `parks` says it is parking rather than spinning, and `lost` says
/// whether a producer outran it — which is the one number a reader of the
/// console can never derive, because what it names is the lines that are not
/// there. Written only by `klogd` and read only by `sched::dump`.
static DRAINED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static LOST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static PARKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `(records drained, records lost, parks)`. Three relaxed loads: the dump may
/// take no lock.
pub fn stats() -> (u64, u64, u64) {
    (
        DRAINED.load(Ordering::Relaxed),
        LOST.load(Ordering::Relaxed),
        PARKS.load(Ordering::Relaxed),
    )
}
