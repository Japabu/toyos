use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU8, Ordering};

use super::PHYS_OFFSET;
use super::PAGE_2M;
use super::pmm;

struct KernelPageSource;

unsafe impl dlmalloc::Allocator for KernelPageSource {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        assert!(size <= PAGE_2M as usize,
            "GlobalAlloc: dlmalloc asked for {} bytes — a caller is using alloc for page-scale memory", size);
        if let Some(page) = pmm::alloc_page() {
            let ptr = page.direct_map().as_mut_ptr::<u8>();
            core::mem::forget(page); // dlmalloc manages the lifetime
            (ptr, PAGE_2M as usize, 0)
        } else {
            (core::ptr::null_mut(), 0, 0)
        }
    }

    fn remap(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize, _can_move: bool) -> *mut u8 {
        core::ptr::null_mut()
    }

    fn free_part(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize) -> bool {
        false
    }

    fn free(&self, ptr: *mut u8, _size: usize) -> bool {
        let phys = ptr as u64 - PHYS_OFFSET;
        drop(pmm::PhysPage::from_raw(phys));
        true
    }

    fn can_release_part(&self, _flags: u32) -> bool {
        false
    }

    fn allocates_zeros(&self) -> bool {
        true
    }

    fn page_size(&self) -> usize {
        PAGE_2M as usize
    }
}

struct KernelAllocator {
    dlmalloc: Lock<dlmalloc::Dlmalloc<KernelPageSource>>,
    phase: AtomicU8,
}

const PHASE_UNINIT: u8 = 0;
const PHASE_EARLY: u8 = 1;
const PHASE_READY: u8 = 2;

use crate::sync::Lock;

impl KernelAllocator {
    const fn new() -> Self {
        Self {
            dlmalloc: Lock::new(dlmalloc::Dlmalloc::new_with_allocator(KernelPageSource)),
            phase: AtomicU8::new(PHASE_UNINIT),
        }
    }
}

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match self.phase.load(Ordering::Acquire) {
            PHASE_UNINIT => core::ptr::null_mut(),
            PHASE_EARLY => early_alloc(layout),
            _ => {
                assert!(layout.align() < PAGE_2M as usize,
                    "GlobalAlloc: {:#x} bytes with {:#x} align — use PageAlloc", layout.size(), layout.align());
                let mut dlm = self.dlmalloc.lock();
                dlm.malloc(layout.size(), layout.align())
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if is_early_ptr(ptr) { return; }
        let mut dlm = self.dlmalloc.lock();
        dlm.free(ptr, layout.size(), layout.align());
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator::new();

const EARLY_SIZE: usize = 512 * 1024;

#[repr(C, align(4096))]
struct EarlyBuffer([u8; EARLY_SIZE]);

static mut EARLY_BUF: EarlyBuffer = EarlyBuffer([0; EARLY_SIZE]);
static mut EARLY_POS: usize = 0;

unsafe fn early_alloc(layout: Layout) -> *mut u8 {
    let buf = core::ptr::addr_of_mut!(EARLY_BUF) as *mut u8;
    let aligned = (EARLY_POS + layout.align() - 1) & !(layout.align() - 1);
    let new_pos = aligned + layout.size();
    if new_pos > EARLY_SIZE {
        return core::ptr::null_mut();
    }
    EARLY_POS = new_pos;
    buf.add(aligned)
}

fn is_early_ptr(ptr: *mut u8) -> bool {
    let buf_start = core::ptr::addr_of!(EARLY_BUF) as usize;
    let p = ptr as usize;
    p >= buf_start && p < buf_start + EARLY_SIZE
}

/// Phase 1: Enable early bump allocator (before paging).
pub(super) fn init_early() {
    ALLOCATOR.phase.store(PHASE_EARLY, Ordering::Release);
}

/// Phase 2: Switch to dlmalloc (after pmm + paging are ready).
pub(super) fn init() {
    ALLOCATOR.phase.store(PHASE_READY, Ordering::Release);
}
