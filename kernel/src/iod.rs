//! `iod`: the context deferred filesystem work runs in, because `Drop` has no
//! `Parkable`.
//!
//! The third of the kernel's three threads — `klogd` drains the console
//! (`log/console.rs`), `usbd` owns the xHCI port machine
//! (`drivers::xhci::usbd`), and this one owns the write-back queue: the flush
//! a closed file's dirty pages owe, and page-cache eviction's.
//!
//! **Why the write-back cannot stay where it is.** `OpenFileState::drop`
//! (`object/file.rs`) takes the VFS lock and flushes. Once that lock is a
//! `SleepLock` the flush needs a [`crate::scheduler::Parkable`], and a `Drop`
//! impl cannot take one — which is §6.1's compile-time property stated from the
//! other end, and the reason this thread exists rather than a rule saying
//! "don't block in `Drop`". So the flush becomes a queue and a closed file is
//! pushed onto it; `SYS_FSYNC` submits and parks on the completion, because a
//! caller asked; `SYS_CLOSE` does not, because it never promised durability.
//!
//! **The queue is C12's and the thread is C6's**, which is what §21 means by
//! "`iod`'s body is C12's". What lands here now is the context: a task with no
//! address space of its own, a process-table row that makes it nameable, a park
//! that gives the CPU back, and a panic row. The loop below is where C12's
//! drain goes.
//!
//! **One `iod`, machine-wide, and that is a decision with a measurement owed.**
//! At the 128-core target the root `CLAUDE.md` sets, one thread draining the
//! write-back of 128 cores' closed files is a serialisation point nobody has
//! sized — §10 says so in terms and leaves per-CPU as the obvious escape. It
//! costs nothing to leave open because the producers do not exist yet: at C6
//! nothing pushes, so there is nothing to serialise and no measurement a
//! honest number could come from. **C12 is where it is measured**, against real
//! producers, and this paragraph is the record that the question was asked
//! rather than missed.
//!
//! **Its panic is recoverable.** A killed `iod` costs the machine its deferred
//! write-back — dirty pages stop reaching the device — and that is a loss
//! `SYS_FSYNC`'s own error path and `/bin/logd`'s give-up policy can both see.
//! `klogd`'s is not recoverable for the opposite reason: its loss is the one
//! nothing left alive can report.

use toyos_sched::task::WaitClass;

use crate::completion::{self, Subject, Token};
use crate::sched::kthread::{self, OnPanic};
use crate::scheduler;
use crate::time::Deadline;

/// The name `sched::dump`, `ps` and a crash report use.
const NAME: &str = "iod";

/// Start the thread. Called once, from `kernel_main`, beside `klogd`'s.
pub fn start() {
    let _ = kthread::spawn(NAME, body, 0, OnPanic::Recover);
}

extern "C" fn body(_arg: u64) -> ! {
    let parkable = scheduler::Parkable::of_current();
    let handle = crate::sched::driver::current_handle().expect("iod runs as a task");
    // Armed once and held across the loop — §5.3a's edge contract: a producer
    // that pushes while this thread is draining must find the watch still
    // armed.
    let armed = completion::arm(
        Subject::of(handle.watch()),
        Token::new(0),
        WaitClass::Io,
    )
    .expect("a kernel thread is a task and can arm");
    loop {
        // C12's drain goes here: take the queue's head, flush it under the VFS
        // sleep lock with this thread's own `Parkable`, and post the completion
        // whoever called `SYS_FSYNC` is parked on. Nothing pushes yet.
        //
        // No deadline: what ends this wait is a push, and a periodic wake on a
        // machine with nothing to write back is an audio change (root
        // `CLAUDE.md`).
        //
        // The cancel arm is unreachable: nothing retires a kernel thread.
        let _ = completion::wait(&parkable, &armed, Deadline::never());
    }
}
