pub const WIDTH: usize = 8;
pub const HEIGHT: usize = 16;

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub trait Canvas {
    fn put_pixel(&self, x: usize, y: usize, color: Color);
}

pub struct Font {
    codepoints: Vec<u32>,
    data: Vec<u8>,
}

impl Font {
    pub fn new(raw: &[u8]) -> Self {
        assert!(raw.len() >= 4, "Font data too small");
        let count = u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize;
        let codepoints_end = 4 + count * 4;
        assert!(
            raw.len() >= codepoints_end + count * WIDTH * HEIGHT,
            "Font data too small"
        );

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
            .unwrap_or(0x3F)
    }

    pub fn draw_char(
        &self,
        canvas: &impl Canvas,
        x: usize,
        y: usize,
        ch: char,
        fg: Color,
        bg: Color,
    ) {
        let idx = self.glyph_index(ch);
        let glyph_offset = idx * WIDTH * HEIGHT;

        for gy in 0..HEIGHT {
            for gx in 0..WIDTH {
                let alpha = self.data[glyph_offset + gy * WIDTH + gx] as u16;
                let inv = 255 - alpha;
                let color = Color {
                    r: ((fg.r as u16 * alpha + bg.r as u16 * inv) / 255) as u8,
                    g: ((fg.g as u16 * alpha + bg.g as u16 * inv) / 255) as u8,
                    b: ((fg.b as u16 * alpha + bg.b as u16 * inv) / 255) as u8,
                };
                canvas.put_pixel(x + gx, y + gy, color);
            }
        }
    }

    pub fn draw_string(
        &self,
        canvas: &impl Canvas,
        x: usize,
        y: usize,
        text: &str,
        fg: Color,
        bg: Color,
    ) {
        for (i, ch) in text.chars().enumerate() {
            self.draw_char(canvas, x + i * WIDTH, y, ch, fg, bg);
        }
    }
}
