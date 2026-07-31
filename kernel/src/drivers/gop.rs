use alloc::boxed::Box;

use toyos_abi::syscall::SyscallError;

use crate::mm::{PAGE_2M, align_2m_checked, DirectMap};
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

    fn set_resolution(&mut self, _width: u32, _height: u32) -> Result<GpuInfo, SyscallError> {
        // GOP cannot change resolution after UEFI boot services exit.
        Err(SyscallError::NotSupported)
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
    // Every argument here is firmware's word for the scanout, and `size` is
    // the one the kernel turns into a mapping. Two ways it lies. A size whose
    // 2 MiB round-up wraps maps a few pages and registers a shared region of
    // that length, while the compositor keeps writing `stride * height`
    // pixels. A size that is merely *smaller* than the mode does the same
    // thing without any arithmetic going wrong. Both end as writes past the
    // mapping into whatever the PMM hands out next, from a process that was
    // told the resolution.
    //
    // Boot-time and firmware-supplied, with no display either way if it is
    // wrong, so this says which number was impossible rather than returning an
    // error nothing could act on — the same call the xHCI PAGESIZE check makes.
    let needed = stride as u64 * height as u64 * 4;
    assert!(
        size >= needed,
        "GOP: firmware reports a {size}-byte framebuffer for {width}x{height} stride={stride}, \
         which needs {needed}"
    );
    let aligned_size = align_2m_checked(size as usize)
        .unwrap_or_else(|| panic!("GOP: firmware reports a {size}-byte framebuffer")) as u64;
    crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(addr, aligned_size);

    let token0 = shared_memory::register(DirectMap::from_phys(addr), aligned_size);
    let token1 = shared_memory::register(DirectMap::from_phys(addr), aligned_size);
    log!("GOP: {}x{} stride={} format={} at {:#x} tokens=[{:?}, {:?}]",
        width, height, stride, pixel_format, addr, token0, token1);

    let cursor_pages = crate::mm::pmm::alloc_contiguous(1, crate::mm::pmm::Category::Framebuffer).expect("GOP: cursor alloc failed");
    let cursor_phys = cursor_pages[0].direct_map().phys();
    let cursor_token = shared_memory::register(
        DirectMap::from_phys(cursor_phys),
        PAGE_2M,
    );
    core::mem::forget(cursor_pages); // lives forever (GPU is never torn down)

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
