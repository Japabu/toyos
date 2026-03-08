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

    pub fn lock(&self) -> LockGuard<'_, T> {
        let my_ticket = self.ticket.fetch_add(1, Ordering::Relaxed);
        while self.now.load(Ordering::Acquire) != my_ticket {
            core::hint::spin_loop();
        }
        LockGuard { lock: self }
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
// IrqLock — spinlock that disables interrupts while held
// ---------------------------------------------------------------------------

/// Ticket spinlock that disables interrupts on the local CPU while held.
/// Prevents deadlock when the same lock might be acquired from both
/// normal code and interrupt handlers on the same CPU.
pub struct IrqLock<T> {
    inner: Lock<T>,
}

// SAFETY: Same as Lock — the ticket lock ensures exclusive access to T.
unsafe impl<T: Send> Sync for IrqLock<T> {}

impl<T> IrqLock<T> {
    pub const fn new(val: T) -> Self {
        Self { inner: Lock::new(val) }
    }

    pub fn lock(&self) -> IrqLockGuard<'_, T> {
        let saved_rflags = save_rflags();
        disable_interrupts();
        let guard = self.inner.lock();
        // Field drop order is declaration order: guard drops first (releases lock),
        // then _restore_irq drops (re-enables interrupts if they were enabled).
        IrqLockGuard { guard, _restore_irq: RestoreIrq(saved_rflags) }
    }

    pub fn data_ptr(&self) -> *mut T {
        self.inner.data_ptr()
    }

    /// # Safety
    /// Caller must ensure the lock is currently held and this is called exactly once.
    /// Does NOT restore interrupt state — caller must handle that separately.
    pub unsafe fn force_unlock(&self) {
        self.inner.force_unlock();
    }
}

/// Guard for IrqLock. Fields drop in declaration order:
/// 1. `guard` drops → releases the ticket lock
/// 2. `_restore_irq` drops → restores interrupt flag
pub struct IrqLockGuard<'a, T> {
    guard: LockGuard<'a, T>,
    _restore_irq: RestoreIrq,
}

impl<T> Deref for IrqLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { &self.guard }
}

impl<T> DerefMut for IrqLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T { &mut self.guard }
}

/// Restores the interrupt flag (IF) on drop if it was set before the lock.
struct RestoreIrq(u64);

impl Drop for RestoreIrq {
    fn drop(&mut self) {
        if self.0 & 0x200 != 0 {
            enable_interrupts();
        }
    }
}

#[inline(always)]
fn save_rflags() -> u64 {
    let rflags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}", out(reg) rflags, options(nomem, preserves_flags)); }
    rflags
}

#[inline(always)]
fn disable_interrupts() {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)); }
}

#[inline(always)]
fn enable_interrupts() {
    unsafe { core::arch::asm!("sti", options(nomem, nostack)); }
}
