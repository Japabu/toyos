use fontdue::{Font, FontSettings};

pub fn rasterize_font() -> Vec<u8> {
    const FONT_WIDTH: usize = 8;
    const FONT_HEIGHT: usize = 16;

    let font_bytes = include_bytes!("../assets/JetBrainsMono-Regular.ttf");
    let font = Font::from_bytes(font_bytes as &[u8], FontSettings::default())
        .expect("Failed to parse font");

    let mut codepoints: Vec<u32> = (0u32..=255).collect();
    codepoints.extend(0x2500u32..=0x257F); // Box Drawing
    codepoints.extend(0x2580u32..=0x259F); // Block Elements

    let mut px_size = FONT_HEIGHT as f32;
    loop {
        let lm = font.horizontal_line_metrics(px_size).unwrap();
        let asc = lm.ascent.ceil() as i32;
        let fits = (0x20u32..=0x7E).all(|ch| {
            let (m, _) = font.rasterize(char::from_u32(ch).unwrap(), px_size);
            let glyph_top = asc - m.height as i32 - m.ymin;
            glyph_top >= 0
                && (glyph_top as usize) + m.height <= FONT_HEIGHT
                && m.width <= FONT_WIDTH
        });
        if fits {
            break;
        }
        px_size -= 0.25;
        assert!(px_size > 4.0, "Could not find a font size that fits");
    }
    let line_metrics = font.horizontal_line_metrics(px_size).unwrap();
    let ascent = line_metrics.ascent.ceil() as i32;

    let glyph_count = codepoints.len();
    let mut glyph_data = vec![0u8; glyph_count * FONT_WIDTH * FONT_HEIGHT];

    for (idx, &cp) in codepoints.iter().enumerate() {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        let (metrics, bitmap) = font.rasterize(c, px_size);
        if metrics.width == 0 || metrics.height == 0 {
            continue;
        }

        let x_offset = ((FONT_WIDTH as i32 - metrics.width as i32) / 2).max(0) as usize;
        let glyph_top = ascent - metrics.height as i32 - metrics.ymin;
        let y_offset = glyph_top.max(0) as usize;
        let glyph_base = idx * FONT_WIDTH * FONT_HEIGHT;

        for gy in 0..metrics.height {
            let cell_y = y_offset + gy;
            if cell_y >= FONT_HEIGHT {
                break;
            }
            for gx in 0..metrics.width {
                let cell_x = x_offset + gx;
                if cell_x >= FONT_WIDTH {
                    break;
                }
                glyph_data[glyph_base + cell_y * FONT_WIDTH + cell_x] =
                    bitmap[gy * metrics.width + gx];
            }
        }
    }

    let mut output = Vec::with_capacity(4 + glyph_count * 4 + glyph_data.len());
    output.extend_from_slice(&(glyph_count as u32).to_le_bytes());
    for &cp in &codepoints {
        output.extend_from_slice(&cp.to_le_bytes());
    }
    output.extend_from_slice(&glyph_data);
    output
}

pub fn generate_cursor() -> Vec<u8> {
    const WIDTH: u32 = 12;
    const HEIGHT: u32 = 17;
    const SPRITE: [&[u8]; HEIGHT as usize] = [
        b"B...........",
        b"BB..........",
        b"BWB.........",
        b"BWWB........",
        b"BWWWB.......",
        b"BWWWWB......",
        b"BWWWWWB.....",
        b"BWWWWWWB....",
        b"BWWWWWWWB...",
        b"BWWWWWWWWB..",
        b"BWWWWWBBBBB.",
        b"BWWBWWB.....",
        b"BWBBWWB.....",
        b"BB..BWWB....",
        b"B...BWWB....",
        b"....BWWB....",
        b".....BB.....",
    ];

    let mut output = Vec::with_capacity(8 + (WIDTH * HEIGHT * 4) as usize);
    output.extend_from_slice(&WIDTH.to_le_bytes());
    output.extend_from_slice(&HEIGHT.to_le_bytes());
    for row in &SPRITE {
        for &pixel in *row {
            match pixel {
                b'B' => output.extend_from_slice(&[0, 0, 0, 255]),
                b'W' => output.extend_from_slice(&[255, 255, 255, 255]),
                _ => output.extend_from_slice(&[0, 0, 0, 0]),
            }
        }
    }
    output
}
