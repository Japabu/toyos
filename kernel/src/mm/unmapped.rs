use core::mem::ManuallyDrop;

/// Something whose page-table entries are gone but whose *translations* may not
/// be, wrapped so that it cannot reach the allocator until they are.
///
/// This is the pairing every unmap-then-free path in the kernel owes and none of
/// them expressed before M3: clearing a PDE reaches this CPU's TLB and no other,
/// so a sibling running another thread of the same process — or, on the
/// cross-address-space paths, another process entirely — can still write through
/// an entry for a page the PMM is about to hand to something else.
///
/// **The obligation is discharged by `Drop`, not by a method the caller must
/// remember.** There is no way to get the inner value out except through
/// [`reclaim`](Self::reclaim), which shoots down first, and no way to skip it by
/// dropping the wrapper, which shoots down and then drops the value. The
/// `#[must_use]` is what stops the whole thing being discarded at the point of
/// construction — which would still be correct, just pointlessly early.
///
/// CLAUDE.md's caveat about `Drop` guards is worth checking against rather than
/// waving at, because the question it asks — which paths does this bind, and is
/// the failing one among them — has a good answer here. This value lives on the
/// stack of the CPU that did the unmap, on an ordinary path, and is dropped by
/// that CPU a few statements later. It is not a guard against being killed by
/// somebody else, which is the shape that cannot fire.
///
/// **Where it must be dropped is still the caller's problem**, and the type
/// cannot state it: the shootdown waits for every other CPU, so it may not run
/// while this one holds a lock a target could be spinning on with `IF` clear.
/// That is why `shared_memory` hands one of these *out* of `with_regions_mut`
/// rather than dropping it inside. `arch::tlb::shootdown` asserts the preempt
/// count, so a mistake here is a panic naming the site and never a hang.
#[must_use = "the pages are still reachable from another CPU until this is dropped"]
pub struct Unmapped<T>(ManuallyDrop<T>);

impl<T> Unmapped<T> {
    /// Wrap something that must not reach the allocator until every CPU has
    /// flushed. Call it with the page-table entries already cleared.
    pub fn new(value: T) -> Self {
        Self(ManuallyDrop::new(value))
    }

    /// Shoot down, then hand the value back — for a caller that has its own use
    /// for it and not merely a drop.
    pub fn reclaim(self) -> T {
        crate::arch::tlb::shootdown();
        let mut this = ManuallyDrop::new(self);
        // SAFETY: `this` suppresses the `Drop` below, so the value is taken
        // exactly once and nothing reads it afterwards.
        unsafe { ManuallyDrop::take(&mut this.0) }
    }
}

impl<T> Drop for Unmapped<T> {
    fn drop(&mut self) {
        crate::arch::tlb::shootdown();
        // SAFETY: `reclaim` is the only other consumer and it suppresses this
        // impl, so the value has not been taken.
        unsafe { ManuallyDrop::drop(&mut self.0) };
    }
}
