//! `usbd`: the context USB work runs in, instead of whichever thread trapped.
//!
//! The kernel has three kernel threads and this is the second: `klogd` drains
//! the console (`log/console.rs`), `usbd` owns the xHCI port machine, and
//! `iod` (`crate::iod`) owns the write-back queue. Three and not one, because
//! a stuck USB enumeration must not stop the log — which is exactly what it
//! does today, where [`super::poll_if_pending`] runs at the top of every
//! scheduler pass on every CPU and `wait_transfer` spins with `XHCI` held.
//!
//! **What it will own, and what it owns now.** C7 moves `poll_if_pending` off
//! `drain_irqs` and onto this thread, C9 hands it the i8042's verdict as a
//! deadline park and the polled-device shape that goes with it, and both need
//! `XHCI` to be a `SleepLock` — which is C7+C8's, because a lock converted alone
//! parks with the other three still ticket locks (§21.1). Until then the body
//! below is one park: the thread exists, it is scheduled, it is named in
//! `ps` and in Ctrl+Alt+D, and its panic row says what a panic in it costs. That
//! is C6's whole deliverable and §21 states it in those words — "C6 spawns
//! `usbd` and `iod` on existing machinery".
//!
//! **Spawned on every machine, including one with no xHCI at all.** §10's
//! cardinality is one of each, machine-wide; a controller-less machine gets a
//! thread with nothing to do, which is the same thing every machine has until
//! C7 and costs one 16 KiB kernel stack. Making the spawn conditional would put
//! a second answer to "how many kernel threads does this machine have" in the
//! tree, and `sched::kthread`'s `MAX_KERNEL_TASKS` is the first.
//!
//! **Its panic is recoverable, and `klogd`'s is deliberately not.** A killed
//! `usbd` costs the machine its USB port machine — hot-plug stops working and
//! the boot disk keeps whatever it already had — and every one of those losses
//! is *visible*: the device does not appear, the dump names the thread as gone.
//! A killed `klogd` costs the machine its only console drainer, which is the
//! one failure nothing left alive can report, so that thread halts instead
//! (`log/console.rs`).

use toyos_sched::task::WaitClass;

use crate::completion::{self, Subject, Token};
use crate::sched::kthread::{self, OnPanic};
use crate::scheduler;
use crate::time::Deadline;

/// The name `sched::dump`, `ps` and a crash report use.
const NAME: &str = "usbd";

/// Start the thread. Called once, from `kernel_main`, beside `klogd`'s.
pub fn start() {
    let _ = kthread::spawn(NAME, body, 0, OnPanic::Recover);
}

extern "C" fn body(_arg: u64) -> ! {
    // Deliberately the first thing: what this stages is the *recoverable* half
    // of the panic predicate, which no other test in the suite reaches —
    // `klogd-panic` stages the other half and the two rows are the whole point
    // of `sched::kthread::Row`.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::usbd_panic() {
        panic!("usbd-panic: the device thread died");
    }

    let parkable = scheduler::Parkable::at_entry();
    let handle = crate::sched::driver::current_handle().expect("usbd runs as a task");
    // Armed once and held across the loop, which is §5.3a's edge contract: a
    // post that lands while this thread is doing its step must find the watch
    // still armed, and an arm consumed per wait would lose exactly that wake.
    let armed = completion::arm(
        Subject::of(handle.watch()),
        Token::new(0),
        WaitClass::Io,
    )
    .expect("a kernel thread is a task and can arm");
    loop {
        // C7's step goes here: drain the event ring's outstanding operations,
        // run the port machine, and post what the transfers' waiters are armed
        // on. Nothing posts to this thread yet, so the park below is where it
        // stays.
        //
        // No deadline. What will end this wait is an interrupt's post or a
        // port's own deadline, and a timeout in the meantime would be a wake on
        // a machine with nothing to run — root `CLAUDE.md`'s rule, and an audio
        // change.
        //
        // The cancel arm is unreachable: nothing retires a kernel thread, so the
        // only way this one dies is its own panic, which does not come back
        // here.
        let _ = completion::wait(&parkable, &armed, Deadline::never());
    }
}
