use crate::mm::pmm;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::mm::paging::CachePolicy;
use crate::mm::{PAGE_2M, Unmapped, align_2m};
use crate::process::{PageTables, Pid};
use crate::sync::Lock;
use crate::{DirectMap, UserAddr};

// SharedToken — opaque handle for a shared memory region

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SharedToken(u32);

impl SharedToken {
    pub fn raw(self) -> u32 { self.0 }
    pub fn from_raw(v: u32) -> Self { Self(v) }
}

#[derive(Clone, Copy, Debug)]
pub enum Error {
    NotFound,
    PermissionDenied,
    OutOfVirtualMemory,
    InvalidSize,
    OutOfMemory,
}

// Ownership — who manages the backing memory lifetime

enum Ownership {
    /// Kernel-owned (GPU framebuffers, DMA buffers).
    /// Never freed by shared_memory — the kernel subsystem manages the lifetime.
    Kernel,
    /// Process-owned. Freed when the owning process exits and no mappings remain.
    Process { pid: Pid, _pages: Vec<pmm::PhysPage> },
}

// SharedRegion — a single shared memory region

struct SharedRegion {
    phys: DirectMap,
    size: u64,
    ownership: Ownership,
    /// The memory type every mapping of this region carries, in every process
    /// and in the kernel's own view. One per region and not one per mapping:
    /// SDM Vol. 3A §11.12.4 rules out one physical page held under two memory
    /// types.
    cache: CachePolicy,
    allowed: Vec<Pid>,
    /// Per-process mappings: each process gets its own virtual address.
    mapped_in: Vec<(Pid, PageTables, UserAddr)>,
}

impl SharedRegion {
    /// Map this region into a process's address space via its AddressSpace allocator.
    /// Returns the per-process virtual address, or the existing one if already mapped.
    fn map_into(&mut self, pid: Pid, pt: &PageTables) -> Option<UserAddr> {
        if let Some((_, _, vaddr)) = self.mapped_in.iter().find(|(p, _, _)| *p == pid) {
            return Some(*vaddr);
        }
        let (addr, _) = pt.lock().alloc_and_map(self.phys.phys(), self.size, true, self.cache)?;
        // A region whose memory type is not RAM's gets a line naming the
        // process, because that process is the one paying the difference and
        // nothing else in the machine says which one it is. Read back out of
        // its page tables, so the line is about the mapping and not the
        // request.
        if self.cache != CachePolicy::DeferToMtrr {
            let installed = pt.lock().user_policy(addr).expect("shm: just mapped");
            crate::log!("shm: {:#x} mapped {:?} into pid {}", self.phys.phys(), installed, pid);
        }
        self.mapped_in.push((pid, Arc::clone(pt), addr));
        Some(addr)
    }

    /// Unmap this region from a process, returning the VA to its AddressSpace pool.
    ///
    /// The virtual address goes back to that process's allocator, so even where
    /// no physical page is freed the caller still owes a shootdown: a sibling
    /// holding a stale entry for the address reads whatever the next mapping
    /// puts there.
    fn unmap_from(&mut self, pid: Pid) {
        if let Some(pos) = self.mapped_in.iter().position(|(p, _, _)| *p == pid) {
            let (_, pt, vaddr) = self.mapped_in.swap_remove(pos);
            pt.lock().free_and_unmap(vaddr);
        }
    }

    /// Unmap from every process that holds a mapping.
    ///
    /// By `&mut self` and not by value: the region has to outlive the shootdown
    /// its callers owe, because dropping it is what returns the pages to the
    /// PMM. Taking it by value put the free strictly *before* the flush.
    fn unmap_all(&mut self) {
        for (_, pt, vaddr) in self.mapped_in.drain(..) {
            pt.lock().free_and_unmap(vaddr);
        }
    }
}

// Lock ordering: REGIONS lock → PageTables lock.
// All public functions that call map_into/unmap_from do so while holding
// the REGIONS lock (inside with_regions_mut), then acquire PageTables inside.
static REGIONS: Lock<Option<Vec<(SharedToken, SharedRegion)>>> = Lock::new(None);
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);

fn with_regions_mut<R>(f: impl FnOnce(&mut Vec<(SharedToken, SharedRegion)>) -> R) -> R {
    let mut guard = REGIONS.lock();
    f(guard.as_mut().expect("shared_memory not initialized"))
}

pub fn init() {
    *REGIONS.lock() = Some(Vec::new());
}

fn next_token() -> SharedToken {
    SharedToken(NEXT_TOKEN.fetch_add(1, Ordering::Relaxed))
}

/// Allocate 2MB-aligned shared memory. Maps it into the owner's page tables.
/// Returns a token; other processes can map it via `map()` after `grant()`.
///
/// Fallible because `size` comes from userland, and all three failures — a
/// zero size, a size above free memory, and an exhausted virtual range — are
/// errors rather than panics. No bound is invented above that:
/// `alloc_contiguous` already refuses more than free physical memory, which is
/// a physical limit rather than a chosen one.
pub fn alloc(size: u64, owner_pid: Pid, addr_space: &PageTables) -> Result<SharedToken, Error> {
    if size == 0 || (size as usize).checked_add(PAGE_2M as usize - 1).is_none() {
        return Err(Error::InvalidSize);
    }
    let aligned_size = align_2m(size as usize);
    let page_count = aligned_size / PAGE_2M as usize;
    let pages = pmm::alloc_contiguous(page_count, pmm::Category::SharedMemory)
        .ok_or(Error::OutOfMemory)?;
    let phys = DirectMap::from_phys(pages[0].direct_map().phys());

    with_regions_mut(|regions| {
        let token = next_token();
        let mut region = SharedRegion {
            phys,
            size: aligned_size as u64,
            ownership: Ownership::Process { pid: owner_pid, _pages: pages },
            cache: CachePolicy::DeferToMtrr,
            allowed: alloc::vec![owner_pid],
            mapped_in: Vec::new(),
        };
        // On this path the region is dropped, which returns its physical
        // pages to the PMM — nothing leaks.
        region.map_into(owner_pid, addr_space).ok_or(Error::OutOfVirtualMemory)?;
        regions.push((token, region));
        Ok(token)
    })
}

/// Register an existing kernel-owned allocation as a shared region.
/// Permanent: never auto-removed. Used for GPU framebuffers and DMA buffers.
#[must_use]
pub fn register(phys: DirectMap, size: u64, cache: CachePolicy) -> SharedToken {
    assert!(phys.phys() & (PAGE_2M - 1) == 0,
        "shared_memory::register: phys {:#x} not 2MB-aligned", phys.phys());
    let token = next_token();
    with_regions_mut(|regions| {
        regions.push((token, SharedRegion {
            phys,
            size,
            ownership: Ownership::Kernel,
            cache,
            allowed: Vec::new(),
            mapped_in: Vec::new(),
        }));
    });
    token
}

/// Regions unmapped from every process and not yet freed, held by the caller so
/// that the shootdown happens before their memory can be reissued. Opaque: the
/// only thing anyone does with one is drop it, and the drop is the point.
pub struct Retired(Vec<SharedRegion>);

/// Unregister a shared region, unmapping it from all processes.
///
/// The wrapper is built outside `with_regions_mut` on purpose: constructing it
/// costs nothing, but *dropping* it waits for every other CPU, and doing that
/// under the region lock would hold it across an IPI round trip.
///
/// It used to return `(phys, size)` "so the caller can free the backing memory",
/// which no caller ever did — `virtio_gpu` frees through the `FbAlloc` it kept.
pub fn unregister(token: SharedToken) -> Option<Unmapped<Retired>> {
    with_regions_mut(|regions| {
        let pos = regions.iter().position(|(t, _)| *t == token)?;
        let (_, mut region) = regions.swap_remove(pos);
        region.unmap_all();
        Some(Retired(alloc::vec![region]))
    })
    .map(Unmapped::new)
}

/// Grant a process permission to map a shared region. Owner only.
///
/// Being allowed to map is not the right to hand on: a grantee that could
/// re-grant makes the owner's ACL transitive, so soundd's per-client audio
/// ring reaches anyone that client names and the owner is never told. The
/// capability design says the same thing with rights — a receiver gets a
/// `MAP`-only handle, with no `DUP` to pass on (spec §8.3) — and this is that
/// rule expressed against pids. `Ownership::Kernel` regions have no owner and
/// so cannot be granted from userland at all; the framebuffer and the DMA
/// windows are handed out by `grant_kernel` at claim time.
pub fn grant(token: SharedToken, caller: Pid, target: Pid) -> Result<(), Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        if !matches!(region.ownership, Ownership::Process { pid, .. } if pid == caller) {
            return Err(Error::PermissionDenied);
        }
        if !region.allowed.contains(&target) {
            region.allowed.push(target);
        }
        Ok(())
    })
}

/// Grant permission on a kernel-owned region.
pub fn grant_kernel(token: SharedToken, target: Pid) -> Result<(), Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        if !matches!(region.ownership, Ownership::Kernel) {
            return Err(Error::PermissionDenied);
        }
        if !region.allowed.contains(&target) {
            region.allowed.push(target);
        }
        Ok(())
    })
}

/// Map a shared region into the caller's address space.
/// Returns the per-process virtual address.
pub fn map(token: SharedToken, pid: Pid, addr_space: &PageTables) -> Result<u64, Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        if !region.allowed.contains(&pid) {
            return Err(Error::PermissionDenied);
        }
        let vaddr = region.map_into(pid, addr_space)
            .ok_or(Error::OutOfVirtualMemory)?;
        Ok(vaddr.raw())
    })
}

/// Is this region unreachable, so that dropping it frees memory nobody can
/// still be using?
///
/// `allowed` and `mapped_in` together are the reference count, and neither
/// alone is: `map` requires membership in `allowed`, so a grantee that has not
/// mapped yet is still entitled to the pages, and a mapper holds pointers into
/// them. Only `grant` — owner only — adds to `allowed`, and only a live
/// process can be granted to, so both sets drain.
///
/// Kernel-owned regions are never reclaimed here. They are device windows
/// registered permanently by a driver, and both sets are empty for the whole
/// window between `register` and the first claim.
fn unreachable_region(region: &SharedRegion) -> bool {
    matches!(region.ownership, Ownership::Process { .. })
        && region.allowed.is_empty()
        && region.mapped_in.is_empty()
}

/// Release a process's reference to a shared region, freeing the region once
/// nobody holds one.
///
/// The reclaim test used to live only in `cleanup_process`, which made process
/// exit the only thing that ever freed a region: a client that closed an audio
/// stream and kept running left soundd's 2 MiB ring resident until some
/// unrelated process happened to exit and the sweep passed over it.
///
/// This is a stepping stone to the refcounted handle, not a substitute for it.
/// `SharedToken` is still a bare `u32` with no RAII, and the count it stands
/// in for is still two `Vec`s of pids maintained by hand — the ground
/// `specs/capability-handles-spec.md` §6.2 claims with `Arc<SharedMemObject>`
/// is unchanged. A working reclaim path makes that debt less visible, not less
/// real.
pub fn release(token: SharedToken, pid: Pid) -> Result<(), Error> {
    let dropped = with_regions_mut(|regions| {
        let pos = regions.iter().position(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        let (_, region) = &mut regions[pos];
        region.allowed.retain(|p| *p != pid);
        region.unmap_from(pid);
        Ok(unreachable_region(region).then(|| regions.swap_remove(pos)))
    })?;
    // Out here, and it is the drop that frees the pages: `unmap_from` cleared
    // this process's PDEs on this CPU alone, so until every other one has
    // flushed, a sibling thread still has a writable window onto memory the PMM
    // is about to hand to somebody else.
    drop(Unmapped::new(dropped));
    Ok(())
}

/// Destroy a process-owned shared region: unmap from all processes, remove from
/// table, and free backing pages (via Drop). The caller must be the owner.
pub fn destroy(token: SharedToken, owner: Pid) -> Result<(), Error> {
    let dropped = with_regions_mut(|regions| {
        let pos = regions.iter().position(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        let (_, ref region) = regions[pos];
        match &region.ownership {
            Ownership::Process { pid, .. } if *pid == owner => {}
            _ => return Err(Error::PermissionDenied),
        }
        let (_, mut region) = regions.swap_remove(pos);
        region.unmap_all();
        Ok(region)
    })?;
    drop(Unmapped::new(dropped));
    Ok(())
}

/// Remove all mappings and permissions for a given PID, freeing any region
/// that becomes unreachable.
///
/// Exit is now just the bulk form of `release`, and asks the same question.
/// It used to free a region as soon as its *owner* left, which threw away
/// pages a grantee was still entitled to map; `unreachable_region` waits for
/// the grantee too, and cannot leak while waiting because every pid in
/// `allowed` names a process whose own exit passes through here.
pub fn cleanup_process(pid: Pid) -> Unmapped<Retired> {
    let dropped = with_regions_mut(|regions| {
        let mut dropped = Vec::new();
        let mut i = 0;
        while i < regions.len() {
            let (_, region) = &mut regions[i];
            region.unmap_from(pid);
            region.allowed.retain(|p| *p != pid);
            if unreachable_region(region) {
                // `swap_remove` moves the last entry into `i`, so the index is
                // not advanced — the entry now sitting there has not been asked.
                dropped.push(regions.swap_remove(i).1);
            } else {
                i += 1;
            }
        }
        dropped
    });
    // Handed back rather than dropped here, because this runs under the process
    // table lock and the drop waits for every other CPU. The caller drops it
    // where nothing is held.
    Unmapped::new(Retired(dropped))
}
