//! A region of memory more than one process can see.
//!
//! What replaced the token registry. A `SharedToken` was a `u32` a process
//! presented and a list of pids that were allowed to present it — designation
//! as capability, with the owner maintaining the ACL by hand. A region is an
//! object now: holding a handle to one is the whole of being allowed to map it,
//! and giving one away is `SYS_HANDLE_SEND`.
//!
//! **Two lifetimes, and keeping them apart is the point.** The *mappings* go
//! when the last handle goes, from the deferred queue with nothing held,
//! because that is a userland-visible event and a handle a killed thread
//! stranded must not delay it. The *pages* go when the last `Arc` goes, which
//! is strictly later — a handle holds an `Arc` — so the unmap and its shootdown
//! are always in front of the free. A driver that keeps its own `Arc` past a
//! mode change is relying on exactly that: the compositor's mapping of the old
//! scanout stays valid until the compositor closes it, and nothing revokes.

use alloc::sync::Arc;
use alloc::vec::Vec;

use toyos_abi::syscall::SyscallError;

use crate::mm::paging::{CachePolicy, Prot};
use crate::mm::{align_2m, pmm, Unmapped, PAGE_2M};
use crate::process::{PageTables, Pid};
use crate::sync::Lock;
use crate::{DirectMap, UserAddr};

use super::{KObjectVariant, ObjectCore, ZeroHandles};

/// Physical pages a region keeps alive.
///
/// Behind an `Arc` because one page set can back several objects: a device
/// window is a fresh [`SharedMemObject`] per claim — an object whose handle
/// count has reached zero is retired for good and can never be named again —
/// while the pages under it are the driver's and outlive every claimant.
// Never read: holding the vector *is* the job, and the pages go back to the
// PMM when the last `Arc` to this drops. `expect` rather than `allow`, so a
// reader that appears has to justify itself.
#[expect(dead_code)]
pub struct Pages(Vec<pmm::PhysPage>);

impl Pages {
    pub fn new(pages: Vec<pmm::PhysPage>) -> Self {
        Self(pages)
    }
}

// SAFETY: open question, not resolved by this comment. The intent is real —
// a `PhysPage` is a physical address and a category byte, nothing in it is
// bound to the CPU that allocated it, and the pages are handed between CPUs
// by every path that frees a process — but `PhysPage` is itself `{phys: u64,
// category: u8}` with no manual `Send`/`Sync` impl, so `Vec<PhysPage>` (and
// therefore `Pages`) already derives both automatically: `cargo check` with
// these two impls deleted still compiles clean (verified 2026-08-20). Filed
// as issues/kernel/redundant-send-sync-impls-mm-object.md together with the
// same finding on `paging::AddressSpace`, rather than removed here — if a
// future `PhysPage` field broke the auto-derive (a raw pointer, say), these
// hand-written impls would silently paper over it with no new review.
unsafe impl Send for Pages {}
// SAFETY: see the `Send` impl above — same open question, same evidence.
unsafe impl Sync for Pages {}

/// A physical range and the memory type every mapping of it must carry.
///
/// One per region and not one per mapping: SDM Vol. 3A §11.12.4 rules out one
/// physical page held under two memory types, and the panic console writes
/// through the direct map while a compositor holds a mapping of the same
/// scanout.
#[derive(Clone)]
pub struct Region {
    pub phys: DirectMap,
    pub size: u64,
    pub cache: CachePolicy,
    /// The pages, when somebody in the kernel owns them. `None` for a window
    /// the kernel does not own — firmware's framebuffer, an MMIO aperture.
    #[expect(dead_code, reason = "the Arc is what keeps the pages alive; nothing reads it")]
    pub pages: Option<Arc<Pages>>,
}

impl Region {
    /// A placeholder for a driver struct built before its buffers exist. Size
    /// zero, so nothing can be mapped through it if one is ever left behind.
    pub fn empty() -> Self {
        Self { phys: DirectMap::from_phys(0), size: 0, cache: CachePolicy::DeferToMtrr, pages: None }
    }
}

pub struct SharedMemObject {
    pub(super) core: ObjectCore,
    region: Region,
    /// Where this region is mapped, per process. Emptied by the zero-handle
    /// hook, which is also the one place the shootdown happens.
    mapped_in: Lock<Vec<(Pid, PageTables, UserAddr)>>,
}

impl SharedMemObject {
    /// A region over memory somebody else owns: unmapped when the last handle
    /// goes, never freed here.
    pub fn over(region: Region) -> Arc<Self> {
        assert!(
            region.phys.phys() & (PAGE_2M - 1) == 0,
            "shm: {:#x} is not 2 MiB aligned",
            region.phys.phys(),
        );
        Arc::new(Self {
            core: Self::new_core(),
            region,
            mapped_in: Lock::new(Vec::new()),
        })
    }

    /// A fresh allocation, rounded up to whole 2 MiB pages.
    ///
    /// Fallible because `size` crossed the syscall boundary: a size that cannot
    /// be expressed in whole pages is `InvalidArgument` and memory the machine
    /// does not have is `ResourceExhausted`. No bound is invented above that —
    /// `alloc_contiguous` already refuses more than free physical memory, which
    /// is a physical limit rather than a chosen one.
    pub fn create(size: u64) -> Result<Arc<Self>, SyscallError> {
        if size == 0 || (size as usize).checked_add(PAGE_2M as usize - 1).is_none() {
            return Err(SyscallError::InvalidArgument);
        }
        let aligned = align_2m(size as usize);
        let pages = pmm::alloc_contiguous(aligned / PAGE_2M as usize, pmm::Category::SharedMemory)
            .ok_or(SyscallError::ResourceExhausted)?;
        let phys = DirectMap::from_phys(pages[0].direct_map().phys());
        Ok(Self::over(Region {
            phys,
            size: aligned as u64,
            cache: CachePolicy::DeferToMtrr,
            pages: Some(Arc::new(Pages(pages))),
        }))
    }

    pub fn size(&self) -> u64 {
        self.region.size
    }

    /// The kernel's own view of the pages, for a subsystem that reads them
    /// through the direct map — an inbox's ring headers are the one case.
    ///
    /// Says nothing about who else can see them. A region reached through this
    /// while a process holds a mapping of it is memory two writers share, and
    /// the reader has to treat it that way: atomics, or a volatile read of a
    /// value it copies out once. `inbox.rs` is where that is spelled out.
    pub fn phys(&self) -> DirectMap {
        self.region.phys
    }

    /// The same address, for a subsystem that is about to *fill* the pages
    /// before anybody else can see them.
    ///
    /// **The assert is the whole method.** A kernel write through the direct
    /// map is exclusive only while the region is mapped nowhere; the moment
    /// [`map_into`](Self::map_into) has run, a sibling thread of the owning
    /// process can be writing the same bytes and a kernel still initialising is
    /// racing it. That ordering is easy to reverse by accident and impossible
    /// to observe when it is wrong — `inbox::create` built its ring headers
    /// *after* mapping for as long as inboxes had existed, and nothing anywhere
    /// would have said so. This says so.
    ///
    /// Cheap: one uncontended lock and a length test, once per region ever
    /// created.
    pub fn phys_before_mapping(&self) -> DirectMap {
        assert!(
            self.mapped_in.lock().is_empty(),
            "shm koid {}: the region is mapped into a process already, so a kernel \
             write through the direct map is not exclusive",
            self.core.koid().raw(),
        );
        self.region.phys
    }

    /// Map into `pt`, or answer the address it is already mapped at.
    ///
    /// Idempotent per process, so a second `SYS_SHM_MAP` is the first one's
    /// answer rather than a second window onto the same pages.
    pub fn map_into(&self, pid: Pid, pt: &PageTables) -> Result<u64, SyscallError> {
        let mut mapped = self.mapped_in.lock();
        if let Some((_, _, vaddr)) = mapped.iter().find(|(p, _, _)| *p == pid) {
            return Ok(vaddr.raw());
        }
        let (addr, _) = pt
            .lock()
            .alloc_and_map(self.region.phys.phys(), self.region.size, Prot::ReadWrite, self.region.cache)
            .ok_or(SyscallError::ResourceExhausted)?;
        // A region whose memory type is not RAM's gets a line naming the
        // process, because that process is the one paying the difference and
        // nothing else in the machine says which one it is. Read back out of
        // its page tables, so the line is about the mapping and not the
        // request.
        if self.region.cache != CachePolicy::DeferToMtrr {
            let installed = pt.lock().user_policy(addr).expect("shm: just mapped");
            crate::log!(
                "shm: {:#x} mapped {:?} into pid {}",
                self.region.phys.phys(),
                installed,
                pid
            );
        }
        mapped.push((pid, Arc::clone(pt), addr));
        Ok(addr.raw())
    }

    /// Take this process's mapping away, if it has one.
    ///
    /// The virtual address goes back to that process's allocator, so even where
    /// no physical page is freed the caller still owes a shootdown: a sibling
    /// holding a stale entry for the address reads whatever the next mapping
    /// puts there.
    #[must_use = "the caller owes a shootdown before the address can be reissued"]
    pub fn unmap_from(&self, pid: Pid) -> Option<Unmapped<()>> {
        let mut mapped = self.mapped_in.lock();
        let pos = mapped.iter().position(|(p, _, _)| *p == pid)?;
        let (_, pt, vaddr) = mapped.swap_remove(pos);
        pt.lock().free_and_unmap(vaddr);
        Some(Unmapped::new(()))
    }
}

/// Every mapping goes, and the flush happens here.
///
/// The pages do not: they belong to `Region::pages`, which the last `Arc`
/// frees. A handle holds an `Arc`, so this always runs first.
impl ZeroHandles for SharedMemObject {
    fn on_zero_handles(&self) {
        let mapped = core::mem::take(&mut *self.mapped_in.lock());
        if mapped.is_empty() {
            return;
        }
        for (_, pt, vaddr) in &mapped {
            pt.lock().free_and_unmap(*vaddr);
        }
        drop(Unmapped::new(mapped));
    }
}

impl Drop for SharedMemObject {
    fn drop(&mut self) {
        debug_assert!(
            self.mapped_in.lock().is_empty(),
            "shm koid {} freed with a live mapping: the zero-handle hook did \
             not run, so the pages go back to the PMM under somebody's window",
            self.core.koid().raw(),
        );
    }
}
