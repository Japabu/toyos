use core::ptr;

pub use font::Color;

#[derive(Clone, Copy, PartialEq)]
enum PixelFormat {
    Rgb,
    Bgr,
}

pub struct CursorImage {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl CursorImage {
    pub fn new(raw: &[u8]) -> Self {
        let width = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
        let height = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
        let data = raw[8..8 + width * height * 4].to_vec();
        Self { width, height, data }
    }
}

pub struct Framebuffer {
    bufs: [*mut u8; 2],
    back_idx: usize,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
}

impl Framebuffer {
    pub fn new(addrs: [u64; 2], width: u32, height: u32, stride: u32, pixel_format: u32) -> Self {
        let width = width as usize;
        let height = height as usize;
        let stride = stride as usize;
        Self {
            bufs: [addrs[0] as *mut u8, addrs[1] as *mut u8],
            back_idx: 1, // front=0 (displayed), back=1 (draw target)
            width,
            height,
            stride,
            pixel_format: if pixel_format == 0 { PixelFormat::Rgb } else { PixelFormat::Bgr },
        }
    }

    fn back(&self) -> *mut u8 {
        self.bufs[self.back_idx]
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

    pub fn swap(&mut self) {
        self.back_idx = 1 - self.back_idx;
    }

    pub fn put_pixel(&self, x: usize, y: usize, color: Color) {
        if x < self.width && y < self.height {
            let pixel = self.encode_pixel(color);
            let offset = (y * self.stride + x) * 4;
            unsafe {
                ptr::copy_nonoverlapping(pixel.as_ptr(), self.back().add(offset), 4);
            }
        }
    }

    fn encode_pixel(&self, color: Color) -> [u8; 4] {
        match self.pixel_format {
            PixelFormat::Rgb => [color.r, color.g, color.b, 0],
            PixelFormat::Bgr => [color.b, color.g, color.r, 0],
        }
    }

    /// Fill a row of pixels with a 4-byte pattern starting at `dst`.
    unsafe fn fill_row(dst: *mut u8, pixel: &[u8; 4], count: usize) {
        if count == 0 { return; }
        // Write first pixel
        ptr::copy_nonoverlapping(pixel.as_ptr(), dst, 4);
        // Doubling copy: 1→2→4→8→... until the row is filled
        let total_bytes = count * 4;
        let mut filled = 4usize;
        while filled < total_bytes {
            let chunk = filled.min(total_bytes - filled);
            ptr::copy_nonoverlapping(dst, dst.add(filled), chunk);
            filled += chunk;
        }
    }

    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        if w == 0 || h == 0 { return; }
        let pixel = self.encode_pixel(color);
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        let actual_w = x_end.saturating_sub(x);
        if actual_w == 0 { return; }
        let row_bytes = actual_w * 4;

        unsafe {
            // Fill first row with the pixel pattern
            let first_row = self.back().add((y * self.stride + x) * 4);
            Self::fill_row(first_row, &pixel, actual_w);

            // Copy first row to all remaining rows
            for dy in 1..(y_end - y) {
                let dst = self.back().add(((y + dy) * self.stride + x) * 4);
                ptr::copy_nonoverlapping(first_row, dst, row_bytes);
            }
        }
    }

    pub fn clear(&self, color: Color) {
        let pixel = self.encode_pixel(color);
        unsafe {
            // Fill first scanline
            let first_row = self.back();
            Self::fill_row(first_row, &pixel, self.stride);

            // Copy to all remaining scanlines
            let row_bytes = self.stride * 4;
            for y in 1..self.height {
                let dst = self.back().add(y * row_bytes);
                ptr::copy_nonoverlapping(first_row, dst, row_bytes);
            }
        }
    }

    /// Blit a buffer to a region of the back buffer (row-by-row memcpy).
    pub fn blit(&self, x: usize, y: usize, w: usize, h: usize, buffer: &[u8]) {
        let row_bytes = w * 4;
        for dy in 0..h {
            let sy = y + dy;
            if sy >= self.height { break; }
            let src_offset = dy * row_bytes;
            let dst_offset = (sy * self.stride + x) * 4;
            unsafe {
                ptr::copy_nonoverlapping(
                    buffer.as_ptr().add(src_offset),
                    self.back().add(dst_offset),
                    row_bytes,
                );
            }
        }
    }

    /// Overlay cursor onto the back buffer.
    pub fn draw_cursor(&self, cursor_x: i32, cursor_y: i32, cursor: &CursorImage) {
        for cy in 0..cursor.height {
            let sy = cursor_y as usize + cy;
            if sy >= self.height { break; }
            for cx in 0..cursor.width {
                let sx = cursor_x as usize + cx;
                if sx >= self.width { break; }
                let off = (cy * cursor.width + cx) * 4;
                let alpha = cursor.data[off + 3];
                if alpha > 0 {
                    let color = Color {
                        r: cursor.data[off],
                        g: cursor.data[off + 1],
                        b: cursor.data[off + 2],
                    };
                    let pixel = self.encode_pixel(color);
                    let dst_offset = (sy * self.stride + sx) * 4;
                    unsafe {
                        ptr::copy_nonoverlapping(pixel.as_ptr(), self.back().add(dst_offset), 4);
                    }
                }
            }
        }
    }
}

impl font::Canvas for Framebuffer {
    fn put_pixel(&self, x: usize, y: usize, color: Color) {
        self.put_pixel(x, y, color);
    }
}
