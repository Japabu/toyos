//! Where a reader finds a shard, when a writer found its own through `gs:`.
//!
//! **This file is compiled a second time by `kernel-loom`**, for `shard.rs`'s
//! reason and under `shard.rs`'s rule: it may name only what that crate shims.
//! The edge it carries is invisible on x86 — every store is a release there —
//! and it is not hypothetical anywhere else, because what the pointer publishes
//! is not the pointer. An AP's shard is `alloc_zeroed` memory whose `head` the
//! BSP writes before storing the pointer; a reader that saw the pointer without
//! that would read whatever the heap held under a slot's sequence number, and
//! accept it if it happened to equal the number it asked for.
//!
//! `specs/log-architecture-spec.md` §2.2.

#[cfg(not(feature = "loom"))]
use core::sync::atomic::{AtomicPtr, Ordering};
#[cfg(feature = "loom")]
use loom::sync::atomic::{AtomicPtr, Ordering};

use toyos_abi::log::MAX_LOG_SHARDS;

/// The store that makes a shard reachable, and what it carries.
///
/// What the pair orders is the zeroing and the initial `head`, not the pointer:
/// an AP's shard is `alloc_zeroed` memory the BSP builds before storing the
/// pointer, and a reader that saw the pointer without this would read whatever
/// the heap held under a slot's sequence number.
///
/// **A cargo feature rather than a comment, because a model that has never
/// failed proves nothing.** `kernel-loom`'s `shard-publish-relaxed` makes both
/// sides `Relaxed` and `kernel-loom/tests/log_publish.rs` must red under it. No
/// kernel build can turn the name on: the kernel declares it only so `cfg`
/// checking knows it.
#[cfg(not(feature = "shard-publish-relaxed"))]
const PUBLISH: Ordering = Ordering::Release;
#[cfg(feature = "shard-publish-relaxed")]
const PUBLISH: Ordering = Ordering::Relaxed;
#[cfg(not(feature = "shard-publish-relaxed"))]
const OBSERVE: Ordering = Ordering::Acquire;
#[cfg(feature = "shard-publish-relaxed")]
const OBSERVE: Ordering = Ordering::Relaxed;

/// One path in both builds: in the kernel `super` is `crate::log`, and in
/// `kernel-loom` it is the crate root, which re-exports `log_shard` under this
/// name for exactly that reason. A `cfg` here instead would have to be the
/// *harness's* cfg rather than loom's, and the two are not the same question —
/// which is how the harness's non-loom invocation stopped compiling.
use super::shard::Shard;

/// Every CPU's shard but cpu0's, published as the BSP builds that CPU's
/// `PerCpu`.
///
/// cpu0 needs no slot: its shard is the boot shard, a `static` reachable from
/// the kernel's first instruction, which is where the boot a panel exists to
/// report begins.
#[cfg(not(feature = "loom"))]
static AP_SHARDS: [AtomicPtr<Shard>; MAX_LOG_SHARDS - 1] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_LOG_SHARDS - 1];

/// Loom's atomics have no `const` constructor, so the model builds its registry
/// at run time and hands it in. Nothing else differs: the two functions below
/// take the slots rather than reaching for them.
#[cfg(feature = "loom")]
pub type Slots = [AtomicPtr<Shard>; MAX_LOG_SHARDS - 1];

#[cfg(feature = "loom")]
pub fn slots() -> Slots {
    core::array::from_fn(|_| AtomicPtr::new(core::ptr::null_mut()))
}

/// Make an AP's shard reachable to a reader. Called once per AP, on the BSP,
/// before that AP executes an instruction.
///
/// **cpu0 is not a caller and does not get an arm here.** `alloc_log_shard`
/// returns before reaching this, so a zero arriving anyway is a caller that has
/// changed its mind about that — a kernel bug, and it dies as one rather than
/// being quietly absorbed.
///
/// # Safety
/// `shard` must be a live, initialised [`Shard`] that is never freed.
pub unsafe fn publish(slots: &[AtomicPtr<Shard>], cpu: u32, shard: *mut Shard) {
    let slot = (cpu as usize)
        .checked_sub(1)
        .and_then(|ap| slots.get(ap))
        .unwrap_or_else(|| panic!("log: cpu{cpu} has no shard slot in an ABI of {MAX_LOG_SHARDS}"));
    slot.store(shard, PUBLISH);
}

/// The shard `cpu` published, if it has one.
///
/// `Acquire` against [`publish`]'s `Release`: what the pair orders is the
/// zeroing and the initial `head`, not the pointer.
pub fn published(slots: &[AtomicPtr<Shard>], ap: usize) -> Option<&'static Shard> {
    let ptr = slots.get(ap)?.load(OBSERVE);
    // SAFETY: `publish`'s contract is a live shard that is never freed, and the
    // pointer is only ever written once.
    (!ptr.is_null()).then(|| unsafe { &*ptr })
}

/// The kernel's own registry, which is the one `emit`'s readers walk.
#[cfg(not(feature = "loom"))]
pub fn kernel_slots() -> &'static [AtomicPtr<Shard>] {
    &AP_SHARDS
}
