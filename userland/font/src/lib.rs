#[derive(Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub trait Canvas {
    fn put_pixel(&self, x: usize, y: usize, color: Color);
}

pub struct Font {
    width: usize,
    height: usize,
    /// Sorted codepoints for binary search lookup.
    codepoints: Vec<u32>,
    /// Flattened glyph bitmaps: `codepoints.len() * width * height` alpha bytes.
    data: Vec<u8>,
}

impl Font {
    /// Load a pre-rasterized font from the binary format produced at build time.
    ///
    /// Format: [u16 width][u16 height][u32 glyph_count][codepoints...][alpha data...]
    pub fn from_prebuilt(bytes: &[u8]) -> Self {
        let width = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
        let height = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
        let glyph_count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;

        let cp_start = 8;
        let cp_end = cp_start + glyph_count * 4;
        let mut codepoints = Vec::with_capacity(glyph_count);
        for i in 0..glyph_count {
            let off = cp_start + i * 4;
            codepoints.push(u32::from_le_bytes([
                bytes[off],
                bytes[off + 1],
                bytes[off + 2],
                bytes[off + 3],
            ]));
        }

        let data = bytes[cp_end..].to_vec();

        Self { width, height, codepoints, data }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
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
        let idx = self
            .codepoints
            .binary_search(&(ch as u32))
            .unwrap_or_else(|_| {
                self.codepoints
                    .binary_search(&0x3F) // '?'
                    .unwrap_or(0)
            });

        let base = idx * self.width * self.height;

        for row in 0..self.height {
            for col in 0..self.width {
                let alpha = self.data[base + row * self.width + col] as u16;
                let inv = 255 - alpha;
                let r = (fg.r as u16 * alpha + bg.r as u16 * inv) / 255;
                let g = (fg.g as u16 * alpha + bg.g as u16 * inv) / 255;
                let b = (fg.b as u16 * alpha + bg.b as u16 * inv) / 255;
                canvas.put_pixel(
                    x + col,
                    y + row,
                    Color { r: r as u8, g: g as u8, b: b as u8 },
                );
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
        let mut cx = x;
        for ch in text.chars() {
            self.draw_char(canvas, cx, y, ch, fg, bg);
            cx += self.width;
        }
    }
}
