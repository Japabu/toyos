//! `klogd` — the kernel thread that puts records on the wire.
//!
//! One thread where every idle CPU used to drain, and that is a reduction this
//! design accepts and names (`specs/log-architecture-spec.md` §4.3). Three
//! things bound it: boot does not need a thread at all, the panic and shutdown
//! paths drain inline and never depend on `klogd` being schedulable, and
//! **`klogd`'s own death is not survivable quietly** — its row in
//! `sched::kthread` is [`OnPanic::Halt`], because a machine whose only console
//! drainer has been killed goes silent with nothing left able to say so.
//!
//! Its body is `drain_ordered`, then park; §2.6a's wake is what makes the park
//! safe. L3 step 1 builds the thread and its hosting; the drain and the wake
//! arrive with the steps that follow.

use crate::sched::kthread::{self, OnPanic};
use crate::scheduler;

/// The name `sched::dump`, `ps` and a crash report use.
///
/// **`klogd` and not `logd` from the first line, and no rename is owed.**
/// `/bin/logd` is a userland program in the same machine from L6, and two
/// things with one name in one machine is a collision a dump report cannot
/// survive.
const NAME: &str = "klogd";

/// Start the thread. Called once, from `kernel_main`, immediately before the
/// machine hands itself to the scheduler.
///
/// **That placement is the whole of the `Drain::Inline` → `Drain::Thread`
/// transition and it is later than §4.2's table said.** The spec put it at
/// `scheduler::init`; the APs spin on `SMP_READY` until the last statement of
/// `kernel_main` and the BSP does not reach a pass before it either, so a
/// `klogd` spawned at `scheduler::init` cannot run for the whole of phases 5,
/// 6 and 7 — which is exactly the window a T14 wedges in, and §4.1's second
/// constraint says that window may not get quieter. So the boot stays inline
/// until the moment something can actually drain.
pub fn start() {
    kthread::spawn(NAME, body, 0, OnPanic::Halt);
}

extern "C" fn body(_arg: u64) -> ! {
    // Deliberately the first thing, before any park: what this stages is a
    // panic *inside a kernel thread*, and the whole question is which branch
    // the panic handler takes.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::klogd_panic() {
        panic!("klogd-panic: the console drainer died");
    }
    loop {
        // L3 step 2 puts `drain_ordered` here and `arm_waiter` between the
        // registration and the park. Until it does, the thread is a real
        // kernel task that parks and never spins — which is what step 1 has
        // to show — and nothing wakes it.
        let ticket = scheduler::prepare_wait(scheduler::park_lot());
        scheduler::block_on(ticket, 0);
    }
}
