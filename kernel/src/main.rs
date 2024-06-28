#![no_std]
#![no_main]
extern crate alloc;
use core::{alloc::{GlobalAlloc, Layout}, cell::UnsafeCell, ptr::null_mut, sync::atomic::{AtomicUsize, Ordering::Relaxed}};

use alloc::{format, string::String, vec::Vec};
mod serial;

#[repr(C)]
pub struct KernelArgs {
    pub memory_map_addr: u64,
    pub memory_map_size: u64,
    pub kernel_memory_addr: u64,
    pub kernel_memory_size: u64,
    pub kernel_stack_addr: u64,
    pub kernel_stack_size: u64,
}

#[repr(C)]
#[derive(Debug)]
struct MemoryMapEntry {
    pub uefi_type: u32,
    pub start: u64,
    pub end: u64,
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Custom panic handling code goes here
    loop {}
}

fn is_cool(entry : &&MemoryMapEntry) -> bool {
    match entry.uefi_type{
        1..=4 => true,
        7 => true,
        _ => false
    }
}

//Üfi Die 1, Die 2, Die 3, Die 4, Die 7 Das wars. Die duerfen wir verwenden. Mit Ausnahme von
//Adressen die von @kernel_memory_addr bis @kernel_memory_addr+@kernel_memory_size gehen.
#[no_mangle]
pub unsafe extern "sysv64" fn _start(kernel_args: KernelArgs) -> ! {
    let x = "Hello from Kernel!";
    serial::init_serial();
    serial::println(x);
    serial::println(&format!("Address: {}, Size: {}", kernel_args.memory_map_addr, kernel_args.memory_map_size));
    let y = kernel_args.memory_map_size as usize / core::mem::size_of::<MemoryMapEntry>();
    let maps = Vec::from_raw_parts(kernel_args.memory_map_addr as *mut MemoryMapEntry, y, y);
    serial::println(&format!("{:?}", maps));
    serial::println(&format!("Total available memory: {}", sanitize_uefi(maps)));
    loop {}
}

fn sanitize_uefi(entries : Vec<MemoryMapEntry>) -> u64{
    entries.iter().filter(is_cool).map(|entry| entry.end - entry.start).sum()
} 

const ARENA_SIZE: usize = 128 * 1024;
const MAX_SUPPORTED_ALIGN: usize = 4096;
#[repr(C, align(4096))] // 4096 == MAX_SUPPORTED_ALIGN
struct SimpleAllocator {
    arena: UnsafeCell<[u8; ARENA_SIZE]>,
    remaining: AtomicUsize, // we allocate from the top, counting down
}

#[global_allocator]
static ALLOCATOR: SimpleAllocator = SimpleAllocator {
    arena: UnsafeCell::new([0x55; ARENA_SIZE]),
    remaining: AtomicUsize::new(ARENA_SIZE),
};

unsafe impl Sync for SimpleAllocator {}

unsafe impl GlobalAlloc for SimpleAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();

        // `Layout` contract forbids making a `Layout` with align=0, or align not power of 2.
        // So we can safely use a mask to ensure alignment without worrying about UB.
        let align_mask_to_round_down = !(align - 1);

        if align > MAX_SUPPORTED_ALIGN {
            return null_mut();
        }
        let mut allocated = 0;
        if self
            .remaining
            .fetch_update(Relaxed, Relaxed, |mut remaining| {
                if size > remaining {
                    return None;
                }
                remaining -= size;
                remaining &= align_mask_to_round_down;
                allocated = remaining;
                Some(remaining)
            })
            .is_err()
        {
            return null_mut();
        };
        self.arena.get().cast::<u8>().add(allocated)
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}
