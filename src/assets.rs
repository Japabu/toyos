use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Rasterize `codepoints` into `cell_width * cell_height` 8-bit alpha cells,
/// laid out one cell after another. The pixel size is the largest at which
/// every printable ASCII glyph fits its cell, so a fixed grid can be blitted
/// without per-glyph metrics.
fn rasterize_cells(
    ttf_bytes: &[u8],
    codepoints: &[u32],
    cell_width: usize,
    cell_height: usize,
) -> Vec<u8> {
    let font = fontdue::Font::from_bytes(ttf_bytes, fontdue::FontSettings::default())
        .expect("failed to parse TTF");

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
    let mut data = vec![0u8; codepoints.len() * cell_width * cell_height];

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

    data
}

/// Pre-rasterize a TTF font into a flat bitmap format.
///
/// Binary format:
///   [2] width: u16 LE
///   [2] height: u16 LE
///   [4] glyph_count: u32 LE
///   [glyph_count * 4] codepoints: [u32 LE]
///   [glyph_count * width * height] alpha bitmaps
fn rasterize_font(ttf_bytes: &[u8], cell_width: usize, cell_height: usize) -> Vec<u8> {
    let mut codepoints: Vec<u32> = (0u32..=255).collect();
    codepoints.extend(0x2500u32..=0x257F); // Box Drawing
    codepoints.extend(0x2580u32..=0x259F); // Block Elements

    let data = rasterize_cells(ttf_bytes, &codepoints, cell_width, cell_height);
    let glyph_count = codepoints.len();

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

/// The pre-rasterized 8x16 font the initrd carries as
/// `/share/fonts/JetBrainsMono-Regular-8x16.font`, which `/bin/console` and
/// `/bin/terminal` blit.
///
/// Produced by the same `rasterize_font` [`collect`] calls, so the screendump
/// decoder in `tests/common/screen.rs` reads the exact table the guest drew
/// with — the property the checked-in `font8x16.bin` gives the panic console,
/// obtained here from one producer instead of one file.
pub fn console_font(root: &Path) -> Vec<u8> {
    let ttf = fs::read(root.join("assets/JetBrainsMono-Regular.ttf"))
        .expect("console_font: JetBrainsMono-Regular.ttf not found");
    rasterize_font(&ttf, 8, 16)
}

/// Where the kernel's panic-console font lives, relative to the repo root.
pub const PANIC_FONT_PATH: &str = "kernel/src/drivers/panic_console/font8x16.bin";

/// First codepoint of the panic-console font; the file holds
/// `PANIC_FONT_GLYPHS` consecutive glyphs starting here.
pub const PANIC_FONT_FIRST: u8 = 0x20;
pub const PANIC_FONT_GLYPHS: usize = 0x7F - 0x20;
pub const PANIC_FONT_BYTES: usize = PANIC_FONT_GLYPHS * 16;

/// Alpha at or above which a rasterized pixel becomes a set bit. Chosen by
/// rendering the whole range at 8x16 and reading the decoded screendump: at
/// 96 every glyph in `0x20..=0x7E` is distinct and stems survive; higher
/// thresholds start eating the thin diagonals of `x` and `y`.
const PANIC_FONT_THRESHOLD: u8 = 96;

/// Regenerate `kernel/src/drivers/panic_console/font8x16.bin`.
///
/// Provenance: `assets/JetBrainsMono-Regular.ttf`, rasterized by fontdue at
/// the largest pixel size whose printable-ASCII glyphs all fit an 8x16 cell,
/// then thresholded to 1 bit at alpha >= 96. Layout is 95 glyphs of 16 bytes,
/// codepoint `0x20 + index`, one byte per row, bit 7 leftmost.
///
/// The artifact is checked in so the kernel can `include_bytes!` it with no
/// build-script coupling, and so the test harness's screendump decoder reads
/// the exact table the renderer blits. Two consumers, one file: the decoder
/// cannot drift from the renderer.
pub fn regen_panic_font(root: &Path) {
    let ttf = fs::read(root.join("assets/JetBrainsMono-Regular.ttf"))
        .expect("regen-font: JetBrainsMono-Regular.ttf not found");
    let codepoints: Vec<u32> =
        (PANIC_FONT_FIRST as u32..PANIC_FONT_FIRST as u32 + PANIC_FONT_GLYPHS as u32).collect();
    let alpha = rasterize_cells(&ttf, &codepoints, 8, 16);

    let mut out = vec![0u8; PANIC_FONT_BYTES];
    for glyph in 0..PANIC_FONT_GLYPHS {
        for row in 0..16 {
            let mut bits = 0u8;
            for col in 0..8 {
                if alpha[glyph * 128 + row * 8 + col] >= PANIC_FONT_THRESHOLD {
                    bits |= 0x80 >> col;
                }
            }
            out[glyph * 16 + row] = bits;
        }
    }

    let path = root.join(PANIC_FONT_PATH);
    fs::create_dir_all(path.parent().unwrap()).expect("regen-font: create dir");
    fs::write(&path, &out).expect("regen-font: write");
    println!("wrote {} ({} bytes)", path.display(), out.len());
}

/// Which files under `dir` git tracks, as paths relative to it.
///
/// **The image is a function of the commit, not of the working tree.** Sweeping
/// the directory instead put `assets/.DS_Store` and an `assets/target/` some
/// cargo invocation left behind into every shipped initrd — 16,368 bytes of it,
/// measured off `target/bootable.img` — so a fresh clone built a different
/// image and opening the directory in Finder moved the image hash with no code
/// change.
///
/// Asked of git rather than filtered by name: an ignore list for dotfiles and
/// `target/` states nothing about the property, and the next stray file ships
/// again. A build that cannot find out what is committed refuses, because it
/// cannot honestly build an image either.
fn tracked(dir: &Path) -> BTreeSet<PathBuf> {
    let out = Command::new("git")
        .args(["-C", &dir.display().to_string(), "ls-files", "-z"])
        .output()
        .unwrap_or_else(|e| panic!("asking git what it tracks under {}: {e}", dir.display()));
    assert!(
        out.status.success(),
        "git could not list {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub fn collect(dirs: &[String]) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![];

    for dir in dirs {
        let dir = Path::new(dir);
        let tracked = tracked(dir);
        let ships = |path: &Path| {
            let relative = path.strip_prefix(dir).unwrap_or(path);
            if tracked.contains(relative) {
                return true;
            }
            eprintln!("assets: skipping {} — git does not track it", path.display());
            false
        };

        // Pre-rasterize TTF fonts
        for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("Failed to read {}: {e}", dir.display())) {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "ttf") && ships(&path) {
                let ttf = fs::read(&path).unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                let stem = path.file_stem().unwrap().to_str().unwrap();
                let font_data = rasterize_font(&ttf, 8, 16);
                files.push((format!("share/fonts/{stem}-8x16.font"), font_data));
            }
        }

        // Pre-decode JPEG images
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "jpg") && ships(&path) {
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
        fn add_dir(
            dir: &Path,
            prefix: &str,
            ships: &dyn Fn(&Path) -> bool,
            files: &mut Vec<(String, Vec<u8>)>,
        ) {
            for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("Failed to read {}: {e}", dir.display())) {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    let subdir = path.file_name().unwrap().to_str().unwrap();
                    add_dir(&path, &format!("{prefix}{subdir}/"), ships, files);
                } else if path.extension().is_some_and(|e| e == "ttf" || e == "jpg") {
                    continue;
                } else if ships(&path) {
                    let name = path.file_name().unwrap().to_str().unwrap().to_lowercase();
                    let data = fs::read(&path).unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
                    files.push((format!("{prefix}{name}"), data));
                }
            }
        }
        add_dir(dir, "share/", &ships, &mut files);
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing git does not track reaches the initrd.
    ///
    /// Against a repository this test builds, not against `assets/`: the two
    /// files that shipped for real — `.DS_Store` and a stray `target/` — are
    /// exactly what a working tree acquires by being worked in, so a gate that
    /// depended on them being present would pass on a clean checkout and prove
    /// nothing. Here they are put there on purpose.
    #[test]
    fn only_tracked_assets_ship() {
        let dir = std::env::temp_dir().join(format!("toyos-assets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("icons")).expect("make the asset tree");
        fs::create_dir_all(dir.join("target")).expect("make a stray target/");

        fs::write(dir.join("kept.wad"), b"tracked").expect("write kept.wad");
        fs::write(dir.join("icons/kept.svg"), b"tracked").expect("write icons/kept.svg");
        fs::write(dir.join(".DS_Store"), b"finder").expect("write .DS_Store");
        fs::write(dir.join("target/.deps-stamp"), b"cargo").expect("write target/.deps-stamp");

        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(["-C", &dir.display().to_string()])
                .args(args)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["add", "kept.wad", "icons/kept.svg"]);

        let shipped: BTreeSet<String> =
            collect(&[dir.display().to_string()]).into_iter().map(|(name, _)| name).collect();
        fs::remove_dir_all(&dir).ok();

        assert_eq!(
            shipped,
            BTreeSet::from(["share/kept.wad".to_string(), "share/icons/kept.svg".to_string()]),
            "the initrd's asset list is not what the commit says it is"
        );
    }
}
