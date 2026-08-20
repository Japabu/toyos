use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU8, Ordering};

use super::MAX_HEAP_ALLOC;
use super::PHYS_OFFSET;
use super::PAGE_2M;
use super::pmm;

/// DESIGN RULE: nothing this type does may panic.
///
/// Every method here runs inside `KernelAllocator`'s `dlmalloc.lock()`, in the
/// middle of dlmalloc mutating its own chunk and segment lists, and the kernel
/// does not unwind — so a panic from in here abandons the heap in whatever
/// state it was in, with the lock held forever. The CPU that recovers from
/// that panic then spins `Lock::lock` on its next `alloc` or `free`, and the
/// machine goes quiet.
///
/// So a size this source cannot back is a `null`, not an assert: dlmalloc
/// hands the null back to the caller with its structures consistent, the lock
/// drops, and whatever the caller does about it — `handle_alloc_error`, an
/// `Option` — happens outside. The fail-fast for a caller asking the heap for
/// page-scale memory is [`MAX_HEAP_ALLOC`], checked in `KernelAllocator::alloc`
/// *before* the lock is taken.
///
/// `pmm::alloc_page` is the only thing reached from here, and it is
/// panic-free by the same rule.
struct KernelPageSource;

unsafe impl dlmalloc::Allocator for KernelPageSource {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        if size > PAGE_2M as usize {
            return (core::ptr::null_mut(), 0, 0);
        }
        if let Some(page) = pmm::alloc_page(pmm::Category::KernelHeap) {
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
                // Before the lock, deliberately. This is the ceiling every
                // bound upstream of the heap is derived against, and it used
                // to be enforced one level down in `KernelPageSource::alloc`
                // — inside `dlmalloc.lock()`, which is where a panic costs
                // the machine rather than the process.
                //
                // `MAX_HEAP_ALLOC` rather than whatever dlmalloc's padding
                // happens to permit, so the documented number is the enforced
                // number — the way `MAX_HANDLES` and `MAX_USER_STR` are at their
                // own primitives. Measured: 2,097,152 asks the page source
                // for 2,162,688, which it cannot back.
                //
                // Being past this is sufficient for a request to fail and not
                // necessary, which is why the page source is total rather
                // than merely unreachable: 2,093,056 with 4096-byte alignment
                // satisfies the check and still asks for 2,162,688, because
                // `memalign` pads by the alignment first.
                assert!(layout.size() <= MAX_HEAP_ALLOC,
                    "GlobalAlloc: {} bytes exceeds MAX_HEAP_ALLOC ({}) — a caller is using alloc for page-scale memory",
                    layout.size(), MAX_HEAP_ALLOC);
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
