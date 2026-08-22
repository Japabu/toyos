use core::ops::{Deref, DerefMut};

#[cfg(not(feature = "loom"))]
use core::cell::UnsafeCell;
#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicU32, Ordering};

#[cfg(feature = "loom")]
use crate::cell::UnsafeCell;
#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicU32, Ordering};

/// The load that decides ownership, and what it carries.
///
/// `try_lock` CASes `ticket`, but the atomic an unlock publishes through is
/// `now` — so whichever operation reads `now` is the one that has to carry the
/// acquire, and an acquire on `ticket` would synchronize with nothing.
///
/// **A cargo feature rather than a comment, because a model that has never
/// failed proves nothing.** `kernel-loom`'s `lock-acquire-off` makes this
/// `Relaxed` and `kernel-loom/tests/ticket_lock.rs` must red under it: the
/// previous owner's writes are then unordered against the next owner's reads,
/// which is exactly the class x86's TSO hides from every guest test in this
/// tree. Loom drives `try_lock`'s load and not `lock`'s — the spin is an
/// unbounded branch it cannot explore — so the model that fails is
/// `try_lock_observes_the_previous_owners_writes`. No kernel build can turn the
/// name on: the kernel declares it only so `cfg` checking knows it.
#[cfg(not(feature = "lock-acquire-off"))]
const ACQUIRED: Ordering = Ordering::Acquire;
#[cfg(feature = "lock-acquire-off")]
const ACQUIRED: Ordering = Ordering::Relaxed;

/// Ticket spinlock. Provides mutual exclusion via `lock() -> LockGuard`.
pub struct Lock<T> {
    ticket: AtomicU32,
    now: AtomicU32,
    data: UnsafeCell<T>,
}

// SAFETY: The ticket spinlock ensures exclusive access to T.
// T: Send required because Lock allows T to be accessed from any thread.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    /// Every `Lock` in the kernel is a `static`, so this must stay `const`.
    /// Loom's atomics have no const constructor, hence the second arm.
    #[cfg(not(feature = "loom"))]
    pub const fn new(val: T) -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            data: UnsafeCell::new(val),
        }
    }

    #[cfg(feature = "loom")]
    pub fn new(val: T) -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            data: UnsafeCell::new(val),
        }
    }

    #[track_caller]
    pub fn lock(&self) -> LockGuard<'_, T> {
        crate::preempt::disable();
        let my_ticket = self.ticket.fetch_add(1, Ordering::Relaxed);
        let mut spins = 0u64;
        let mut next_warn = 50_000_000u64;
        while self.now.load(ACQUIRED) != my_ticket {
            core::hint::spin_loop();
            // A waiter answers TLB shootdowns. This spin is the one unbounded
            // wait in the kernel that routinely runs with `IF` clear — every
            // interrupt and exception gate clears it, and handlers take locks —
            // so without this an initiator that waits for acknowledgements while
            // holding a lock is a two-CPU deadlock, and which locks those are
            // could not be enumerated once and stay true. `arch::tlb` has the
            // argument in full.
            crate::arch::tlb::poll();
            spins += 1;
            if spins == next_warn {
                let caller = core::panic::Location::caller();
                crate::log!("LOCK CONTENTION: {}M spins at {}, ticket={} now={}",
                    spins / 1_000_000, caller, my_ticket, self.now.load(Ordering::Relaxed));
                next_warn = (next_warn * 2).min(500_000_000);
            }
            if spins >= 500_000_000 {
                let caller = core::panic::Location::caller();
                panic!("DEADLOCK at {}: 500M spins, ticket={} now={}",
                    caller, my_ticket, self.now.load(Ordering::Relaxed));
            }
        }
        LockGuard { lock: self }
    }

    pub fn try_lock(&self) -> Option<LockGuard<'_, T>> {
        crate::preempt::disable();
        let current = self.now.load(ACQUIRED);
        match self.ticket.compare_exchange(current, current + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => Some(LockGuard { lock: self }),
            Err(_) => {
                crate::preempt::enable();
                None
            }
        }
    }

    /// Raw pointer to the underlying data. Does not acquire the lock.
    /// Only for statics that need a stable address for asm (GDT, TSS, IDT).
    pub fn data_ptr(&self) -> *mut T {
        self.data.get()
    }
}

pub struct LockGuard<'a, T> {
    lock: &'a Lock<T>,
}

impl<T> Deref for LockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: a `LockGuard` exists only where `lock()`/`try_lock()` handed
        // the ticket out, and the ticket is released in `Drop` — so for this
        // guard's whole life no other CPU holds one over the same `Lock`.
        // Irreducible: turning a `&UnsafeCell<T>` into a `&T` is the entire
        // job of a lock, and there is no safe operation that does it — the
        // proof that nothing else is looking lives in the ticket protocol
        // above, not in a type.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for LockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: same ticket argument as `deref`, and `&mut self` adds that
        // no `&T` minted from this guard is alive either. Irreducible for
        // `deref`'s reason.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for LockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.now.fetch_add(1, Ordering::Release);
        crate::preempt::enable();
    }
}


