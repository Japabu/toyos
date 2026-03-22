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
// Virtual address allocator for shared memory mappings
// ---------------------------------------------------------------------------

/// Shared memory mappings live at 16GB+ in every process's virtual address space,
/// far above code, heap, stack, and mmap regions.
const SHM_VIRT_BASE: u64 = 0x4_0000_0000;

static SHM_VIRT_NEXT: Lock<UserAddr> = Lock::new(UserAddr::new(SHM_VIRT_BASE));

/// Allocate a 2MB-aligned virtual address range for a shared memory mapping.
fn alloc_vaddr(size: u64) -> UserAddr {
    let aligned_size = (size + PAGE_2M - 1) & !(PAGE_2M - 1);
    let mut next = SHM_VIRT_NEXT.lock();
    let aligned = UserAddr::new((next.raw() + PAGE_2M - 1) & !(PAGE_2M - 1));
    *next = UserAddr::new(aligned.raw() + aligned_size);
    aligned
}

// ---------------------------------------------------------------------------
// SharedRegion — a single shared memory region
// ---------------------------------------------------------------------------

struct SharedRegion {
    phys: DirectMap,
    size: u64,
    vaddr: UserAddr,
    ownership: Ownership,
    allowed: Vec<Pid>,
    mapped_in: Vec<(Pid, PageTables)>,
}

impl SharedRegion {
    fn map_into(&mut self, pid: Pid, pt: &PageTables) {
        if !self.mapped_in.iter().any(|(p, _)| *p == pid) {
            pt.lock().map_range(self.vaddr, self.phys.phys(), self.size, true);
            self.mapped_in.push((pid, Arc::clone(pt)));
        }
    }

    fn unmap_from(&mut self, pid: Pid) {
        if let Some(pos) = self.mapped_in.iter().position(|(p, _)| *p == pid) {
            let (_, pt) = self.mapped_in.swap_remove(pos);
            pt.lock().unmap_range(self.vaddr, self.size);
        }
    }

    fn unmap_all(&self) {
        for (_, pt) in &self.mapped_in {
            pt.lock().unmap_range(self.vaddr, self.size);
        }
    }
}

// ---------------------------------------------------------------------------
// Global registry
// ---------------------------------------------------------------------------

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
    let pages = pmm::alloc_contiguous(page_count).expect("shared_memory: allocation failed");
    let phys = DirectMap::from_phys(pages[0].direct_map().phys());
    let vaddr = alloc_vaddr(aligned_size as u64);

    let token = next_token();
    let mut region = SharedRegion {
        phys,
        size: aligned_size as u64,
        vaddr,
        ownership: Ownership::Process { pid: owner_pid, _pages: pages },
        allowed: alloc::vec![owner_pid],
        mapped_in: Vec::new(),
    };
    region.map_into(owner_pid, addr_space);

    with_regions_mut(|regions| regions.push((token, region)));
    token
}

/// Register an existing kernel-owned allocation as a shared region.
/// Permanent: never auto-removed. Used for GPU framebuffers and DMA buffers.
#[must_use]
pub fn register(phys: DirectMap, size: u64) -> SharedToken {
    let vaddr = alloc_vaddr(size);
    let token = next_token();
    with_regions_mut(|regions| {
        regions.push((token, SharedRegion {
            phys,
            size,
            vaddr,
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
        region.unmap_all();
        Some((region.phys, region.size))
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
/// Returns the userland virtual address.
pub fn map(token: SharedToken, pid: Pid, addr_space: &PageTables) -> Result<u64, Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        if !region.allowed.contains(&pid) {
            return Err(Error::PermissionDenied);
        }
        region.map_into(pid, addr_space);
        Ok(region.vaddr.raw())
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

/// Remove all mappings and permissions for a given PID.
/// Also frees process-owned regions that become empty.
pub fn cleanup_process(pid: Pid) {
    with_regions_mut(|regions| {
        regions.retain_mut(|(_, region)| {
            region.unmap_from(pid);
            region.allowed.retain(|p| *p != pid);

            if let Ownership::Process { pid: owner, .. } = &region.ownership {
                if *owner == pid && region.mapped_in.is_empty() {
                    return false; // PhysPages freed via Drop
                }
            }
            true
        });
    })
}
