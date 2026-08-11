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
use toyos_sched::task::WaitClass;

use crate::process::Pid;
use crate::sched::payload::KWaitQueue;
use crate::sched::waitqs::{new_queue, wake_all};
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
    waiters: Arc<KWaitQueue>,
}

impl ProcessObject {
    pub fn new(pid: Pid) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            pid,
            exit: Lock::new(None),
            finished: AtomicBool::new(false),
            waiters: new_queue(WaitClass::Other),
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

    pub fn waiters(&self) -> Arc<KWaitQueue> {
        self.waiters.clone()
    }

    /// Publish the exit and release every waiter.
    ///
    /// Idempotent by assertion rather than by tolerance: two publishes mean two
    /// teardowns claimed one process, which `claim_teardown` exists to prevent.
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
        wake_all(&self.waiters);
    }
}
