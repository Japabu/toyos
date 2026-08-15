use core::cell::Cell;
use core::ptr;

use crate::Rect;

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
/// in system RAM should be. A GOP scanout is not one: the kernel maps it
/// write-combining, so a write is a posted store that merges with its
/// neighbours on the way out, while a *read* misses every cache and costs a
/// bus transaction. Composing against the panel therefore pays that per byte
/// read back. QEMU's framebuffer is host RAM, where none of it exists, so no
/// test can show it.
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
    /// Where the blits since the last [`Screen::take_damage`] landed.
    ///
    /// The surface is the one thing that sees every blit, so it is the one
    /// thing that can answer "what changed" without its owner keeping a second
    /// account that can disagree with the pixels.
    damage: Cell<Option<Rect>>,
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
            damage: Cell::new(None),
        }
    }

    /// The area every blit since the last call landed in, and `None` when
    /// there has been none.
    ///
    /// One rectangle rather than a list: what asks is a client about to name
    /// its damage to the compositor, and one message describing one box is
    /// cheaper than the pixels a second box would save at the sizes a client
    /// dirties between two frames.
    pub fn take_damage(&self) -> Option<Rect> {
        self.damage.take()
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
        // A scanout is write-combining, so the tail of the last row can sit in
        // a partly filled WC buffer until something unrelated evicts it — a
        // sliver of the previous frame left on the panel for as long as its
        // owner has nothing else to draw. SFENCE is what drains one (SDM
        // Vol. 3A §11.3.1), and this is the call that says the pixels are the
        // surface's now.
        unsafe { core::arch::x86_64::_mm_sfence() };
        self.written.set(self.written.get() + (row_bytes * h) as u64);
        self.blits.set(self.blits.get() + 1);
        let painted = Rect { x: x as u32, y: y as u32, w: w as u32, h: h as u32 };
        self.damage.set(Some(match self.damage.get() {
            Some(prev) => prev.union(painted),
            None => painted,
        }));
    }
}

/// Bytes a [`Framebuffer`] has moved through its surface since it was created.
///
/// Cumulative; a caller sampling a window subtracts its previous sample.
#[derive(Clone, Copy, Default)]
pub struct Traffic {
    /// Bytes written by the bulk paths — `blit` and `fill_rect`.
    ///
    /// `put_pixel` is not counted. It is the per-pixel path — every glyph a
    /// font draws goes through it one pixel at a time — so a counter there
    /// would tax every program that draws text to pay for one program's
    /// instrument.
    pub written: u64,
    /// Bytes read back *out of* the surface: `get_pixel`, and the row
    /// replication inside `fill_rect`.
    ///
    /// Zero for a surface that is only ever drawn into. A caller holding a
    /// scanout mapping should read this as its bill: a read of write-combining
    /// memory misses every cache, so it costs a bus transaction that a write —
    /// which merges with its neighbours — does not.
    pub read: u64,
    /// Reads of a single pixel (`get_pixel`) — each one a separate round trip.
    pub pixel_reads: u64,
    /// Reads of a whole row or region (`fill_rect`).
    pub bulk_reads: u64,
}

impl Traffic {
    /// What moved between an earlier sample and this one. Both must come from
    /// the same surface; a sample from another one underflows rather than
    /// producing a plausible figure.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            written: self.written - earlier.written,
            read: self.read - earlier.read,
            pixel_reads: self.pixel_reads - earlier.pixel_reads,
            bulk_reads: self.bulk_reads - earlier.bulk_reads,
        }
    }
}

pub struct Framebuffer {
    buf: *mut u8,
    len: usize,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
    written: Cell<u64>,
    read: Cell<u64>,
    pixel_reads: Cell<u64>,
    bulk_reads: Cell<u64>,
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
            written: Cell::new(0),
            read: Cell::new(0),
            pixel_reads: Cell::new(0),
            bulk_reads: Cell::new(0),
        }
    }

    pub fn traffic(&self) -> Traffic {
        Traffic {
            written: self.written.get(),
            read: self.read.get(),
            pixel_reads: self.pixel_reads.get(),
            bulk_reads: self.bulk_reads.get(),
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
            self.read.set(self.read.get() + 4);
            self.pixel_reads.set(self.pixel_reads.get() + 1);
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
    /// Returns how many of those copies read back out of the row.
    unsafe fn fill_row(dst: *mut u8, pixel: &[u8; 4], count: usize) -> u64 {
        if count == 0 {
            return 0;
        }
        ptr::copy_nonoverlapping(pixel.as_ptr(), dst, 4);
        let total_bytes = count * 4;
        let mut filled = 4usize;
        let mut copies = 0;
        while filled < total_bytes {
            let chunk = filled.min(total_bytes - filled);
            ptr::copy_nonoverlapping(dst, dst.add(filled), chunk);
            filled += chunk;
            copies += 1;
        }
        copies
    }

    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }
        let actual_w = x_end - x;
        let rows = y_end - y;
        let row_bytes = actual_w * 4;
        let pixel = self.encode_pixel(color);

        let doubling_copies = unsafe {
            let first_row = self.buf.add((y * self.stride + x) * 4);
            debug_assert!((y * self.stride + x) * 4 + row_bytes <= self.len);
            let copies = Self::fill_row(first_row, &pixel, actual_w);
            for dy in 1..rows {
                let dst_offset = ((y + dy) * self.stride + x) * 4;
                debug_assert!(dst_offset + row_bytes <= self.len);
                let dst = self.buf.add(dst_offset);
                ptr::copy_nonoverlapping(first_row, dst, row_bytes);
            }
            copies
        };

        // Every row but the first is a copy *of* the first, and the doubling
        // that built the first read all but its seed pixel back.
        self.written.set(self.written.get() + (row_bytes * rows) as u64);
        self.read
            .set(self.read.get() + (row_bytes - 4 + row_bytes * (rows - 1)) as u64);
        self.bulk_reads
            .set(self.bulk_reads.get() + doubling_copies + (rows - 1) as u64);
    }

    /// Blit a buffer to a region of the framebuffer (row-by-row memcpy).
    /// `src_stride` is the width of the source buffer (may differ from `w` during resize).
    pub fn blit(&self, x: usize, y: usize, w: usize, h: usize, src_stride: usize, buffer: &[u8]) {
        let blit_w = w.min(self.width.saturating_sub(x));
        if blit_w == 0 {
            return;
        }
        let rows = h.min(self.height.saturating_sub(y));
        let copy_bytes = blit_w * 4;
        let src_row_bytes = src_stride * 4;
        for dy in 0..rows {
            let src_offset = dy * src_row_bytes;
            let dst_offset = ((y + dy) * self.stride + x) * 4;
            debug_assert!(dst_offset + copy_bytes <= self.len);
            unsafe {
                ptr::copy_nonoverlapping(
                    buffer.as_ptr().add(src_offset),
                    self.buf.add(dst_offset),
                    copy_bytes,
                );
            }
        }
        self.written.set(self.written.get() + (copy_bytes * rows) as u64);
    }

    pub fn clear(&self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }
}

impl font::Canvas for Framebuffer {
    fn put_pixel(&self, x: usize, y: usize, color: Color) {
        self.put_pixel(x, y, color);
    }
}
