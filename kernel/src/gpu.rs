use alloc::boxed::Box;
use crate::shared_memory::SharedToken;
use crate::sync::Lock;

pub const FLAG_HARDWARE_CURSOR: u32 = 1 << 0;

pub struct GpuInfo {
    pub tokens: [SharedToken; 2],
    pub cursor_token: SharedToken,
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
    fn set_resolution(&mut self, width: u32, height: u32) -> Result<GpuInfo, ()>;
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

pub fn set_resolution(width: u32, height: u32) -> Result<GpuInfo, ()> {
    let new_info = {
        let mut gpu = GPU.lock();
        let gpu = gpu.as_mut().ok_or(())?;
        gpu.set_resolution(width, height)?
    };
    let mut info = INFO.lock();
    *info = Some(GpuInfo {
        tokens: new_info.tokens,
        cursor_token: new_info.cursor_token,
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
