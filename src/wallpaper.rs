//! The desktop background, written down as a description instead of fetched.
//!
//! `assets/wallpaper.jpg` used to be a file of unknown origin — an aggregator
//! upload naming no author and no copyright holder, under a "wallpaper use
//! only" restriction nobody on that page had the standing to grant. What
//! replaces it is the output of [`draw`], so provenance is answered by source
//! anyone can re-run rather than by a paragraph in `NOTICE`.
//!
//! The picture is a lit horizon: a night sky, a glow behind it, and three
//! ridges receding into haze. It is nothing but the constants below — change
//! one, run `cargo run -- --regen-wallpaper`, and the committed artifact moves
//! with it.
//!
//! All compositing is in **linear light**, converted to sRGB once per pixel at
//! the end. Mixing two colours in sRGB mixes two numbers that were never linear
//! to begin with, which is what sends a naive gradient grey through its middle.

use std::path::Path;

use image::codecs::jpeg::JpegEncoder;
use image::ExtendedColorType;

/// Where the artifact lives, relative to the repository root.
pub const WALLPAPER_PATH: &str = "assets/wallpaper.jpg";

/// Drawn at the panel this project targets, the T14's 1920x1080. The
/// compositor scales whatever it is handed, so this is the size at which it
/// scales by nothing.
pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;

/// **The quantization table is why this is up here and not at the usual 90.**
/// libjpeg's scaling leaves every entry of both tables at 1 by 99, and `image`'s
/// encoder never subsamples chroma (`h: 1, v: 1` on all three components), so
/// the round trip is the DCT's own rounding and nothing else. Anywhere below
/// that the DC coefficient alone is quantized in steps of two or three levels,
/// which lays an 8x8 staircase across exactly the smooth field this picture is
/// made of — and the same quantizer crushes the dither that was there to stop
/// one. `the_wallpaper_neither_bands_nor_blocks` is that stated as a bound.
///
/// Not 100, which is the same file size to within 0.2% and worse on every other
/// measure. The sweep both readings come from is
/// `specs/assessments/dependency-audit-2026-08-08.md` §7f.1.
pub const QUALITY: u8 = 99;

/// The sky, top to horizon, as sRGB.
///
/// Both nearly black on purpose. The glow is what the eye sees; a sky bright
/// enough to notice on its own would flatten it.
const SKY_TOP: [u8; 3] = [0x04, 0x05, 0x0d];
const SKY_HORIZON: [u8; 3] = [0x0a, 0x0f, 0x22];

/// A soft elliptical light, in coordinates normalised to the screen so the
/// composition is the same at any resolution.
///
/// The falloff is `exp(-d²)` over the normalised radius: a gaussian has no edge
/// anywhere, where anything built out of a `smoothstep` has one where the step
/// ends, and on a field this dark an edge is the first thing the eye finds.
struct Light {
    x: f32,
    y: f32,
    rx: f32,
    ry: f32,
    color: [u8; 3],
    gain: f32,
    /// Which noise field warps this light's radius. Distinct per light, or two
    /// of them breathe together and the repetition is what reads.
    seed: u32,
}

/// Three, and each has a job: the sun that has just gone down behind the far
/// ridge, a violet counterweight high on the other side so the frame is not
/// symmetric, and a small cold one at the left margin that shows only as a
/// shift in hue.
const LIGHTS: [Light; 3] = [
    Light {
        x: 0.38,
        y: 0.635,
        rx: 0.42,
        ry: 0.22,
        color: [0x4a, 0x74, 0xd8],
        gain: 0.36,
        seed: 0x51ed_2701,
    },
    Light {
        x: 0.87,
        y: 0.05,
        rx: 0.40,
        ry: 0.44,
        color: [0x5a, 0x2f, 0x96],
        gain: 0.085,
        seed: 0x9e37_79b9,
    },
    Light {
        x: 0.02,
        y: 0.40,
        rx: 0.26,
        ry: 0.34,
        color: [0x17, 0x63, 0x6e],
        gain: 0.045,
        seed: 0xc2b2_ae35,
    },
];

/// One layer of the skyline: where it sits, how far it moves, how coarse it is
/// and what it is made of.
///
/// `crest` is the colour along the top edge and `base` the colour `depth`
/// below it, which is the whole of the haze: air between the viewer and a ridge
/// scatters the sky into it, and the nearer the ridge the less of that there is
/// to scatter.
struct Ridge {
    /// Mean height as a fraction of the screen, measured from the top.
    height: f32,
    /// How far the profile departs from that, peak to trough.
    amplitude: f32,
    /// Noise cells across the full width at the first octave.
    frequency: f32,
    /// How sharp the peaks are: 0 is rolling hills, 1 is ridged.
    sharpness: f32,
    crest: [u8; 3],
    base: [u8; 3],
    /// How far below the crest the colour has finished falling to `base`.
    depth: f32,
    seed: u32,
}

/// Far to near. Each is lower, darker, less hazy and less detailed than the one
/// behind it — the three cues that make a flat image read as depth.
const RIDGES: [Ridge; 3] = [
    Ridge {
        height: 0.600,
        amplitude: 0.130,
        frequency: 2.9,
        sharpness: 0.85,
        crest: [0x17, 0x25, 0x3f],
        base: [0x10, 0x1a, 0x2e],
        depth: 0.18,
        seed: 0x7f4a_7c15,
    },
    Ridge {
        height: 0.735,
        amplitude: 0.100,
        frequency: 3.4,
        sharpness: 0.80,
        crest: [0x0d, 0x15, 0x26],
        base: [0x09, 0x0f, 0x1c],
        depth: 0.14,
        seed: 0x1b87_3593,
    },
    Ridge {
        height: 0.875,
        amplitude: 0.065,
        frequency: 4.6,
        sharpness: 0.90,
        crest: [0x07, 0x0a, 0x14],
        base: [0x04, 0x05, 0x0c],
        depth: 0.10,
        seed: 0x3243_f6a8,
    },
];

/// Octaves in a ridge profile. Four puts the finest detail at roughly a hundred
/// pixels across the target panel, which is landscape rather than noise.
const RIDGE_OCTAVES: u32 = 4;

/// How much the noise field may move a light's radius, and how many noise cells
/// span the screen.
///
/// This is the difference between a sky and a diagram of three ellipses. Both
/// octaves are slow: anything faster costs JPEG bits and reads as texture.
const DRIFT: f32 = 0.42;
const DRIFT_CELLS: f32 = 2.4;

/// Corner darkening: how much is taken at full strength, and the normalised
/// radii the ramp runs between. 1.0 is the middle of an edge, 1.414 a corner.
const VIGNETTE: f32 = 0.5;
const VIGNETTE_IN: f32 = 0.30;
const VIGNETTE_OUT: f32 = 1.25;

/// Grain amplitude in output levels, applied per channel after the sRGB
/// conversion and before rounding.
///
/// This is dither and not decoration. Eight bits a channel over a field this
/// smooth puts a contour every few dozen rows; a triangular perturbation of
/// this size turns the contour into noise the eye integrates away, and it is
/// what lets `the_wallpaper_neither_bands_nor_blocks` hold.
const GRAIN: f32 = 1.6;

fn srgb_to_linear(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.040_45 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f32) -> f32 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn linear(c: [u8; 3]) -> [f32; 3] {
    [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2])]
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// A uniform value in `[0, 1)` from a lattice point and a seed.
fn hash(x: i32, y: i32, seed: u32) -> f32 {
    let mut h =
        (x as u32).wrapping_mul(0x27d4_eb2d) ^ (y as u32).wrapping_mul(0x1656_67b1) ^ seed;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    (h >> 8) as f32 / (1u32 << 24) as f32
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Smooth value noise in `[0, 1)`, one lattice cell per unit of `x`/`y`.
fn value_noise(x: f32, y: f32, seed: u32) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (smoothstep(x - x0), smoothstep(y - y0));
    let (ix, iy) = (x0 as i32, y0 as i32);
    let top = hash(ix, iy, seed) + (hash(ix + 1, iy, seed) - hash(ix, iy, seed)) * fx;
    let bottom =
        hash(ix, iy + 1, seed) + (hash(ix + 1, iy + 1, seed) - hash(ix, iy + 1, seed)) * fx;
    top + (bottom - top) * fy
}

/// Two octaves of [`value_noise`], centred on zero and spanning `-1..1`.
fn drift(u: f32, v: f32, seed: u32) -> f32 {
    let coarse = value_noise(u * DRIFT_CELLS, v * DRIFT_CELLS, seed) * 2.0 - 1.0;
    let fine =
        value_noise(u * DRIFT_CELLS * 2.3 + 7.0, v * DRIFT_CELLS * 2.3 + 3.0, seed) * 2.0 - 1.0;
    coarse * 0.72 + fine * 0.28
}

/// Where the skyline of `ridge` sits above column `u`, as a fraction of the
/// screen from the top.
///
/// The `sharpness` mix is what makes a landscape out of a wave: folding the
/// noise about its midpoint (`1 - |2n - 1|`) turns smooth troughs into creases
/// and smooth peaks into points, and a ridge with none of it reads as water.
fn skyline(u: f32, ridge: &Ridge) -> f32 {
    let mut sum = 0.0;
    let mut weight = 0.0;
    let mut amplitude = 1.0;
    let mut frequency = ridge.frequency;
    for octave in 0..RIDGE_OCTAVES {
        let x = u * frequency;
        let x0 = x.floor();
        let t = smoothstep(x - x0);
        let (a, b) = (
            hash(x0 as i32, octave as i32, ridge.seed),
            hash(x0 as i32 + 1, octave as i32, ridge.seed),
        );
        let n = a + (b - a) * t;
        let ridged = 1.0 - (2.0 * n - 1.0).abs();
        sum += amplitude * (n + (ridged - n) * ridge.sharpness);
        weight += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    ridge.height - ridge.amplitude * (sum / weight - 0.5)
}

/// The wallpaper as `width * height` RGB triples.
pub fn draw(width: u32, height: u32) -> Vec<u8> {
    let sky_top = linear(SKY_TOP);
    let sky_horizon = linear(SKY_HORIZON);
    let light_colors: Vec<[f32; 3]> = LIGHTS.iter().map(|l| linear(l.color)).collect();
    let ridge_colors: Vec<([f32; 3], [f32; 3])> =
        RIDGES.iter().map(|r| (linear(r.crest), linear(r.base))).collect();

    // Each column's skyline is the same for every row of it, and there are
    // three of them per column against 1080 rows.
    let skylines: Vec<[f32; 3]> = (0..width)
        .map(|px| {
            let u = (px as f32 + 0.5) / width as f32;
            [skyline(u, &RIDGES[0]), skyline(u, &RIDGES[1]), skyline(u, &RIDGES[2])]
        })
        .collect();

    // One row in `v` units, which is the width of the ramp that keeps a
    // skyline from being a staircase.
    let row = 1.0 / height as f32;

    let mut out = Vec::with_capacity(width as usize * height as usize * 3);
    for py in 0..height {
        let v = (py as f32 + 0.5) / height as f32;
        for px in 0..width {
            let u = (px as f32 + 0.5) / width as f32;

            let mut rgb = mix(sky_top, sky_horizon, smoothstep(v / RIDGES[0].height));

            for (light, color) in LIGHTS.iter().zip(&light_colors) {
                let dx = (u - light.x) / light.rx;
                let dy = (v - light.y) / light.ry;
                let warped = (dx * dx + dy * dy) * (1.0 + DRIFT * drift(u, v, light.seed));
                let fall = (-warped).exp() * light.gain;
                for c in 0..3 {
                    rgb[c] += color[c] * fall;
                }
            }

            for (i, ridge) in RIDGES.iter().enumerate() {
                let horizon = skylines[px as usize][i];
                let coverage = ((v - horizon) / row + 0.5).clamp(0.0, 1.0);
                if coverage == 0.0 {
                    continue;
                }
                let (crest, base) = ridge_colors[i];
                let body = mix(crest, base, smoothstep((v - horizon) / ridge.depth));
                rgb = mix(rgb, body, coverage);
            }

            let (cx, cy) = ((u - 0.5) * 2.0, (v - 0.5) * 2.0);
            let radius = (cx * cx + cy * cy).sqrt();
            let vignette = 1.0
                - VIGNETTE * smoothstep((radius - VIGNETTE_IN) / (VIGNETTE_OUT - VIGNETTE_IN));

            for (c, value) in rgb.iter().enumerate() {
                let level = linear_to_srgb(value * vignette) * 255.0;
                // Triangular rather than uniform: the sum of two uniforms
                // leaves no correlation between the rounding error and the
                // signal, which is what stops the dither itself forming a
                // pattern along a slow gradient.
                let noise = hash(px as i32, py as i32, 0x2545_f491 + c as u32)
                    + hash(px as i32, py as i32, 0x7f4a_7c15 + c as u32)
                    - 1.0;
                out.push((level + noise * GRAIN).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    out
}

/// [`draw`] at [`WIDTH`]x[`HEIGHT`], JPEG-encoded at [`QUALITY`].
pub fn encoded() -> Vec<u8> {
    let rgb = draw(WIDTH, HEIGHT);
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, QUALITY)
        .encode(&rgb, WIDTH, HEIGHT, ExtendedColorType::Rgb8)
        .expect("encode the wallpaper");
    jpeg
}

/// Rewrite `assets/wallpaper.jpg` from the constants in this file.
pub fn regen(root: &Path) {
    let jpeg = encoded();
    let path = root.join(WALLPAPER_PATH);
    std::fs::write(&path, &jpeg).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    println!("wrote {} ({} bytes)", path.display(), jpeg.len());
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The longest run of one value straight down a column the finished picture
    /// may contain, in rows.
    ///
    /// A gradient with no dither in it is nothing but such runs, so this is
    /// what "it does not band" turns into as a property. The bound is above
    /// what a triangular dither leaves behind by chance and well below what a
    /// contour makes: drawn without the grain, this picture's worst column is
    /// almost four times it.
    const MAX_FLAT_RUN: usize = 40;

    /// How much more a pixel may differ from the one above it across an 8x8
    /// block boundary than inside one.
    ///
    /// JPEG's characteristic artifact *is* this ratio going up — the blocks are
    /// quantized independently, so what a person sees as blocking is a step
    /// that appears only where two of them meet. A picture with none of it
    /// sits at 1.0, and the value this bound separates from is a doubling.
    const MAX_BLOCKING: f64 = 1.25;

    fn longest_column_run(rgb: &[u8], width: usize, height: usize, channel: usize) -> usize {
        let mut worst = 0;
        for x in 0..width {
            let mut run = 1;
            for y in 1..height {
                let here = rgb[(y * width + x) * 3 + channel];
                let above = rgb[((y - 1) * width + x) * 3 + channel];
                run = if here == above { run + 1 } else { 1 };
                worst = worst.max(run);
            }
        }
        worst
    }

    fn blocking(rgb: &[u8], width: usize, height: usize) -> f64 {
        let (mut across, mut across_n) = (0.0f64, 0u64);
        let (mut within, mut within_n) = (0.0f64, 0u64);
        for y in 1..height {
            for x in 0..width {
                for c in 0..3 {
                    let step = (rgb[(y * width + x) * 3 + c] as i32
                        - rgb[((y - 1) * width + x) * 3 + c] as i32)
                        .abs() as f64;
                    if y % 8 == 0 {
                        across += step;
                        across_n += 1;
                    } else {
                        within += step;
                        within_n += 1;
                    }
                }
            }
        }
        (across / across_n as f64) / (within / within_n as f64)
    }

    /// What ships is the decoded JPEG, so the decoded JPEG is what is measured.
    ///
    /// Asking either question of [`draw`]'s own output would pass at an encoder
    /// setting that undoes both: the dither is the highest-frequency thing in
    /// the picture and therefore the first thing a quantizer throws away, and
    /// the staircase it was holding off comes back as blocking. Between the two
    /// they pin the encoder as well as the drawing — nothing here can be
    /// weakened without one of them saying so.
    #[test]
    fn the_wallpaper_neither_bands_nor_blocks() {
        let decoded = image::load_from_memory_with_format(&encoded(), image::ImageFormat::Jpeg)
            .expect("decode what we just encoded")
            .to_rgb8();
        let (w, h) = (decoded.width() as usize, decoded.height() as usize);
        for (channel, name) in ["red", "green", "blue"].iter().enumerate() {
            let run = longest_column_run(decoded.as_raw(), w, h, channel);
            assert!(
                run <= MAX_FLAT_RUN,
                "{run} rows of one {name} value down a column, over a bound of \
                 {MAX_FLAT_RUN}: this wallpaper bands"
            );
        }
        let blocking = blocking(decoded.as_raw(), w, h);
        assert!(
            blocking <= MAX_BLOCKING,
            "steps across the 8x8 block boundaries are {blocking:.2}x the steps inside them, \
             over a bound of {MAX_BLOCKING}: QUALITY = {QUALITY} is not enough for a field \
             this smooth"
        );
    }

    /// The committed file is [`draw`]'s output and not a picture somebody put
    /// there — the property this whole module exists to establish, and the
    /// answer to the licence question that replacing it was about.
    #[test]
    fn the_committed_wallpaper_is_the_one_this_file_describes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let on_disk = std::fs::read(root.join(WALLPAPER_PATH)).expect("read the wallpaper");
        assert!(
            on_disk == encoded(),
            "{WALLPAPER_PATH} is {} bytes and is not what this file draws — run \
             `cargo run -- --regen-wallpaper`",
            on_disk.len()
        );
    }
}
