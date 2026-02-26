use core::ptr;

pub use font::Color;

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
        let pixel = self.encode_pixel(color);
        let offset = (y * self.stride + x) * 4;
        unsafe {
            ptr::copy_nonoverlapping(pixel.as_ptr(), self.addr.add(offset), 4);
        }
    }

    fn encode_pixel(&self, color: Color) -> [u8; 4] {
        match self.pixel_format {
            PixelFormat::Rgb => [color.r, color.g, color.b, 0],
            PixelFormat::Bgr => [color.b, color.g, color.r, 0],
        }
    }

    fn fill_rows(&self, y_start: usize, y_end: usize, color: Color) {
        let pixel = self.encode_pixel(color);
        for y in y_start..y_end {
            let row_offset = y * self.stride * 4;
            for x in 0..self.width {
                unsafe {
                    ptr::copy_nonoverlapping(pixel.as_ptr(), self.addr.add(row_offset + x * 4), 4);
                }
            }
        }
    }

    pub fn clear(&self, color: Color) {
        self.fill_rows(0, self.height, color);
    }

    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let pixel = self.encode_pixel(color);
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        for py in y..y_end {
            let row_offset = py * self.stride * 4;
            for px in x..x_end {
                unsafe {
                    ptr::copy_nonoverlapping(pixel.as_ptr(), self.addr.add(row_offset + px * 4), 4);
                }
            }
        }
    }

    pub fn scroll_up(&self, pixel_rows: usize, bg: Color) {
        let row_bytes = self.stride * 4;
        unsafe {
            let src = self.addr.add(pixel_rows * row_bytes);
            let dst = self.addr;
            let count = (self.height - pixel_rows) * row_bytes;
            ptr::copy(src, dst, count);
        }
        self.fill_rows(self.height - pixel_rows, self.height, bg);
    }

    pub fn scroll_down(&self, pixel_rows: usize, bg: Color) {
        if pixel_rows >= self.height {
            self.clear(bg);
            return;
        }
        let row_bytes = self.stride * 4;
        unsafe {
            let src = self.addr;
            let dst = self.addr.add(pixel_rows * row_bytes);
            let count = (self.height - pixel_rows) * row_bytes;
            ptr::copy(src, dst, count);
        }
        self.fill_rows(0, pixel_rows, bg);
    }
}

impl font::Canvas for Framebuffer {
    fn put_pixel(&self, x: usize, y: usize, color: Color) {
        self.put_pixel(x, y, color);
    }
}
