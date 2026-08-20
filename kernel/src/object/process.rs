//! A process, as something a handle can name.
//!
//! **The exit code belongs to the object, not to a table entry.** A pid-keyed
//! wait needed the process table to keep a corpse around until somebody claimed
//! it, which is what a zombie was, and it needed rules for who was allowed to
//! claim one and what happened when nobody did. None of that exists here: the
//! spawn that made the process answered with a handle, the teardown publishes
//! the code into the object that handle names, and the table entry is freed as
//! soon as its threads are gone. A wait after the fact reads a value; a wait
//! before it parks and is woken by the publish.
//!
//! So there is no reap, no orphan adoption, no "exactly once" and no window in
//! which an exit is missed — and a process nobody holds a handle to simply
//! disappears.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use toyos_abi::syscall::ProcessStats;

use crate::process::Pid;
use crate::completion::{self, Outcome, Subject, Watch};
use crate::sync::Lock;

use super::{KObjectVariant, ObjectCore};

/// What is left of a process once it has stopped running.
pub struct Exit {
    pub code: i32,
    pub stats: ProcessStats,
}

pub struct ProcessObject {
    pub(super) core: ObjectCore,
    pid: Pid,
    /// Written exactly once, by whichever of exit, kill or panic recovery owns
    /// this process's teardown.
    exit: Lock<Option<Exit>>,
    /// The same fact, readable without taking the lock: a parked waiter's
    /// predicate runs on every wake, and a `Lock` there is a `fetch_add` on a
    /// path that already has one.
    finished: AtomicBool,
    /// What a `SYS_PROCESS_WAIT` arms on. The object is what the waiter holds
    /// across its park, so the watch it names cannot outlive its subject.
    ///
    /// **One waiter set where there were two.** The `KWaitQueue` beside this
    /// went with the park it served: after §5.6 a thread arms here and parks
    /// on its own queue, so a second list on the object had nothing left in
    /// it.
    watch: Watch,
}

impl ProcessObject {
    pub fn new(pid: Pid) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            pid,
            exit: Lock::new(None),
            finished: AtomicBool::new(false),
            watch: Watch::new(),
        })
    }

    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit.lock().as_ref().map(|e| e.code)
    }

    /// The last accounting the process had. `None` while it is still running —
    /// a live process is sampled from its own `ProcessData` instead, which is
    /// where the numbers are still moving.
    pub fn final_stats(&self) -> Option<ProcessStats> {
        self.exit.lock().as_ref().map(|e| e.stats)
    }

    pub fn watch(&self) -> &Watch {
        &self.watch
    }

    /// Publish the exit and release every waiter.
    ///
    /// Idempotent by assertion rather than by tolerance: two publishes mean two
    /// teardowns claimed one process, which `claim_teardown` exists to prevent.
    ///
    /// This is also the moment the process's table entry becomes collectable —
    /// `process::reap_finished` takes exactly the entries whose object answers
    /// `finished` — so the idle loop is told, after the store it has to see.
    /// That signal is the only one it gets: without it the loop would have to
    /// take the process table to find out, which is what it did on every trip
    /// until `sched::reap_gate`.
    pub fn publish_exit(&self, exit: Exit) {
        {
            let mut slot = self.exit.lock();
            assert!(
                slot.is_none(),
                "pid {} published two exits ({} then {})",
                self.pid,
                slot.as_ref().map_or(0, |e| e.code),
                exit.code,
            );
            *slot = Some(exit);
        }
        self.finished.store(true, Ordering::Release);
        crate::scheduler::note_reapable();
        completion::post(Subject::of(&self.watch), Outcome::Ready);
    }
}
