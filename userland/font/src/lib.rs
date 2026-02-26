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
    /// Rasterize a TTF at the given cell dimensions.
    pub fn new(ttf_bytes: &[u8], cell_width: usize, cell_height: usize) -> Self {
        let font = fontdue::Font::from_bytes(ttf_bytes, fontdue::FontSettings::default())
            .expect("failed to parse TTF");

        let mut codepoints: Vec<u32> = (0u32..=255).collect();
        codepoints.extend(0x2500u32..=0x257F); // Box Drawing
        codepoints.extend(0x2580u32..=0x259F); // Block Elements

        // Find the largest pixel size that fits all printable ASCII glyphs in the cell
        let mut px_size = cell_height as f32;
        loop {
            let lm = font.horizontal_line_metrics(px_size).unwrap();
            let asc = lm.ascent.ceil() as i32;
            let fits = (0x20u32..=0x7E).all(|ch| {
                let (m, _) = font.rasterize(char::from_u32(ch).unwrap(), px_size);
                let glyph_top = asc - m.height as i32 - m.ymin;
                glyph_top >= 0
                    && (glyph_top as usize) + m.height <= cell_height
                    && m.width <= cell_width
            });
            if fits {
                break;
            }
            px_size -= 0.25;
            assert!(
                px_size > 2.0,
                "could not find a font size that fits {}x{}",
                cell_width,
                cell_height,
            );
        }

        let ascent = font.horizontal_line_metrics(px_size).unwrap().ascent.ceil() as i32;
        let glyph_count = codepoints.len();
        let mut data = vec![0u8; glyph_count * cell_width * cell_height];

        for (idx, &cp) in codepoints.iter().enumerate() {
            let Some(c) = char::from_u32(cp) else { continue };
            let (metrics, bitmap) = font.rasterize(c, px_size);
            if metrics.width == 0 || metrics.height == 0 {
                continue;
            }

            let x_offset = ((cell_width as i32 - metrics.width as i32) / 2).max(0) as usize;
            let glyph_top = ascent - metrics.height as i32 - metrics.ymin;
            let y_offset = glyph_top.max(0) as usize;
            let glyph_base = idx * cell_width * cell_height;

            for gy in 0..metrics.height {
                let cell_y = y_offset + gy;
                if cell_y >= cell_height {
                    break;
                }
                for gx in 0..metrics.width {
                    let cell_x = x_offset + gx;
                    if cell_x >= cell_width {
                        break;
                    }
                    data[glyph_base + cell_y * cell_width + cell_x] =
                        bitmap[gy * metrics.width + gx];
                }
            }
        }

        Self { width: cell_width, height: cell_height, codepoints, data }
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
