use resvg::tiny_skia;
use resvg::usvg;

pub struct Sprite {
    width: usize,
    height: usize,
    data: Vec<u8>, // RGBA, 4 bytes per pixel
}

impl Sprite {
    /// Rasterize an SVG at the given pixel size. The SVG is scaled to fit a `size x size` square.
    pub fn from_svg(svg_bytes: &[u8], size: u32) -> Self {
        let tree = usvg::Tree::from_data(svg_bytes, &usvg::Options::default())
            .expect("failed to parse SVG");
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("failed to create pixmap");
        let svg_size = tree.size();
        let sx = size as f32 / svg_size.width();
        let sy = size as f32 / svg_size.height();
        resvg::render(&tree, tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
        // resvg outputs premultiplied RGBA; convert to straight alpha
        let mut data = pixmap.take();
        for chunk in data.chunks_exact_mut(4) {
            let a = chunk[3] as u16;
            if a > 0 && a < 255 {
                chunk[0] = ((chunk[0] as u16 * 255) / a) as u8;
                chunk[1] = ((chunk[1] as u16 * 255) / a) as u8;
                chunk[2] = ((chunk[2] as u16 * 255) / a) as u8;
            }
        }
        Self { width: size as usize, height: size as usize, data }
    }

    /// Rasterize an SVG, replacing `currentColor` with the given RGB color.
    pub fn from_svg_colored(svg_bytes: &[u8], size: u32, color: [u8; 3]) -> Self {
        let svg_str = String::from_utf8_lossy(svg_bytes);
        let hex = format!("#{:02x}{:02x}{:02x}", color[0], color[1], color[2]);
        let replaced = svg_str.replace("currentColor", &hex);
        Self::from_svg(replaced.as_bytes(), size)
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Alpha-blended blit onto a raw pixel buffer.
    /// `pixel_format`: 0 = RGB, 1 = BGR (matches WindowInfo.pixel_format).
    pub fn draw(
        &self,
        dst: *mut u8,
        dst_stride: usize,
        dst_w: usize,
        dst_h: usize,
        pixel_format: u32,
        dx: usize,
        dy: usize,
    ) {
        let bgr = pixel_format != 0;
        for sy in 0..self.height {
            let y = dy + sy;
            if y >= dst_h {
                break;
            }
            for sx in 0..self.width {
                let x = dx + sx;
                if x >= dst_w {
                    break;
                }
                let src_off = (sy * self.width + sx) * 4;
                let alpha = self.data[src_off + 3] as u16;
                if alpha == 0 {
                    continue;
                }
                let dst_off = (y * dst_stride + x) * 4;
                let sr = self.data[src_off] as u16;
                let sg = self.data[src_off + 1] as u16;
                let sb = self.data[src_off + 2] as u16;
                unsafe {
                    let p = dst.add(dst_off);
                    if alpha == 255 {
                        if bgr {
                            *p = sb as u8;
                            *p.add(1) = sg as u8;
                            *p.add(2) = sr as u8;
                        } else {
                            *p = sr as u8;
                            *p.add(1) = sg as u8;
                            *p.add(2) = sb as u8;
                        }
                    } else {
                        let inv = 255 - alpha;
                        let (dr, dg, db) = if bgr {
                            (*p.add(2) as u16, *p.add(1) as u16, *p as u16)
                        } else {
                            (*p as u16, *p.add(1) as u16, *p.add(2) as u16)
                        };
                        let r = ((sr * alpha + dr * inv) / 255) as u8;
                        let g = ((sg * alpha + dg * inv) / 255) as u8;
                        let b = ((sb * alpha + db * inv) / 255) as u8;
                        if bgr {
                            *p = b;
                            *p.add(1) = g;
                            *p.add(2) = r;
                        } else {
                            *p = r;
                            *p.add(1) = g;
                            *p.add(2) = b;
                        }
                    }
                }
            }
        }
    }
}
