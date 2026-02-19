use core::ptr;

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255 };
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0 };
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
    /// # Safety
    /// `addr` must point to valid framebuffer memory of at least `size` bytes.
    pub unsafe fn new(
        addr: u64,
        _size: u64,
        width: u32,
        height: u32,
        stride: u32,
        pixel_format: u32,
    ) -> Self {
        Self {
            addr: addr as *mut u8,
            width: width as usize,
            height: height as usize,
            stride: stride as usize,
            pixel_format: if pixel_format == 0 {
                PixelFormat::Rgb
            } else {
                PixelFormat::Bgr
            },
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    #[inline]
    pub fn put_pixel(&self, x: usize, y: usize, color: Color) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = (y * self.stride + x) * 4;
        unsafe {
            let pixel = self.addr.add(offset);
            match self.pixel_format {
                PixelFormat::Rgb => {
                    ptr::write_volatile(pixel, color.r);
                    ptr::write_volatile(pixel.add(1), color.g);
                    ptr::write_volatile(pixel.add(2), color.b);
                }
                PixelFormat::Bgr => {
                    ptr::write_volatile(pixel, color.b);
                    ptr::write_volatile(pixel.add(1), color.g);
                    ptr::write_volatile(pixel.add(2), color.r);
                }
            }
        }
    }

    pub fn clear(&self, color: Color) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, color);
            }
        }
    }

    pub fn scroll_up(&self, pixel_rows: usize, bg: Color) {
        let bytes_per_pixel = 4usize;
        let row_bytes = self.stride * bytes_per_pixel;

        unsafe {
            let src = self.addr.add(pixel_rows * row_bytes);
            let dst = self.addr;
            let count = (self.height - pixel_rows) * row_bytes;
            ptr::copy(src, dst, count);
        }

        let clear_start_y = self.height - pixel_rows;
        for y in clear_start_y..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, bg);
            }
        }
    }
}
