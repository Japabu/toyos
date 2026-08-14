//! Bake the glyphs `calc` draws with into the binary, at four cell sizes.
//!
//! Snake bakes one 8x16 table the same way. `calc` needs four because a result
//! is up to 40 significant digits and the display strip picks the largest cell
//! the string still fits in — a number is never cut to make it fit, so the
//! sizes below are what "shrink to stay readable" is made of.

use std::env;
use std::fs;
use std::path::PathBuf;

/// `(cell width, cell height)` for every table baked into the binary.
const SIZES: &[(usize, usize)] = &[(6, 12), (8, 16), (10, 20), (12, 24)];

/// Codepoints beyond Latin-1 that the buttons and the display name.
///
/// Every one is asserted to rasterize below: a glyph the font does not carry
/// would otherwise be baked as a blank cell and the button would read empty.
/// Sorted, because `font::Font` looks a codepoint up by binary search.
/// `src/main.rs`'s `DRAWABLE_NON_ASCII` is the same set from the other side,
/// and its test is what keeps a label from naming a glyph nothing baked.
const EXTRA: &[u32] = &[
    0x03C0, // GREEK SMALL LETTER PI
    0x2190, // LEFTWARDS ARROW — the backspace key
    0x2212, // MINUS SIGN
    0x221A, // SQUARE ROOT
    0x2248, // ALMOST EQUAL TO
];

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ttf = fs::read("../../assets/JetBrainsMono-Regular.ttf").expect("Failed to read font TTF");
    for &(w, h) in SIZES {
        let data = rasterize_font(&ttf, w, h);
        fs::write(out_dir.join(format!("JetBrainsMono-Regular-{w}x{h}.font")), data).unwrap();
    }
    println!("cargo:rerun-if-changed=../../assets/JetBrainsMono-Regular.ttf");
}

fn rasterize_font(ttf_bytes: &[u8], cell_width: usize, cell_height: usize) -> Vec<u8> {
    let font = fontdue::Font::from_bytes(ttf_bytes, fontdue::FontSettings::default())
        .expect("failed to parse TTF");

    let mut codepoints: Vec<u32> = (0u32..=255).collect();
    codepoints.extend(EXTRA.iter().copied());

    let mut px_size = cell_height as f32;
    loop {
        let lm = font.horizontal_line_metrics(px_size).unwrap();
        let asc = lm.ascent.ceil() as i32;
        let fits = (0x20u32..=0x7E).chain(EXTRA.iter().copied()).all(|ch| {
            let (m, _) = font.rasterize(char::from_u32(ch).unwrap(), px_size);
            let glyph_top = asc - m.height as i32 - m.ymin;
            glyph_top >= 0 && (glyph_top as usize) + m.height <= cell_height && m.width <= cell_width
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
            assert!(
                !EXTRA.contains(&cp),
                "JetBrains Mono has no glyph for U+{cp:04X}; a button that names it would draw \
                 an empty cell"
            );
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
                data[glyph_base + cell_y * cell_width + cell_x] = bitmap[gy * metrics.width + gx];
            }
        }
    }

    for &cp in EXTRA {
        let idx = codepoints.iter().position(|&c| c == cp).unwrap();
        let base = idx * cell_width * cell_height;
        let ink: u32 = data[base..base + cell_width * cell_height].iter().map(|&a| a as u32).sum();
        assert!(ink > 0, "U+{cp:04X} rasterized blank at {cell_width}x{cell_height}");
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
