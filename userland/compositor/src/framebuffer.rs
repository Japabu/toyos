use core::ptr;

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Clone, Copy, PartialEq)]
enum PixelFormat {
    Rgb,
    Bgr,
}

pub struct Framebuffer {
    addr: *mut u8,
    width: usize,
    height: usize,
    stride: usize,
    pixel_format: PixelFormat,
}

impl Framebuffer {
    pub fn new(addr: u64, width: u32, height: u32, stride: u32, pixel_format: u32) -> Self {
        Self {
            addr: addr as *mut u8,
            width: width as usize,
            height: height as usize,
            stride: stride as usize,
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
            unsafe {
                let dst = self.addr.add((y * self.stride + x) * 4);
                ptr::write_volatile(dst as *mut [u8; 4], pixel);
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
            if sy >= self.height {
                break;
            }
            let row_base = sy * self.stride * 4;
            for dx in 0..w {
                let sx = x + dx;
                if sx >= self.width {
                    break;
                }
                unsafe {
                    let dst = self.addr.add(row_base + sx * 4);
                    ptr::write_volatile(dst as *mut [u8; 4], pixel);
                }
            }
        }
    }

    pub fn clear(&self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Blit a buffer to a region of the framebuffer (row-by-row memcpy).
    pub fn blit(&self, x: usize, y: usize, w: usize, h: usize, buffer: &[u8]) {
        let row_bytes = w * 4;
        for dy in 0..h {
            let sy = y + dy;
            if sy >= self.height {
                break;
            }
            let src_offset = dy * row_bytes;
            let dst_offset = (sy * self.stride + x) * 4;
            unsafe {
                ptr::copy_nonoverlapping(
                    buffer.as_ptr().add(src_offset),
                    self.addr.add(dst_offset),
                    row_bytes,
                );
            }
        }
    }
}
