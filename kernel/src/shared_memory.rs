use alloc::alloc::{alloc_zeroed, dealloc};
use alloc::vec::Vec;
use core::alloc::Layout;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::paging::{self, PAGE_2M};
use crate::sync::Lock;

struct SharedRegion {
    phys_addr: u64,
    size: u64,
    owner_pid: u32,                  // u32::MAX for kernel-owned
    allowed: Vec<u32>,                // PIDs allowed to map
    mapped_in: Vec<(u32, *mut u64)>,  // (PID, PML4) for processes that have mapped it
    layout: Option<Layout>,           // for freeing; None = kernel-owned
}

static REGIONS: Lock<Option<Vec<(u32, SharedRegion)>>> = Lock::new(None);
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);

pub fn init() {
    *REGIONS.lock() = Some(Vec::new());
}

fn next_token() -> u32 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// Allocate 2MB-aligned shared memory. Maps it as USER in the owner's page tables.
/// Returns (token, physical address).
pub fn alloc(size: u64, owner_pid: u32, owner_pml4: *mut u64) -> (u32, u64) {
    let aligned_size = ((size + PAGE_2M - 1) & !(PAGE_2M - 1)) as usize;
    let layout = Layout::from_size_align(aligned_size, PAGE_2M as usize).unwrap();
    let ptr = unsafe { alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "shared_memory: allocation failed");
    let phys_addr = ptr as u64;

    paging::map_user_in(owner_pml4, phys_addr, aligned_size as u64);

    let token = next_token();
    let mut guard = REGIONS.lock();
    let regions = guard.as_mut().expect("shared_memory not initialized");
    regions.push((token, SharedRegion {
        phys_addr,
        size: aligned_size as u64,
        owner_pid,
        allowed: alloc::vec![owner_pid],
        mapped_in: alloc::vec![(owner_pid, owner_pml4)],
        layout: Some(layout),
    }));

    (token, phys_addr)
}

/// Register an existing kernel-owned allocation as a shared region.
/// Used for GPU framebuffers. Not freeable, no initial owner.
pub fn register(phys_addr: u64, size: u64) -> u32 {
    let token = next_token();
    let mut guard = REGIONS.lock();
    let regions = guard.as_mut().expect("shared_memory not initialized");
    regions.push((token, SharedRegion {
        phys_addr,
        size,
        owner_pid: u32::MAX,
        allowed: Vec::new(),
        mapped_in: Vec::new(),
        layout: None,
    }));
    token
}

/// Grant a process permission to map a shared region.
/// Caller must be the owner (or kernel-owned regions can be granted by anyone with the token).
pub fn grant(token: u32, caller_pid: u32, target_pid: u32) -> bool {
    let mut guard = REGIONS.lock();
    let regions = guard.as_mut().expect("shared_memory not initialized");
    let Some((_, region)) = regions.iter_mut().find(|(t, _)| *t == token) else {
        return false;
    };
    // Only owner can grant (kernel-owned regions: any granted process can re-grant)
    if region.owner_pid != u32::MAX && region.owner_pid != caller_pid {
        return false;
    }
    if !region.allowed.contains(&target_pid) {
        region.allowed.push(target_pid);
    }
    true
}

/// Map a shared region into the caller's address space.
/// Returns the physical address, or None if not allowed.
pub fn map(token: u32, caller_pid: u32, caller_pml4: *mut u64) -> Option<u64> {
    let mut guard = REGIONS.lock();
    let regions = guard.as_mut().expect("shared_memory not initialized");
    let (_, region) = regions.iter_mut().find(|(t, _)| *t == token)?;

    if !region.allowed.contains(&caller_pid) {
        return None;
    }

    if !region.mapped_in.iter().any(|&(pid, _)| pid == caller_pid) {
        paging::map_user_in(caller_pml4, region.phys_addr, region.size);
        region.mapped_in.push((caller_pid, caller_pml4));
    }

    Some(region.phys_addr)
}

/// Free a shared region owned by the caller. Unmaps from all processes that mapped it.
pub fn free(token: u32, caller_pid: u32) -> bool {
    let mut guard = REGIONS.lock();
    let regions = guard.as_mut().expect("shared_memory not initialized");
    let Some(pos) = regions.iter().position(|(t, _)| *t == token) else {
        return false;
    };
    if regions[pos].1.owner_pid != caller_pid {
        return false;
    }
    let (_, region) = regions.swap_remove(pos);

    // Unmap from all processes (safe: we hold REGIONS lock so no concurrent cleanup_process)
    for &(_, pml4) in &region.mapped_in {
        paging::unmap_user(pml4, region.phys_addr, region.size);
    }

    if let Some(layout) = region.layout {
        unsafe { dealloc(region.phys_addr as *mut u8, layout); }
    }
    true
}

/// Clean up all shared memory state for an exiting process.
/// Unmaps from all regions, frees owned regions, removes from allowed lists.
pub fn cleanup_process(pid: u32, pml4: *mut u64) {
    let mut guard = REGIONS.lock();
    let regions = guard.as_mut().expect("shared_memory not initialized");

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

    // Free regions owned by this process, unmapping from all other processes
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
        } else {
            i += 1;
        }
    }
}
