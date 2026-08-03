use core::cell::Cell;
use core::ptr;

pub use font::Color;

#[derive(Clone, Copy, PartialEq)]
enum PixelFormat {
    Rgb,
    Bgr,
}

impl PixelFormat {
    fn from_raw(raw: u32) -> Self {
        if raw == 0 { Self::Rgb } else { Self::Bgr }
    }

    #[inline]
    fn encode(self, color: Color) -> [u8; 4] {
        match self {
            Self::Rgb => [color.r, color.g, color.b, 0],
            Self::Bgr => [color.b, color.g, color.r, 0],
        }
    }
}

/// The scanout, which may only be written to.
///
/// [`Framebuffer`] reads as freely as it writes, which is what a pixel surface
/// in system RAM should be. A GOP scanout is not one. The kernel programs
/// neither MTRRs nor PAT, so every mapping of it is PAT entry 0 and defers to
/// firmware's MTRR for the range — WC on most UEFI machines, UC on some — and
/// under either of those a *read* is uncached while a write is a posted store.
/// Composing against the panel therefore costs a bus transaction per byte read
/// back out of it. QEMU's framebuffer is host RAM, where none of that exists,
/// so no test can show it.
///
/// The property is that nothing reads the panel, and this type is how it is
/// held rather than promised: `Screen` hands out no pointer and returns no
/// pixel, so there is nothing to read through. Its one drawing operation takes
/// pixels that were composed somewhere else.
pub struct Screen {
    buf: *mut u8,
    len: usize,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
    /// What this mapping has cost, for a machine whose only instrument is the
    /// panel it is being asked about.
    written: Cell<u64>,
    blits: Cell<u64>,
}

impl Screen {
    pub fn new(buf: *mut u8, width: usize, height: usize, stride: usize, pixel_format: u32) -> Self {
        debug_assert!(!buf.is_null());
        debug_assert!(stride >= width);
        Self {
            buf,
            len: stride * height * 4,
            width,
            height,
            stride,
            pixel_format: PixelFormat::from_raw(pixel_format),
            written: Cell::new(0),
            blits: Cell::new(0),
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pixel_format_raw(&self) -> u32 {
        if self.pixel_format == PixelFormat::Rgb { 0 } else { 1 }
    }

    /// Bytes handed to the panel, and the number of blits that carried them.
    pub fn traffic(&self) -> (u64, u64) {
        (self.written.get(), self.blits.get())
    }

    /// Copy `h` rows of `w` pixels from `src` to `(x, y)`, one memcpy per
    /// scanline. `src_stride` is `src`'s width in pixels.
    ///
    /// Rows clipped away by the screen's height are not copied, and a `src`
    /// too short for the rectangle asked for is a caller bug rather than a
    /// partial paint.
    pub fn blit(&self, x: usize, y: usize, w: usize, h: usize, src_stride: usize, src: &[u8]) {
        let w = w.min(self.width.saturating_sub(x));
        let h = h.min(self.height.saturating_sub(y));
        if w == 0 || h == 0 {
            return;
        }
        let row_bytes = w * 4;
        let src_row_bytes = src_stride * 4;
        assert!(
            src.len() >= (h - 1) * src_row_bytes + row_bytes,
            "Screen::blit: {}-byte source for {w}x{h} at stride {src_stride}",
            src.len()
        );
        for dy in 0..h {
            let dst_offset = ((y + dy) * self.stride + x) * 4;
            debug_assert!(dst_offset + row_bytes <= self.len);
            unsafe {
                ptr::copy_nonoverlapping(
                    src.as_ptr().add(dy * src_row_bytes),
                    self.buf.add(dst_offset),
                    row_bytes,
                );
            }
        }
        self.written.set(self.written.get() + (row_bytes * h) as u64);
        self.blits.set(self.blits.get() + 1);
    }
}

pub struct Framebuffer {
    buf: *mut u8,
    len: usize,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
}

impl Framebuffer {
    pub fn new(buf: *mut u8, width: usize, height: usize, stride: usize, pixel_format: u32) -> Self {
        debug_assert!(!buf.is_null());
        debug_assert!(stride >= width);
        let len = stride * height * 4;
        Self {
            buf,
            len,
            width,
            height,
            stride,
            pixel_format: PixelFormat::from_raw(pixel_format),
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn pixel_format_raw(&self) -> u32 {
        if self.pixel_format == PixelFormat::Rgb { 0 } else { 1 }
    }

    pub fn ptr(&self) -> *mut u8 {
        self.buf
    }

    #[inline]
    fn encode_pixel(&self, color: Color) -> [u8; 4] {
        self.pixel_format.encode(color)
    }

    #[inline]
    pub fn get_pixel(&self, x: usize, y: usize) -> Color {
        if x < self.width && y < self.height {
            let offset = (y * self.stride + x) * 4;
            debug_assert!(offset + 4 <= self.len);
            let pixel = unsafe { core::slice::from_raw_parts(self.buf.add(offset), 4) };
            match self.pixel_format {
                PixelFormat::Rgb => Color { r: pixel[0], g: pixel[1], b: pixel[2] },
                PixelFormat::Bgr => Color { r: pixel[2], g: pixel[1], b: pixel[0] },
            }
        } else {
            Color { r: 0, g: 0, b: 0 }
        }
    }

    #[inline]
    pub fn put_pixel(&self, x: usize, y: usize, color: Color) {
        if x < self.width && y < self.height {
            let offset = (y * self.stride + x) * 4;
            debug_assert!(offset + 4 <= self.len);
            let pixel = self.encode_pixel(color);
            unsafe {
                ptr::copy_nonoverlapping(pixel.as_ptr(), self.buf.add(offset), 4);
            }
        }
    }

    /// Fill a row of pixels with a 4-byte pattern using doubling memcpy.
    unsafe fn fill_row(dst: *mut u8, pixel: &[u8; 4], count: usize) {
        if count == 0 {
            return;
        }
        ptr::copy_nonoverlapping(pixel.as_ptr(), dst, 4);
        let total_bytes = count * 4;
        let mut filled = 4usize;
        while filled < total_bytes {
            let chunk = filled.min(total_bytes - filled);
            ptr::copy_nonoverlapping(dst, dst.add(filled), chunk);
            filled += chunk;
        }
    }

    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }
        let actual_w = x_end - x;
        let row_bytes = actual_w * 4;
        let pixel = self.encode_pixel(color);

        unsafe {
            let first_row = self.buf.add((y * self.stride + x) * 4);
            debug_assert!((y * self.stride + x) * 4 + row_bytes <= self.len);
            Self::fill_row(first_row, &pixel, actual_w);
            for dy in 1..(y_end - y) {
                let dst_offset = ((y + dy) * self.stride + x) * 4;
                debug_assert!(dst_offset + row_bytes <= self.len);
                let dst = self.buf.add(dst_offset);
                ptr::copy_nonoverlapping(first_row, dst, row_bytes);
            }
        }
    }

    /// Blit a buffer to a region of the framebuffer (row-by-row memcpy).
    /// `src_stride` is the width of the source buffer (may differ from `w` during resize).
    pub fn blit(&self, x: usize, y: usize, w: usize, h: usize, src_stride: usize, buffer: &[u8]) {
        let blit_w = w.min(self.width.saturating_sub(x));
        if blit_w == 0 {
            return;
        }
        let copy_bytes = blit_w * 4;
        let src_row_bytes = src_stride * 4;
        for dy in 0..h {
            let sy = y + dy;
            if sy >= self.height {
                break;
            }
            let src_offset = dy * src_row_bytes;
            let dst_offset = (sy * self.stride + x) * 4;
            debug_assert!(dst_offset + copy_bytes <= self.len);
            unsafe {
                ptr::copy_nonoverlapping(
                    buffer.as_ptr().add(src_offset),
                    self.buf.add(dst_offset),
                    copy_bytes,
                );
            }
        }
    }

    pub fn clear(&self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    pub fn scroll_up(&self, pixel_rows: usize, bg: Color) {
        if pixel_rows >= self.height {
            self.clear(bg);
            return;
        }
        let row_bytes = self.stride * 4;
        unsafe {
            let src = self.buf.add(pixel_rows * row_bytes);
            let dst = self.buf;
            let count = (self.height - pixel_rows) * row_bytes;
            ptr::copy(src, dst, count);
        }
        let fill_y = self.height - pixel_rows;
        self.fill_rect(0, fill_y, self.width, pixel_rows, bg);
    }

    pub fn scroll_down(&self, pixel_rows: usize, bg: Color) {
        if pixel_rows >= self.height {
            self.clear(bg);
            return;
        }
        let row_bytes = self.stride * 4;
        unsafe {
            let src = self.buf;
            let dst = self.buf.add(pixel_rows * row_bytes);
            let count = (self.height - pixel_rows) * row_bytes;
            ptr::copy(src, dst, count);
        }
        self.fill_rect(0, 0, self.width, pixel_rows, bg);
    }
}

impl font::Canvas for Framebuffer {
    fn put_pixel(&self, x: usize, y: usize, color: Color) {
        self.put_pixel(x, y, color);
    }
}
