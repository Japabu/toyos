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
    hw: *mut u8,
    back: Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
}

impl Framebuffer {
    pub fn new(addr: u64, width: u32, height: u32, stride: u32, pixel_format: u32) -> Self {
        let width = width as usize;
        let height = height as usize;
        let stride = stride as usize;
        let back = vec![0u8; height * stride * 4];
        Self {
            hw: addr as *mut u8,
            back,
            width,
            height,
            stride,
            pixel_format: if pixel_format == 0 { PixelFormat::Rgb } else { PixelFormat::Bgr },
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

    pub fn put_pixel(&self, x: usize, y: usize, color: Color) {
        if x < self.width && y < self.height {
            let pixel = self.encode_pixel(color);
            let offset = (y * self.stride + x) * 4;
            unsafe {
                ptr::copy_nonoverlapping(pixel.as_ptr(), self.back.as_ptr().add(offset) as *mut u8, 4);
            }
        }
    }

    fn encode_pixel(&self, color: Color) -> [u8; 4] {
        match self.pixel_format {
            PixelFormat::Rgb => [color.r, color.g, color.b, 0],
            PixelFormat::Bgr => [color.b, color.g, color.r, 0],
        }
    }

    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let pixel = self.encode_pixel(color);
        for dy in 0..h {
            let sy = y + dy;
            if sy >= self.height { break; }
            let row_base = sy * self.stride * 4;
            for dx in 0..w {
                let sx = x + dx;
                if sx >= self.width { break; }
                unsafe {
                    let dst = self.back.as_ptr().add(row_base + sx * 4) as *mut u8;
                    ptr::copy_nonoverlapping(pixel.as_ptr(), dst, 4);
                }
            }
        }
    }

    pub fn clear(&self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
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
                    self.back.as_ptr().add(dst_offset) as *mut u8,
                    row_bytes,
                );
            }
        }
    }

    /// Copy back buffer to hardware framebuffer, overlaying the cursor.
    pub fn present(&self, cursor_x: i32, cursor_y: i32, cursor: &CursorImage) {
        // Copy entire back buffer to hardware framebuffer
        unsafe {
            ptr::copy_nonoverlapping(self.back.as_ptr(), self.hw, self.back.len());
        }
        // Overlay cursor on hardware framebuffer
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
                        ptr::write_volatile(self.hw.add(dst_offset) as *mut [u8; 4], pixel);
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
