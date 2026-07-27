use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ttf = fs::read("../../assets/JetBrainsMono-Regular.ttf").expect("Failed to read font TTF");
    let font_data = rasterize_font(&ttf, 8, 16);
    fs::write(out_dir.join("JetBrainsMono-Regular-8x16.font"), font_data).unwrap();
    println!("cargo:rerun-if-changed=../../assets/JetBrainsMono-Regular.ttf");
}

fn rasterize_font(ttf_bytes: &[u8], cell_width: usize, cell_height: usize) -> Vec<u8> {
    let font = fontdue::Font::from_bytes(ttf_bytes, fontdue::FontSettings::default())
        .expect("failed to parse TTF");

    let mut codepoints: Vec<u32> = (0u32..=255).collect();
    codepoints.extend(0x2500u32..=0x257F);
    codepoints.extend(0x2580u32..=0x259F);

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
        assert!(px_size > 2.0);
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

    let mut out = Vec::new();
    out.extend((cell_width as u16).to_le_bytes());
    out.extend((cell_height as u16).to_le_bytes());
    out.extend((glyph_count as u32).to_le_bytes());
    for &cp in &codepoints {
        out.extend(cp.to_le_bytes());
    }
    out.extend(data);
    out
}
