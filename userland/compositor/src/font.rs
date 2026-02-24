use crate::framebuffer::{Color, Framebuffer};

const WIDTH: usize = 8;
const HEIGHT: usize = 16;

pub struct Font {
    codepoints: Vec<u32>,
    data: Vec<u8>,
}

impl Font {
    pub fn new(raw: &[u8]) -> Self {
        let count = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
        let codepoints_end = 4 + count * 4;

        let mut codepoints = Vec::with_capacity(count);
        for i in 0..count {
            let off = 4 + i * 4;
            let cp = u32::from_le_bytes(raw[off..off + 4].try_into().unwrap());
            codepoints.push(cp);
        }
        let data = raw[codepoints_end..].to_vec();

        Self { codepoints, data }
    }

    fn glyph_index(&self, ch: char) -> usize {
        self.codepoints
            .binary_search(&(ch as u32))
            .unwrap_or(0)
    }

    pub fn draw_string(&self, fb: &Framebuffer, x: usize, y: usize, text: &str, fg: Color, bg: Color) {
        for (i, ch) in text.chars().enumerate() {
            let idx = self.glyph_index(ch);
            let glyph_offset = idx * WIDTH * HEIGHT;
            let px = x + i * WIDTH;

            for gy in 0..HEIGHT {
                for gx in 0..WIDTH {
                    let alpha = self.data[glyph_offset + gy * WIDTH + gx] as u16;
                    let inv = 255 - alpha;
                    let color = Color {
                        r: ((fg.r as u16 * alpha + bg.r as u16 * inv) / 255) as u8,
                        g: ((fg.g as u16 * alpha + bg.g as u16 * inv) / 255) as u8,
                        b: ((fg.b as u16 * alpha + bg.b as u16 * inv) / 255) as u8,
                    };
                    fb.put_pixel(px + gx, y + gy, color);
                }
            }
        }
    }
}
