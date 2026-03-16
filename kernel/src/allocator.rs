// Kernel heap allocator — slab allocator for small objects on top of the
// physical memory manager (pmm.rs buddy allocator).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::MemoryMapEntry;
use crate::pmm::{self, BuddyAllocator};

pub use crate::pmm::Region;

const PAGE_SIZE: usize = 4096;
const SLAB_CLASSES: usize = 9; // 8, 16, 32, 64, 128, 256, 512, 1024, 2048

// --- Phase 1: Early bump allocator (static buffer, used before paging) ---

const EARLY_SIZE: usize = 512 * 1024; // 512KB — enough for paging::init() page tables

#[repr(C, align(4096))]
struct EarlyBuffer([u8; EARLY_SIZE]);

/// Boot-only cell. Only accessed during single-threaded BSP boot (before SMP).
#[repr(transparent)]
struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(val: T) -> Self { Self(UnsafeCell::new(val)) }
    /// # Safety: only call during single-threaded boot phase.
    unsafe fn get(&self) -> *mut T { self.0.get() }
}

static EARLY_BUF: BootCell<EarlyBuffer> = BootCell::new(EarlyBuffer([0; EARLY_SIZE]));
static EARLY_POS: BootCell<usize> = BootCell::new(0);

// --- Slab allocator ---

struct SlabAllocator {
    free_lists: [*mut u8; SLAB_CLASSES],
}

impl SlabAllocator {
    const fn new() -> Self {
        Self {
            free_lists: [null_mut(); SLAB_CLASSES],
        }
    }

    fn alloc(&mut self, class: usize, buddy: &mut BuddyAllocator) -> *mut u8 {
        let head = self.free_lists[class];
        if !head.is_null() {
            let next = unsafe { *(head as *const *mut u8) };
            self.free_lists[class] = next;
            return head;
        }

        let page = buddy.alloc(0);
        if page.is_null() {
            return null_mut();
        }

        let obj_size = 8usize << class;
        let count = PAGE_SIZE / obj_size;

        // Link objects [1..count) into free list, return object 0
        for i in (1..count).rev() {
            let obj = unsafe { page.add(i * obj_size) };
            unsafe { *(obj as *mut *mut u8) = self.free_lists[class]; }
            self.free_lists[class] = obj;
        }

        page
    }

    fn free(&mut self, ptr: *mut u8, class: usize) {
        unsafe { *(ptr as *mut *mut u8) = self.free_lists[class]; }
        self.free_lists[class] = ptr;
    }
}

// --- Combined allocator ---

const PHASE_UNINIT: u8 = 0;
const PHASE_EARLY: u8 = 1;
const PHASE_READY: u8 = 2;

struct KernelAllocator {
    ticket: AtomicU32,
    now: AtomicU32,
    buddy: UnsafeCell<BuddyAllocator>,
    slab: UnsafeCell<SlabAllocator>,
    phase: UnsafeCell<u8>,
}

unsafe impl Sync for KernelAllocator {}

impl KernelAllocator {
    const fn new() -> Self {
        Self {
            ticket: AtomicU32::new(0),
            now: AtomicU32::new(0),
            buddy: UnsafeCell::new(BuddyAllocator::new()),
            slab: UnsafeCell::new(SlabAllocator::new()),
            phase: UnsafeCell::new(PHASE_UNINIT),
        }
    }

    /// Acquire the allocator lock, disabling interrupts to prevent deadlock.
    /// Returns the saved RFLAGS for restoring interrupt state on release.
    fn acquire(&self) -> u64 {
        let rflags: u64;
        unsafe { core::arch::asm!("pushfq; pop {}", out(reg) rflags); }
        unsafe { core::arch::asm!("cli"); }
        let my_ticket = self.ticket.fetch_add(1, Ordering::Relaxed);
        let mut spins = 0u64;
        while self.now.load(Ordering::Acquire) != my_ticket {
            core::hint::spin_loop();
            spins += 1;
            if spins == 10_000_000 {
                unsafe {
                    for &b in b"DEADLOCK ticket=" {
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
                    }
                    for i in (0..8).rev() {
                        let nibble = ((my_ticket >> (i * 4)) & 0xF) as u8;
                        let c = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") c);
                    }
                    for &b in b" now=" {
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b);
                    }
                    let now_val = self.now.load(Ordering::Relaxed);
                    for i in (0..8).rev() {
                        let nibble = ((now_val >> (i * 4)) & 0xF) as u8;
                        let c = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
                        core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") c);
                    }
                    core::arch::asm!("out dx, al", in("dx") 0x3F8u16, in("al") b'\n');
                }
                spins = 0;
            }
        }
        rflags
    }

    fn release(&self, saved_rflags: u64) {
        self.now.fetch_add(1, Ordering::Release);
        if saved_rflags & 0x200 != 0 {
            unsafe { core::arch::asm!("sti"); }
        }
    }
}

fn size_class(size: usize) -> usize {
    if size <= 8 { return 0; }
    size.next_power_of_two().trailing_zeros() as usize - 3
}

fn order_for(size: usize, align: usize) -> usize {
    let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    let size_order = if pages <= 1 { 0 } else { pages.next_power_of_two().trailing_zeros() as usize };
    let align_order = if align <= PAGE_SIZE {
        0
    } else {
        (align / PAGE_SIZE).next_power_of_two().trailing_zeros() as usize
    };
    size_order.max(align_order)
}

unsafe fn early_alloc(layout: Layout) -> *mut u8 {
    let buf = EARLY_BUF.get().cast::<u8>();
    let pos = &mut *EARLY_POS.get();
    let aligned = (*pos + layout.align() - 1) & !(layout.align() - 1);
    let new_pos = aligned + layout.size();
    if new_pos > EARLY_SIZE {
        return null_mut();
    }
    *pos = new_pos;
    buf.add(aligned)
}

fn is_early_ptr(ptr: *mut u8) -> bool {
    let buf_start = EARLY_BUF.0.get() as usize;
    let p = ptr as usize;
    p >= buf_start && p < buf_start + EARLY_SIZE
}

pub fn early_buf_range() -> (u64, u64) {
    let start = EARLY_BUF.0.get() as u64;
    (start, start + EARLY_SIZE as u64)
}

unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let phase = *self.phase.get();
        if phase == PHASE_UNINIT {
            return null_mut();
        }
        if phase == PHASE_EARLY {
            return early_alloc(layout);
        }

        let flags = self.acquire();
        let buddy = &mut *self.buddy.get();
        let slab = &mut *self.slab.get();
        let effective = layout.size().max(layout.align());
        let result = if effective <= 2048 {
            slab.alloc(size_class(effective), buddy)
        } else {
            buddy.alloc(order_for(layout.size(), layout.align()))
        };
        self.release(flags);
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Early allocations are permanent (page tables etc.) — never freed
        if is_early_ptr(ptr) {
            return;
        }

        let flags = self.acquire();
        let buddy = &mut *self.buddy.get();
        let slab = &mut *self.slab.get();
        let effective = layout.size().max(layout.align());
        if effective <= 2048 {
            slab.free(ptr, size_class(effective));
        } else {
            let order = order_for(layout.size(), layout.align());
            buddy.free((ptr as u64 - crate::PHYS_OFFSET) / PAGE_SIZE as u64, order);
        }
        self.release(flags);
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator::new();

// --- Initialization ---

/// Phase 1: Enable the early bump allocator.
/// Called before paging::init(). Uses a static buffer — no writes to physical RAM.
pub unsafe fn init(
    _entries: &[MemoryMapEntry],
    _reserved_regions: &[Region],
) {
    *ALLOCATOR.phase.get() = PHASE_EARLY;
}

/// Phase 2: Switch to the buddy + slab allocator.
/// Called after paging::init() has set up the high-half direct map (all physical RAM writable).
pub unsafe fn init_buddy(
    entries: &[MemoryMapEntry],
    reserved_regions: &[Region],
) {
    let buddy = &mut *ALLOCATOR.buddy.get();
    pmm::init_buddy(buddy, entries, reserved_regions);
    *ALLOCATOR.phase.get() = PHASE_READY;
}

/// Returns (total_usable_bytes, used_bytes).
pub fn memory_stats() -> (u64, u64) {
    let flags = ALLOCATOR.acquire();
    let buddy = unsafe { &*ALLOCATOR.buddy.get() };
    let stats = buddy.memory_stats();
    ALLOCATOR.release(flags);
    stats
}
