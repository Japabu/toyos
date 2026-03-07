use alloc::alloc::{alloc_zeroed, dealloc};
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::paging::{self, PAGE_2M};
use crate::process::Pid;
use crate::sync::Lock;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SharedToken(u32);

impl SharedToken {
    pub fn raw(self) -> u32 { self.0 }
    pub fn from_raw(v: u32) -> Self { Self(v) }
}

struct SharedRegion {
    phys_addr: u64,
    size: u64,
    owner_pid: Pid,
    allowed: Vec<Pid>,                // PIDs allowed to map
    mapped_in: Vec<(Pid, *mut u64)>,  // (PID, PML4) for processes that have mapped it
    layout: Option<Layout>,           // for freeing; None = kernel-owned
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
    let phys_addr = ptr as u64;

    paging::map_user_in(owner_pml4, phys_addr, aligned_size as u64);

    let token = next_token();
    with_regions_mut(|regions| {
        regions.push((token, SharedRegion {
            phys_addr,
            size: aligned_size as u64,
            owner_pid,
            allowed: alloc::vec![owner_pid],
            mapped_in: alloc::vec![(owner_pid, owner_pml4)],
            layout: Some(layout),
        }));
    });

    token
}

/// Register an existing kernel-owned allocation as a shared region.
/// Used for GPU framebuffers. Not freeable, no initial owner.
#[must_use]
pub fn register(phys_addr: u64, size: u64) -> SharedToken {
    let token = next_token();
    with_regions_mut(|regions| {
        regions.push((token, SharedRegion {
            phys_addr,
            size,
            owner_pid: Pid::MAX,
            allowed: Vec::new(),
            mapped_in: Vec::new(),
            layout: None,
        }));
    });
    token
}

/// Grant a process permission to map a shared region.
/// Caller must be the owner, or for kernel-owned regions, already in the allowed list.
#[must_use]
pub fn grant(token: SharedToken, caller_pid: Pid, target_pid: Pid) -> bool {
    with_regions_mut(|regions| {
        let Some((_, region)) = regions.iter_mut().find(|(t, _)| *t == token) else {
            return false;
        };
        if caller_pid != region.owner_pid && !region.allowed.contains(&caller_pid) {
            return false;
        }
        if !region.allowed.contains(&target_pid) {
            region.allowed.push(target_pid);
        }
        true
    })
}

/// Map a shared region into the caller's address space.
/// Returns the physical address, or None if not allowed.
#[must_use]
pub fn map(token: SharedToken, caller_pid: Pid, caller_pml4: *mut u64) -> Option<u64> {
    with_regions_mut(|regions| {
        let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)?;

        if !region.allowed.contains(&caller_pid) {
            return None;
        }

        if !region.mapped_in.iter().any(|&(pid, _)| pid == caller_pid) {
            paging::map_user_in(caller_pml4, region.phys_addr, region.size);
            region.mapped_in.push((caller_pid, caller_pml4));
        }

        Some(region.phys_addr)
    })
}

/// Release a process's mapping of a shared region. Any mapped process can call this.
/// Unmaps only from the caller's page tables. If no mappings remain, deallocates.
#[must_use]
pub fn release(token: SharedToken, caller_pid: Pid, caller_pml4: *mut u64) -> bool {
    with_regions_mut(|regions| {
        let Some(pos) = regions.iter().position(|(t, _)| *t == token) else {
            return false;
        };

        let region = &mut regions[pos].1;

        // Unmap from caller's page tables
        if let Some(idx) = region.mapped_in.iter().position(|&(p, _)| p == caller_pid) {
            paging::unmap_user(caller_pml4, region.phys_addr, region.size);
            region.mapped_in.swap_remove(idx);
        }

        // Remove from allowed list
        if let Some(idx) = region.allowed.iter().position(|&p| p == caller_pid) {
            region.allowed.swap_remove(idx);
        }

        // If no mappings remain, deallocate
        if region.mapped_in.is_empty() {
            let (_, region) = regions.swap_remove(pos);
            if let Some(layout) = region.layout {
                unsafe { dealloc(region.phys_addr as *mut u8, layout); }
            }
        }

        true
    })
}

/// Clean up all shared memory state for an exiting process.
/// Unmaps from all regions, frees owned regions, removes from allowed lists.
pub fn cleanup_process(pid: Pid, pml4: *mut u64) {
    with_regions_mut(|regions| {
        // Unmap this process from all regions it has mapped
        for (_, region) in regions.iter_mut() {
            if let Some(pos) = region.mapped_in.iter().position(|&(p, _)| p == pid) {
                paging::unmap_user(pml4, region.phys_addr, region.size);
                region.mapped_in.swap_remove(pos);
            }
            if let Some(pos) = region.allowed.iter().position(|&p| p == pid) {
                region.allowed.swap_remove(pos);
            }
        }

        // Free regions owned by this process (unmapping all other processes),
        // and deallocate any regions left with no mappings (owner already released).
        let mut i = 0;
        while i < regions.len() {
            if regions[i].1.owner_pid == pid {
                let (_, region) = regions.swap_remove(i);
                for &(_, mapped_pml4) in &region.mapped_in {
                    paging::unmap_user(mapped_pml4, region.phys_addr, region.size);
                }
                if let Some(layout) = region.layout {
                    unsafe { dealloc(region.phys_addr as *mut u8, layout); }
                }
            } else if regions[i].1.mapped_in.is_empty() {
                let (_, region) = regions.swap_remove(i);
                if let Some(layout) = region.layout {
                    unsafe { dealloc(region.phys_addr as *mut u8, layout); }
                }
            } else {
                i += 1;
            }
        }
    });
}
