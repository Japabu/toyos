use alloc::alloc::{alloc_zeroed, Layout};
use alloc::boxed::Box;

use crate::arch::paging::{self, PAGE_2M};
use crate::gpu::{Gpu, GpuInfo};
use crate::log;
use crate::shared_memory;

struct GopGpu;

impl Gpu for GopGpu {
    fn present_rect(&mut self, _x: u32, _y: u32, _w: u32, _h: u32) {
        // GOP framebuffer is memory-mapped — writes are immediately visible.
    }

    fn set_cursor(&mut self, _hot_x: u32, _hot_y: u32) {
        // No hardware cursor on GOP.
    }

    fn move_cursor(&mut self, _x: u32, _y: u32) {
        // No hardware cursor on GOP.
    }
}

/// Initialize the UEFI GOP framebuffer driver.
/// `addr` is the physical address of the framebuffer from UEFI.
pub fn init(
    addr: u64,
    size: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
) -> (Box<dyn Gpu>, GpuInfo) {
    // Map the GOP framebuffer into kernel address space
    let aligned_size = paging::align_2m(size as usize) as u64;
    paging::map_kernel(addr, aligned_size);

    // Register framebuffer as shared memory (same buffer for both tokens)
    let token0 = shared_memory::register(addr, aligned_size);
    let token1 = shared_memory::register(addr, aligned_size);
    log!("GOP: {}x{} stride={} format={} at {:#x} tokens=[{:?}, {:?}]",
        width, height, stride, pixel_format, addr, token0, token1);

    // Allocate cursor buffer (unused but required by FramebufferInfo)
    let cursor_bytes = (64 * 64 * 4) as usize;
    let cursor_aligned = paging::align_2m(cursor_bytes);
    let cursor_layout = Layout::from_size_align(cursor_aligned, PAGE_2M as usize).unwrap();
    let cursor_ptr = unsafe { alloc_zeroed(cursor_layout) };
    assert!(!cursor_ptr.is_null(), "GOP: cursor alloc failed");
    let cursor_token = shared_memory::register(cursor_ptr as u64, cursor_aligned as u64);

    let info = GpuInfo {
        tokens: [token0, token1],
        cursor_token,
        width,
        height,
        stride,
        pixel_format,
        flags: 0,
    };

    (Box::new(GopGpu), info)
}
