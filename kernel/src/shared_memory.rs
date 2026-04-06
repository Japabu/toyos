use crate::mm::pmm;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::mm::{PAGE_2M, align_2m};
use crate::process::{PageTables, Pid};
use crate::sync::Lock;
use crate::{DirectMap, UserAddr};

// ---------------------------------------------------------------------------
// SharedToken — opaque handle for a shared memory region
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SharedToken(u32);

impl SharedToken {
    pub fn raw(self) -> u32 { self.0 }
    pub fn from_raw(v: u32) -> Self { Self(v) }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub enum Error {
    NotFound,
    PermissionDenied,
    OutOfVirtualMemory,
}

// ---------------------------------------------------------------------------
// Ownership — who manages the backing memory lifetime
// ---------------------------------------------------------------------------

enum Ownership {
    /// Kernel-owned (GPU framebuffers, DMA buffers).
    /// Never freed by shared_memory — the kernel subsystem manages the lifetime.
    Kernel,
    /// Process-owned. Freed when the owning process exits and no mappings remain.
    Process { pid: Pid, _pages: Vec<pmm::PhysPage> },
}

// ---------------------------------------------------------------------------
// SharedRegion — a single shared memory region
// ---------------------------------------------------------------------------

struct SharedRegion {
    phys: DirectMap,
    size: u64,
    ownership: Ownership,
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
        let (addr, _) = pt.lock().alloc_and_map(self.phys.phys(), self.size)?;
        self.mapped_in.push((pid, Arc::clone(pt), addr));
        Some(addr)
    }

    /// Unmap this region from a process, returning the VA to its AddressSpace pool.
    fn unmap_from(&mut self, pid: Pid) {
        if let Some(pos) = self.mapped_in.iter().position(|(p, _, _)| *p == pid) {
            let (_, pt, vaddr) = self.mapped_in.swap_remove(pos);
            pt.lock().free_and_unmap(vaddr);
        }
    }

    /// Unmap from all processes. Takes self by value — region must be dropped after.
    fn unmap_all(self) {
        for (_, pt, vaddr) in &self.mapped_in {
            pt.lock().free_and_unmap(*vaddr);
        }
    }
}

// ---------------------------------------------------------------------------
// Global registry
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Allocate 2MB-aligned shared memory. Maps it into the owner's page tables.
/// Returns a token; other processes can map it via `map()` after `grant()`.
#[must_use]
pub fn alloc(size: u64, owner_pid: Pid, addr_space: &PageTables) -> SharedToken {
    let aligned_size = align_2m(size as usize);
    let page_count = aligned_size / PAGE_2M as usize;
    let pages = pmm::alloc_contiguous(page_count, pmm::Category::SharedMemory).expect("shared_memory: allocation failed");
    let phys = DirectMap::from_phys(pages[0].direct_map().phys());

    with_regions_mut(|regions| {
        let token = next_token();
        let mut region = SharedRegion {
            phys,
            size: aligned_size as u64,
            ownership: Ownership::Process { pid: owner_pid, _pages: pages },
            allowed: alloc::vec![owner_pid],
            mapped_in: Vec::new(),
        };
        region.map_into(owner_pid, addr_space)
            .expect("shared_memory::alloc: failed to map into owner");
        regions.push((token, region));
        token
    })
}

/// Register an existing kernel-owned allocation as a shared region.
/// Permanent: never auto-removed. Used for GPU framebuffers and DMA buffers.
#[must_use]
pub fn register(phys: DirectMap, size: u64) -> SharedToken {
    assert!(phys.phys() & (PAGE_2M - 1) == 0,
        "shared_memory::register: phys {:#x} not 2MB-aligned", phys.phys());
    let token = next_token();
    with_regions_mut(|regions| {
        regions.push((token, SharedRegion {
            phys,
            size,
            ownership: Ownership::Kernel,
            allowed: Vec::new(),
            mapped_in: Vec::new(),
        }));
    });
    token
}

/// Unregister a kernel-owned shared region, unmapping it from all processes.
/// Returns `(phys, size)` so the caller can free the backing memory.
pub fn unregister(token: SharedToken) -> Option<(DirectMap, u64)> {
    with_regions_mut(|regions| {
        let pos = regions.iter().position(|(t, _)| *t == token)?;
        let (_, region) = regions.swap_remove(pos);
        let phys = region.phys;
        let size = region.size;
        region.unmap_all();
        Some((phys, size))
    })
}

/// Grant a process permission to map a shared region.
/// The caller must be the owner, or already in the allowed list.
pub fn grant(token: SharedToken, caller: Pid, target: Pid) -> Result<(), Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        let is_owner = matches!(region.ownership, Ownership::Process { pid, .. } if pid == caller);
        if !is_owner && !region.allowed.contains(&caller) {
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

/// Release a shared region for a process (unmap, revoke permission).
pub fn release(token: SharedToken, pid: Pid) -> Result<(), Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        region.allowed.retain(|p| *p != pid);
        region.unmap_from(pid);
        Ok(())
    })
}

/// Destroy a process-owned shared region: unmap from all processes, remove from
/// table, and free backing pages (via Drop). The caller must be the owner.
pub fn destroy(token: SharedToken, owner: Pid) -> Result<(), Error> {
    with_regions_mut(|regions| {
        let pos = regions.iter().position(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        let (_, ref region) = regions[pos];
        match &region.ownership {
            Ownership::Process { pid, .. } if *pid == owner => {}
            _ => return Err(Error::PermissionDenied),
        }
        let (_, region) = regions.swap_remove(pos);
        region.unmap_all();
        // region dropped here → PhysPages dropped → pages returned to PMM
        Ok(())
    })
}

/// Remove all mappings and permissions for a given PID.
/// Also frees process-owned regions that become empty.
pub fn cleanup_process(pid: Pid) {
    with_regions_mut(|regions| {
        regions.retain_mut(|(_, region)| {
            region.unmap_from(pid);
            region.allowed.retain(|p| *p != pid);

            // Drop process-owned regions when the owner exits, or when the
            // last mapper exits (handles orphaned regions whose owner already left).
            if let Ownership::Process { pid: owner, .. } = &region.ownership {
                if (*owner == pid || !region.allowed.contains(owner))
                    && region.mapped_in.is_empty()
                {
                    return false; // PhysPages freed via Drop
                }
            }
            true
        });
    })
}
