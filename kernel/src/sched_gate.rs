//! `sched-operation-nesting`: the in-guest gate on what nesting an
//! [`Operation`] may and may not do.
//!
//! **The law is one line and nothing host-side can reach it.**
//! `scheduler::Operation::begin` stores `outer.min(until)` and its `Drop`
//! restores what it displaced, so an inner establishment can only *narrow* — a
//! caller cannot buy itself more device time by starting a second operation
//! inside the first, which is the failure `block::OPERATION` exists to stop
//! arriving one layer lower. The type reaches `percpu::cpu_id` and
//! `driver::current_handle`, and `kernel/` is excluded from the host workspace,
//! so nothing outside a booted machine can construct one. This is what a test
//! can read instead.
//!
//! **It measures and stages nothing.** No driver behaviour changes with it
//! armed, no device is touched and no deadline it establishes outlives the
//! function: three guards on one stack, dropped in reverse, and the slot is
//! back to where the caller left it. `io_depth_probe` is the same shape of
//! actuator — an instrument rather than an injection.
//!
//! **Both homes, because the slot a context establishes in is a decision too.**
//! A task's word lives on its [`TaskHandle`](crate::sched::payload::TaskHandle)
//! and a context with no task uses one slot per CPU, and until this ran in two
//! places every establishment any test had ever driven was the second kind:
//! `nvme_gate` and `usb_gate` both establish from a boot phase, where there is
//! no current task at all. So [`run`] is called twice — once from
//! `kernel_main`, and once from `iod`'s body, which is the kernel's own task
//! that reaches its loop with nothing to do (`crate::iod`) and is therefore the
//! one context that can ask this question without displacing work.
//!
//! What each line carries is the *offset* from the instant the sequence
//! started, not an absolute deadline, so the numbers a test asserts on are the
//! numbers this file asks for and not a function of when the machine booted.

use crate::clock;
use crate::scheduler::Operation;
use crate::time::{Deadline, Duration};

/// The three deadlines, as offsets from the base instant.
///
/// Chosen so that each nesting step is a different question: [`INNER`] is
/// earlier than [`OUTER`], so level 2 must narrow; [`WIDER`] is later than
/// both, so level 3 must change nothing. Their spacing is arbitrary and their
/// magnitudes are irrelevant — nothing waits for any of them.
const OUTER: u64 = 1_000_000_000;
const INNER: u64 = 250_000_000;
const WIDER: u64 = 4_000_000_000;

/// Establish three nested operations and report what every level observed.
///
/// `site` names the context, which is the whole of what distinguishes the two
/// calls: `boot` has no task and establishes in its CPU's slot, `iod` is a task
/// and establishes in its own handle's.
pub fn run(site: &str) {
    let base = clock::now();
    log!(
        "sched-op: {site} outside established={}",
        Operation::established(),
    );

    let level = |until: u64| Deadline::at(base + Duration::from_nanos(until));
    let observed = || Operation::deadline().nanos() - base.nanos_since_boot();

    let outer = Operation::begin(level(OUTER));
    log!(
        "sched-op: {site} begin level=1 asked={OUTER} observed={}",
        observed(),
    );
    {
        let inner = Operation::begin(level(INNER));
        log!(
            "sched-op: {site} begin level=2 asked={INNER} observed={}",
            observed(),
        );
        {
            // The one that matters: an establishment asking for *more* than the
            // frame above it, which must change nothing at all.
            let _wider = Operation::begin(level(WIDER));
            log!(
                "sched-op: {site} begin level=3 asked={WIDER} observed={}",
                observed(),
            );
        }
        log!(
            "sched-op: {site} end level=3 observed={} established={}",
            observed(),
            Operation::established(),
        );
        drop(inner);
        log!(
            "sched-op: {site} end level=2 observed={} established={}",
            observed(),
            Operation::established(),
        );
    }
    drop(outer);
    log!(
        "sched-op: {site} end level=1 established={}",
        Operation::established(),
    );
}
