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
            pixel_format: if pixel_format == 0 { PixelFormat::Rgb } else { PixelFormat::Bgr },
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    fn encode_pixel(&self, color: Color) -> [u8; 4] {
        match self.pixel_format {
            PixelFormat::Rgb => [color.r, color.g, color.b, 0],
            PixelFormat::Bgr => [color.b, color.g, color.r, 0],
        }
    }

    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }
        let pixel = self.encode_pixel(color);
        for py in y..y_end {
            for px in x..x_end {
                let off = (py * self.stride + px) * 4;
                unsafe {
                    core::ptr::copy_nonoverlapping(pixel.as_ptr(), self.addr.add(off), 4);
                }
            }
        }
    }
}

impl font::Canvas for Framebuffer {
    fn put_pixel(&self, x: usize, y: usize, color: Color) {
        if x < self.width && y < self.height {
            let pixel = self.encode_pixel(color);
            let off = (y * self.stride + x) * 4;
            unsafe {
                core::ptr::copy_nonoverlapping(pixel.as_ptr(), self.addr.add(off), 4);
            }
        }
    }
}
