use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

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
    pub const fn new(val: T) -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            data: UnsafeCell::new(val),
        }
    }

    #[track_caller]
    pub fn lock(&self) -> LockGuard<'_, T> {
        let my_ticket = self.ticket.fetch_add(1, Ordering::Relaxed);
        let mut spins = 0u64;
        let mut next_warn = 50_000_000u64;
        while self.now.load(Ordering::Acquire) != my_ticket {
            core::hint::spin_loop();
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
        let current = self.now.load(Ordering::Relaxed);
        self.ticket.compare_exchange(current, current + 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| LockGuard { lock: self })
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
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for LockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for LockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.now.fetch_add(1, Ordering::Release);
    }
}

impl<T> Lock<T> {
    /// Release the lock without a guard. Used when the lock is acquired on one
    /// stack (before context_switch) and released on another (after resume).
    ///
    /// # Safety
    /// Caller must ensure the lock is currently held and this is called exactly once.
    pub unsafe fn force_unlock(&self) {
        self.now.fetch_add(1, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Lock<Option<T>> projection — lock and unwrap in one step
// ---------------------------------------------------------------------------

impl<T> Lock<Option<T>> {
    /// Lock and project through the Option, returning a guard that derefs to T.
    /// Panics if the Option is None (i.e. not yet initialized).
    pub fn lock_unwrap(&self) -> OptionGuard<'_, T> {
        let guard = self.lock();
        assert!(guard.is_some(), "lock_unwrap: not initialized");
        OptionGuard { guard }
    }
}

/// Guard that projects `LockGuard<Option<T>>` → `&T` / `&mut T`.
/// Drops the underlying lock when dropped.
pub struct OptionGuard<'a, T> {
    guard: LockGuard<'a, Option<T>>,
}

impl<T> Deref for OptionGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: lock_unwrap asserted Some
        self.guard.as_ref().unwrap()
    }
}

impl<T> DerefMut for OptionGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: lock_unwrap asserted Some
        self.guard.as_mut().unwrap()
    }
}

