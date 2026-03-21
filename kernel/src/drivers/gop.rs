use alloc::alloc::{alloc_zeroed, Layout};
use alloc::boxed::Box;

use crate::mm::{PAGE_2M, align_2m, DirectMap};
use crate::gpu::{Gpu, GpuInfo};
use crate::log;
use crate::shared_memory;

struct GopGpu;

impl Gpu for GopGpu {
    fn present_rect(&mut self, _x: u32, _y: u32, _w: u32, _h: u32) {
        // GOP framebuffer is memory-mapped — writes are immediately visible.
    }

    fn set_cursor(&mut self, _hot_x: u32, _hot_y: u32) {}
    fn move_cursor(&mut self, _x: u32, _y: u32) {}

    fn set_resolution(&mut self, _width: u32, _height: u32) -> Result<GpuInfo, ()> {
        Err(()) // GOP cannot change resolution after UEFI boot services exit
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
    let aligned_size = align_2m(size as usize) as u64;
    crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(addr, aligned_size);

    let token0 = shared_memory::register(DirectMap::new(addr), aligned_size);
    let token1 = shared_memory::register(DirectMap::new(addr), aligned_size);
    log!("GOP: {}x{} stride={} format={} at {:#x} tokens=[{:?}, {:?}]",
        width, height, stride, pixel_format, addr, token0, token1);

    let cursor_bytes = (64 * 64 * 4) as usize;
    let cursor_aligned = align_2m(cursor_bytes);
    let cursor_layout = Layout::from_size_align(cursor_aligned, PAGE_2M as usize).unwrap();
    let cursor_ptr = unsafe { alloc_zeroed(cursor_layout) };
    assert!(!cursor_ptr.is_null(), "GOP: cursor alloc failed");
    let cursor_token = shared_memory::register(
        DirectMap::new(DirectMap::phys_of(cursor_ptr)),
        cursor_aligned as u64,
    );

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
