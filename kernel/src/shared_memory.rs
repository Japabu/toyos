use alloc::alloc::{alloc_zeroed, dealloc};
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::paging::{self, PAGE_2M};
use crate::process::Pid;
use crate::sync::Lock;
use crate::PhysAddr;

#[derive(Clone, Copy, Debug)]
pub enum Error {
    NotFound,
    PermissionDenied,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SharedToken(u32);

impl SharedToken {
    pub fn raw(self) -> u32 { self.0 }
    pub fn from_raw(v: u32) -> Self { Self(v) }
}

/// Who owns the backing memory and how it should be cleaned up.
enum Ownership {
    /// Kernel-owned (e.g. GPU framebuffers, DMA buffers).
    /// Never freed by shared_memory — the kernel subsystem manages the lifetime.
    Kernel,
    /// Process-owned. Freed when the owning process exits and no mappings remain.
    Process { pid: Pid, layout: Layout },
}

// SAFETY: SharedRegion contains raw PML4 pointers in mapped_in.
// These are physical memory addresses valid across all CPUs.
// Access is serialized by the REGIONS lock.
unsafe impl Send for SharedRegion {}

struct SharedRegion {
    phys_addr: PhysAddr,
    size: u64,
    ownership: Ownership,
    allowed: Vec<Pid>,                // PIDs allowed to map
    mapped_in: Vec<(Pid, *mut u64)>,  // (PID, PML4) for processes that have mapped it
}

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
/// Returns a token; the owner can retrieve the address via `map()`.
#[must_use]
pub fn alloc(size: u64, owner_pid: Pid, owner_pml4: *mut u64) -> SharedToken {
    let aligned_size = paging::align_2m(size as usize);
    let layout = Layout::from_size_align(aligned_size, PAGE_2M as usize).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "shared_memory: allocation failed");
    let phys_addr = PhysAddr::from_ptr(ptr);

    paging::map_user_in(owner_pml4, phys_addr, aligned_size as u64);

    let token = next_token();
    with_regions_mut(|regions| {
        regions.push((token, SharedRegion {
            phys_addr,
            size: aligned_size as u64,
            ownership: Ownership::Process { pid: owner_pid, layout },
            allowed: alloc::vec![owner_pid],
            mapped_in: alloc::vec![(owner_pid, owner_pml4)],
        }));
    });

    token
}

/// Register an existing kernel-owned allocation as a shared region.
/// Permanent: never auto-removed. Used for GPU framebuffers and DMA buffers.
#[must_use]
pub fn register(phys_addr: PhysAddr, size: u64) -> SharedToken {
    let token = next_token();
    with_regions_mut(|regions| {
        regions.push((token, SharedRegion {
            phys_addr,
            size,
            ownership: Ownership::Kernel,
            allowed: Vec::new(),
            mapped_in: Vec::new(),
        }));
    });
    token
}

/// Unregister a kernel-owned shared region, unmapping it from all processes.
/// Returns `(phys_addr, size)` so the caller can free the backing memory.
pub fn unregister(token: SharedToken) -> Option<(PhysAddr, u64)> {
    with_regions_mut(|regions| {
        let pos = regions.iter().position(|(t, _)| *t == token)?;
        let (_, region) = regions.swap_remove(pos);
        for &(_, pml4) in &region.mapped_in {
            paging::unmap_user(pml4, region.phys_addr, region.size);
        }
        Some((region.phys_addr, region.size))
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

/// Grant permission on a kernel-owned region. Only works for regions with no owner.
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
pub fn map(token: SharedToken, pid: Pid, pml4: *mut u64) -> Result<u64, Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        if !region.allowed.contains(&pid) {
            return Err(Error::PermissionDenied);
        }
        if !region.mapped_in.iter().any(|(p, _)| *p == pid) {
            paging::map_user_in(pml4, region.phys_addr, region.size);
            region.mapped_in.push((pid, pml4));
        }
        Ok(region.phys_addr.raw())
    })
}

/// Release a shared region for a process (unmap, revoke permission).
pub fn release(token: SharedToken, pid: Pid) -> Result<(), Error> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)
            .ok_or(Error::NotFound)?;
        region.allowed.retain(|p| *p != pid);
        if let Some(pos) = region.mapped_in.iter().position(|(p, _)| *p == pid) {
            let (_, pml4) = region.mapped_in.swap_remove(pos);
            paging::unmap_user(pml4, region.phys_addr, region.size);
        }
        Ok(())
    })
}

/// Remove all mappings and permissions for a given PID.
/// Also frees process-owned regions (non-permanent) that become empty.
pub fn cleanup_process(pid: Pid) {
    with_regions_mut(|regions| {
        regions.retain_mut(|(_, region)| {
            // Unmap from this process
            if let Some(pos) = region.mapped_in.iter().position(|(p, _)| *p == pid) {
                let (_, pml4) = region.mapped_in.swap_remove(pos);
                paging::unmap_user(pml4, region.phys_addr, region.size);
            }
            region.allowed.retain(|p| *p != pid);

            // If process-owned and no mappings remain, free the backing memory.
            if let Ownership::Process { pid: owner, layout } = region.ownership {
                if owner == pid && region.mapped_in.is_empty() {
                    unsafe { dealloc(region.phys_addr.as_mut_ptr(), layout); }
                    return false; // remove from list
                }
            }
            // Kernel-owned regions are never freed here.
            true
        });
    })
}
