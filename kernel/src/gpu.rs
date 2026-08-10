use alloc::boxed::Box;
use toyos_abi::syscall::SyscallError;
use crate::object::shm::Region;
use crate::sync::Lock;

pub const FLAG_HARDWARE_CURSOR: u32 = 1 << 0;

/// What a display driver publishes about its screen.
///
/// The three regions are physical ranges and their memory types, not handles: a
/// fresh [`SharedMemObject`] is minted over each one per claim, because an
/// object whose handle count has reached zero is retired for good while the
/// scanout under it outlives every compositor the machine runs.
///
/// [`SharedMemObject`]: crate::object::shm::SharedMemObject
pub struct GpuInfo {
    pub scanout: [Region; 2],
    pub cursor: Region,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: u32,
    pub flags: u32,
}

/// Hardware-agnostic GPU interface. Implement this for any display driver
/// (virtio-gpu, UEFI GOP, etc.) and register it with `gpu::register()`.
pub trait Gpu: Send {
    fn present_rect(&mut self, x: u32, y: u32, w: u32, h: u32);
    fn set_cursor(&mut self, hot_x: u32, hot_y: u32);
    fn move_cursor(&mut self, x: u32, y: u32);
    /// Width and height come straight from userspace. Implementations must
    /// refuse a resolution they cannot back rather than panic on the way.
    fn set_resolution(&mut self, width: u32, height: u32) -> Result<GpuInfo, SyscallError>;
}

static GPU: Lock<Option<Box<dyn Gpu>>> = Lock::new(None);
static INFO: Lock<Option<GpuInfo>> = Lock::new(None);

pub fn register(gpu: Box<dyn Gpu>, info: GpuInfo) {
    *INFO.lock() = Some(info);
    *GPU.lock() = Some(gpu);
}

pub fn present_rect(x: u32, y: u32, w: u32, h: u32) {
    let (x, y, w, h) = {
        let info = INFO.lock();
        let Some(info) = info.as_ref() else { return };
        let x = x.min(info.width);
        let y = y.min(info.height);
        let w = w.min(info.width.saturating_sub(x));
        let h = h.min(info.height.saturating_sub(y));
        (x, y, w, h)
    };
    if w == 0 || h == 0 { return; }
    if let Some(gpu) = GPU.lock().as_mut() {
        gpu.present_rect(x, y, w, h);
    }
}

pub fn set_cursor(hot_x: u32, hot_y: u32) {
    if let Some(gpu) = GPU.lock().as_mut() {
        gpu.set_cursor(hot_x, hot_y);
    }
}

pub fn set_resolution(width: u32, height: u32) -> Result<GpuInfo, SyscallError> {
    let new_info = {
        let mut gpu = GPU.lock();
        let gpu = gpu.as_mut().ok_or(SyscallError::NotSupported)?;
        // A driver that honours this allocates a new framebuffer and frees the
        // old one, so anything caching the address would be left writing into
        // reallocated physical memory. Blind the panic console for the window;
        // worst case it has no screen, never a wild write.
        crate::drivers::panic_console::detach();
        let result = gpu.set_resolution(width, height);
        if result.is_err() {
            crate::drivers::panic_console::rearm();
        }
        result?
    };
    let mut info = INFO.lock();
    *info = Some(GpuInfo {
        scanout: new_info.scanout.clone(),
        cursor: new_info.cursor.clone(),
        width: new_info.width,
        height: new_info.height,
        stride: new_info.stride,
        pixel_format: new_info.pixel_format,
        flags: new_info.flags,
    });
    Ok(new_info)
}

pub fn move_cursor(x: u32, y: u32) {
    let (max_x, max_y) = {
        let info = INFO.lock();
        match info.as_ref() {
            Some(i) => (i.width, i.height),
            None => return,
        }
    };
    if let Some(gpu) = GPU.lock().as_mut() {
        gpu.move_cursor(x.min(max_x), y.min(max_y));
    }
}
