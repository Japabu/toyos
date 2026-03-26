use std::fs;
use std::path::Path;

/// Pre-rasterize a TTF font into a flat bitmap format.
///
/// Binary format:
///   [2] width: u16 LE
///   [2] height: u16 LE
///   [4] glyph_count: u32 LE
///   [glyph_count * 4] codepoints: [u32 LE]
///   [glyph_count * width * height] alpha bitmaps
fn rasterize_font(ttf_bytes: &[u8], cell_width: usize, cell_height: usize) -> Vec<u8> {
    let font = fontdue::Font::from_bytes(ttf_bytes, fontdue::FontSettings::default())
        .expect("failed to parse TTF");

    let mut codepoints: Vec<u32> = (0u32..=255).collect();
    codepoints.extend(0x2500u32..=0x257F); // Box Drawing
    codepoints.extend(0x2580u32..=0x259F); // Block Elements

    // Find the largest pixel size that fits all printable ASCII glyphs
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
        assert!(px_size > 2.0, "could not find a font size that fits {cell_width}x{cell_height}");
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

    // Serialize to binary format
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

pub fn collect(dirs: &[String]) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![];

    for dir in dirs {
        let dir = Path::new(dir);

        // Pre-rasterize TTF fonts
        for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("Failed to read {}: {e}", dir.display())) {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "ttf") {
                let ttf = fs::read(&path).unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                let stem = path.file_stem().unwrap().to_str().unwrap();
                let font_data = rasterize_font(&ttf, 8, 16);
                files.push((format!("share/fonts/{stem}-8x16.font"), font_data));
            }
        }

        // Pre-decode JPEG images
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "jpg") {
                let jpg_data = fs::read(&path).unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                let img = image::load_from_memory_with_format(&jpg_data, image::ImageFormat::Jpeg)
                    .expect("Failed to decode JPEG")
                    .to_rgb8();
                let stem = path.file_stem().unwrap().to_str().unwrap();
                let mut data = Vec::new();
                data.extend((img.width() as u32).to_le_bytes());
                data.extend((img.height() as u32).to_le_bytes());
                data.extend(img.as_raw());
                files.push((format!("share/{stem}.rgb"), data));
            }
        }

        // Include all other files recursively (skipping pre-processed types)
        fn add_dir(dir: &Path, prefix: &str, files: &mut Vec<(String, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("Failed to read {}: {e}", dir.display())) {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    let subdir = path.file_name().unwrap().to_str().unwrap();
                    add_dir(&path, &format!("{prefix}{subdir}/"), files);
                } else if path.extension().is_some_and(|e| e == "ttf" || e == "jpg") {
                    continue;
                } else {
                    let name = path.file_name().unwrap().to_str().unwrap().to_lowercase();
                    let data = fs::read(&path).unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                    files.push((format!("{prefix}{name}"), data));
                }
            }
        }
        add_dir(dir, "share/", &mut files);
    }

    files
}
