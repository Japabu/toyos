use alloc::vec::Vec;

use crate::framebuffer::{Color, Framebuffer};

pub const WIDTH: usize = 8;
pub const HEIGHT: usize = 16;

const GLYPHS: usize = 256;
const BYTES: usize = GLYPHS * WIDTH * HEIGHT;

pub struct Font {
    data: Vec<u8>,
}

impl Font {
    pub fn new(data: Vec<u8>) -> Self {
        assert!(data.len() >= BYTES, "Font data too small");
        Self { data }
    }

    pub fn draw_char(&self, fb: &Framebuffer, px: usize, py: usize, ch: u8, fg: Color, bg: Color) {
        let glyph_offset = (ch as usize) * WIDTH * HEIGHT;

        for gy in 0..HEIGHT {
            for gx in 0..WIDTH {
                let alpha = self.data[glyph_offset + gy * WIDTH + gx] as u16;
                let inv = 255 - alpha;
                let color = Color {
                    r: ((fg.r as u16 * alpha + bg.r as u16 * inv) / 255) as u8,
                    g: ((fg.g as u16 * alpha + bg.g as u16 * inv) / 255) as u8,
                    b: ((fg.b as u16 * alpha + bg.b as u16 * inv) / 255) as u8,
                };
                fb.put_pixel(px + gx, py + gy, color);
            }
        }
    }
}
