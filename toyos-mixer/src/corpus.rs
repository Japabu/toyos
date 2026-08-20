//! The corpus every decision in this crate is held to, sample by sample.
//!
//! **This file is the certification and `fixtures/mix-corpus.txt` is the
//! verdict.** The transcript below is a pure function of the code beside it: it
//! runs every decision the mixer makes over inputs chosen to cover the space
//! that reaches them, and writes each answer as the bits it actually is —
//! `{:08x}` of an `f32`, the decimal of an `i16` — so no formatting rounds a
//! difference away. The committed fixture was produced by
//! `userland/soundd/src/main.rs` before a line of it moved here, and the test
//! that reads it asserts equality byte for byte. A change to any of this
//! crate's arithmetic reds it, and that is the point: **audible behaviour is
//! the owner's to change**, so it may not move under a refactor.
//!
//! Where a domain is small enough to be exhausted it is exhausted, and the
//! transcript carries an FNV-1a digest of every value the sweep produced plus
//! the handful of raw values that make a failure diagnosable. Where it is not,
//! the cases are constructed: both rails, the ties the quantizer has to round,
//! the lengths that are one frame off a period, and the client shapes that
//! actually connect.
//!
//! What is **not** here is the resampler, which is `rubato`'s and not ours.
//! What is here is everything on both sides of it: the decode that feeds it,
//! the planar append that fills it, the interleave that empties it, and the
//! accumulate and quantize that carry its output to the wire.

use crate::*;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

// ---------------------------------------------------------------------------
// Transcript primitives
// ---------------------------------------------------------------------------

/// FNV-1a over the exact bytes of everything a sweep produced.
///
/// A digest and not a sample: it is how a 65,536-value domain fits one line of
/// the fixture without giving up a single bit of what it covers. Any one bit of
/// any one value moves it.
struct Digest(u64);

impl Digest {
    fn new() -> Self {
        Digest(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn f32(&mut self, v: f32) {
        self.bytes(&v.to_bits().to_le_bytes());
    }

    fn f64(&mut self, v: f64) {
        self.bytes(&v.to_bits().to_le_bytes());
    }

    fn i16(&mut self, v: i16) {
        self.bytes(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }

    fn seal(&self) -> String {
        format!("{:016x}", self.0)
    }
}

/// The transcript under construction.
struct Out {
    text: String,
}

impl Out {
    fn new() -> Self {
        Out { text: String::new() }
    }

    fn line(&mut self, s: &str) {
        self.text.push_str(s);
        self.text.push('\n');
    }

    fn section(&mut self, name: &str) {
        self.line("");
        self.line(&format!("== {name}"));
    }

    /// One row of `f32`s as their bit patterns, wrapped at eight so a diff
    /// names a place rather than a line.
    fn f32s(&mut self, label: &str, v: &[f32]) {
        for (chunk, part) in v.chunks(8).enumerate() {
            let mut row = format!("{label}[{:04}]", chunk * 8);
            for x in part {
                row.push(' ');
                row.push_str(&format!("{:08x}", x.to_bits()));
            }
            self.line(&row);
        }
    }

    /// One row of `i16`s, wrapped at sixteen.
    fn i16s(&mut self, label: &str, v: &[i16]) {
        for (chunk, part) in v.chunks(16).enumerate() {
            let mut row = format!("{label}[{:04}]", chunk * 16);
            for x in part {
                row.push_str(&format!(" {x:6}"));
            }
            self.line(&row);
        }
    }
}

// ---------------------------------------------------------------------------
// The arithmetic soundd performs inline, mirrored here verbatim
// ---------------------------------------------------------------------------
//
// Each of these was an expression inside `userland/soundd/src/main.rs` when the
// fixture was captured, and each is a public function of this crate now. They
// are written out again rather than called so the transcript is the same
// program on both sides of the move; `the_crate_agrees_with_the_captured_shell`
// is where the crate's own versions are held against them.

/// `open_stream`: how many client frames cover one device period.
///
/// The manual ceiling is what the shell wrote, and being *unrewritten* is this
/// function's whole job: `client_period_frames` is the same arithmetic said the
/// way the standard library says it, and this is what holds it to the answer the
/// shell gave.
#[allow(clippy::manual_div_ceil)]
fn shell_client_period_frames(
    device_period_frames: u32,
    client_rate: u32,
    device_rate: u32,
) -> u32 {
    if client_rate != device_rate {
        ((device_period_frames as u64 * client_rate as u64 + device_rate as u64 - 1)
            / device_rate as u64) as u32
    } else {
        device_period_frames
    }
}

/// `mix_thread`: the widest client period the scratch buffers have to hold.
fn shell_scratch_frames(device_period_frames: usize, device_rate: usize) -> usize {
    (device_period_frames * MAX_CLIENT_RATE as usize).div_ceil(device_rate)
}

/// `mix_thread`/`null_sink_thread`: one device period in nanoseconds.
fn shell_period_nanos(device_period_frames: u64, device_rate: u64) -> u64 {
    (device_period_frames * 1_000_000_000) / device_rate
}

/// `run_with_device`: the ~5 ms connect/disconnect/volume ramp.
fn shell_ramp_frames(device_rate: u32) -> u32 {
    device_rate * 5 / 1000
}

/// `mix_client`, minus the shared memory the slot came out of.
///
/// The one path a client's audio takes when nothing resamples it: decode out of
/// the ring, convert the channel count if it differs, and accumulate onto the
/// bus under the ramp. `mix_interleaved` is this, and
/// `the_crate_agrees_with_the_captured_shell` is where it is held to it.
#[allow(clippy::too_many_arguments)]
fn shell_mix_slot(
    mix: &mut [f32],
    slot: &[u8],
    decode_buf: &mut [f32],
    convert_buf: &mut [f32],
    client_channels: usize,
    device_channels: usize,
    client_frames: usize,
    gain: &mut GainRamp,
) {
    let client_samples = client_frames * client_channels;
    assert!(client_samples <= decode_buf.len());
    decode_i16_to_f32(slot, &mut decode_buf[..client_samples]);
    let src: &[f32] = if client_channels != device_channels {
        let out_samples = client_frames * device_channels;
        assert!(out_samples <= convert_buf.len());
        match (client_channels, device_channels) {
            (1, 2) => channel_convert_mono_to_stereo(
                &decode_buf[..client_samples],
                &mut convert_buf[..out_samples],
            ),
            (2, 1) => channel_convert_stereo_to_mono(
                &decode_buf[..client_samples],
                &mut convert_buf[..out_samples],
            ),
            (c, d) => panic!("soundd: unsupported channel conversion {c}→{d}"),
        }
        &convert_buf[..out_samples]
    } else {
        &decode_buf[..client_samples]
    };
    accumulate(mix, src, device_channels, gain);
}

/// `mix_client`'s resampled tail: the planar frames rubato produced, laid back
/// out interleaved for the bus.
fn shell_interleave(planar: &[Vec<f32>], frames: usize, out: &mut [f32]) {
    let channels = planar.len();
    for frame in 0..frames {
        for ch in 0..channels {
            out[frame * channels + ch] = planar[ch][frame];
        }
    }
}

/// `mix_thread`: one mixed period, dithered and quantized into the DMA buffer.
fn shell_quantize_period(dst: &mut [i16], mix: &[f32], rng: &mut Xorshift32) {
    for i in 0..dst.len() {
        dst[i] = dither_and_quantize(mix[i], rng);
    }
}

// ---------------------------------------------------------------------------
// The inputs, and why they are the space
// ---------------------------------------------------------------------------

/// Every `f32` a sample can be that the quantizer has to decide about.
///
/// Both rails and one ULP either side of them; the half-LSB ties, which are the
/// only inputs where round-to-nearest has a choice to make; the overrange a sum
/// of clients reaches; and the small end, where a truncating quantizer used to
/// open a dead zone.
fn quantizer_edges() -> Vec<f32> {
    let mut v = Vec::new();
    let lsb = 1.0f32 / I16_SCALE;
    for anchor in [
        0.0f32,
        -0.0,
        1.0,
        -1.0,
        32767.0 / I16_SCALE,
        -32767.0 / I16_SCALE,
        0.5,
        -0.5,
        2.0,
        -2.0,
        1e-9,
        -1e-9,
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
    ] {
        v.push(anchor);
        v.push(f32::from_bits(anchor.to_bits().wrapping_add(1)));
        v.push(f32::from_bits(anchor.to_bits().wrapping_sub(1)));
        v.push(anchor + 0.5 * lsb);
        v.push(anchor - 0.5 * lsb);
        v.push(anchor + lsb);
        v.push(anchor - lsb);
    }
    // The exact half-way points at the small end, where the tie rule shows.
    for k in -4i32..=4 {
        v.push((k as f32 + 0.5) / I16_SCALE);
    }
    v
}

/// Every dither a TPDF generator can hand the quantizer, plus the two ties it
/// straddles.
fn dither_edges() -> Vec<f32> {
    vec![
        -1.0,
        -0.5000001,
        -0.5,
        -0.4999999,
        -1e-7,
        0.0,
        1e-7,
        0.4999999,
        0.5,
        0.5000001,
        1.0,
    ]
}

/// A client period of `frames` frames at `channels` channels, as the raw
/// little-endian `i16` its ring slot holds.
///
/// `kind` picks the signal: 0 silence, 1 both rails alternating, 2 a ramp
/// across the whole i16 range, 3 a half-scale square, 4 the negative rail held.
fn slot_bytes(frames: usize, channels: usize, kind: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(frames * channels * 2);
    for frame in 0..frames {
        for ch in 0..channels {
            let n = frame * channels + ch;
            let s: i16 = match kind {
                0 => 0,
                1 => {
                    if n.is_multiple_of(2) {
                        i16::MAX
                    } else {
                        i16::MIN
                    }
                }
                2 => (i16::MIN as i32 + ((n as i32 * 4099) % 65536)) as i16,
                3 => {
                    if (frame / 8) % 2 == 0 {
                        16384
                    } else {
                        -16384
                    }
                }
                _ => i16::MIN,
            };
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------

/// The whole corpus, as the text `fixtures/mix-corpus.txt` holds.
pub fn transcript() -> String {
    let mut o = Out::new();
    o.line("# toyos-mixer corpus: every decision, over the space that reaches it.");
    o.line("# f32 values are their bit patterns; i16 values are decimal.");
    o.line("# Captured from userland/soundd/src/main.rs before the extraction.");

    constants(&mut o);
    decode(&mut o);
    quantizer(&mut o);
    generator(&mut o);
    channels(&mut o);
    planar(&mut o);
    gains(&mut o);
    mixing(&mut o);
    shape(&mut o);
    rates(&mut o);
    clock(&mut o);
    stats(&mut o);
    scenes(&mut o);

    o.text
}

/// Every constant the arithmetic below is anchored on. A moved constant is a
/// moved decision, and it reds here before it reaches a speaker.
fn constants(o: &mut Out) {
    o.section("constants");
    o.line(&format!("I16_SCALE {:08x}", I16_SCALE.to_bits()));
    o.line(&format!("MAX_PIPELINE {MAX_PIPELINE}"));
    o.line(&format!("DEFERRAL_RESERVE {DEFERRAL_RESERVE}"));
    o.line(&format!("MIN_CLIENT_RATE {MIN_CLIENT_RATE}"));
    o.line(&format!("MAX_CLIENT_RATE {MAX_CLIENT_RATE}"));
}

/// The i16 domain is 65,536 values wide, so it is exhausted rather than
/// sampled: every one of them decoded, and every one of them back again through
/// an undithered quantizer. The round trip is the property `soundd` has asserted
/// since 2026-08-15 — 32,768 as the scale in both directions, so nothing gains
/// or loses an LSB in passing.
fn decode(o: &mut Out) {
    o.section("decode: every i16, exhaustive");
    let mut dec = Digest::new();
    let mut trip = Digest::new();
    let mut changed = 0u32;
    for s in i16::MIN..=i16::MAX {
        let mut out = [0.0f32; 1];
        decode_i16_to_f32(&s.to_le_bytes(), &mut out);
        dec.f32(out[0]);
        let back = quantize(out[0], 0.0);
        trip.i16(back);
        if back != s {
            changed += 1;
        }
    }
    o.line(&format!("decode-digest {}", dec.seal()));
    o.line(&format!("roundtrip-digest {}", trip.seal()));
    o.line(&format!("roundtrip-changed {changed}"));

    let rails: Vec<i16> = vec![i16::MIN, -32767, -16384, -1, 0, 1, 16384, 32766, i16::MAX];
    let mut decoded = vec![0.0f32; rails.len()];
    for (i, s) in rails.iter().enumerate() {
        decode_i16_to_f32(&s.to_le_bytes(), &mut decoded[i..i + 1]);
    }
    o.i16s("rails-in", &rails);
    o.f32s("rails-out", &decoded);

    // A whole period decoded at once, which is the shape the mix path uses.
    let slot = slot_bytes(9, 2, 2);
    let mut whole = vec![0.0f32; 18];
    decode_i16_to_f32(&slot, &mut whole);
    o.f32s("period-out", &whole);
}

/// Where the sum of every client becomes the i16 the device plays. Both rails,
/// the ties, the overrange, and the dither that can push a sample across one.
fn quantizer(o: &mut Out) {
    o.section("quantize: the rails, the ties and the overrange");
    for sample in quantizer_edges() {
        let mut row = format!("q {:08x}", sample.to_bits());
        for d in dither_edges() {
            row.push_str(&format!(" {}", quantize(sample, d)));
        }
        o.line(&row);
    }

    o.section("quantize: every half-way point in the i16 range, exhaustive");
    let mut ties = Digest::new();
    for k in -32769i32..=32768 {
        ties.i16(quantize((k as f32 + 0.5) / I16_SCALE, 0.0));
        ties.i16(quantize((k as f32 - 0.5) / I16_SCALE, 0.0));
    }
    o.line(&format!("tie-digest {}", ties.seal()));
    o.line(&format!("tie-below-zero {}", quantize(-0.5 / I16_SCALE, 0.0)));
    o.line(&format!("tie-above-zero {}", quantize(0.5 / I16_SCALE, 0.0)));
    o.line(&format!("tie-at-top {}", quantize(32766.5 / I16_SCALE, 0.0)));
    o.line(&format!("tie-at-bottom {}", quantize(-32767.5 / I16_SCALE, 0.0)));
}

/// The dither generator. A fixed seed makes the noise a sequence rather than a
/// cloud, and the sequence is what the fixture holds: nothing about the dither
/// may drift, because a listener hears the noise floor it sets.
fn generator(o: &mut Out) {
    o.section("dither: the generator");
    for seed in [0u32, 1, 2, 0x5eed_face, 0xffff_ffff, 1_234_567_891] {
        let mut rng = Xorshift32::new(seed);
        let first: Vec<f32> = (0..16).map(|_| rng.next()).collect();
        o.f32s(&format!("rng-{seed:08x}"), &first);
        let mut d = Digest::new();
        for _ in 0..65_536 {
            d.f32(rng.next());
        }
        o.line(&format!("rng-{seed:08x}-digest {}", d.seal()));
    }

    o.section("dither: the quantizer under it");
    for (name, sample) in [
        ("silence", 0.0f32),
        ("full-scale", 1.0f32),
        ("neg-full-scale", -1.0f32),
        ("half", 0.5f32),
        ("sub-lsb", 0.25f32 / I16_SCALE),
    ] {
        let mut rng = Xorshift32::new(0x5eed_face);
        let run: Vec<i16> = (0..64).map(|_| dither_and_quantize(sample, &mut rng)).collect();
        o.i16s(&format!("dq-{name}"), &run);
        let mut d = Digest::new();
        for _ in 0..16_384 {
            d.i16(dither_and_quantize(sample, &mut rng));
        }
        o.line(&format!("dq-{name}-digest {}", d.seal()));
    }
}

/// Mono to stereo and back, which is the whole of what the mixer converts.
fn channels(o: &mut Out) {
    o.section("channel conversion");
    for frames in [1usize, 2, 3, 127, 128, 129] {
        let slot = slot_bytes(frames, 1, 2);
        let mut mono = vec![0.0f32; frames];
        decode_i16_to_f32(&slot, &mut mono);
        let mut stereo = vec![0.0f32; frames * 2];
        channel_convert_mono_to_stereo(&mono, &mut stereo);
        let mut d = Digest::new();
        for x in &stereo {
            d.f32(*x);
        }
        o.line(&format!("mono-to-stereo-{frames} {}", d.seal()));
        if frames <= 3 {
            o.f32s(&format!("mono-to-stereo-{frames}-out"), &stereo);
        }
    }
    for frames in [1usize, 2, 3, 127, 128, 129] {
        let slot = slot_bytes(frames, 2, 1);
        let mut stereo = vec![0.0f32; frames * 2];
        decode_i16_to_f32(&slot, &mut stereo);
        let mut mono = vec![0.0f32; frames];
        channel_convert_stereo_to_mono(&stereo, &mut mono);
        let mut d = Digest::new();
        for x in &mono {
            d.f32(*x);
        }
        o.line(&format!("stereo-to-mono-{frames} {}", d.seal()));
        if frames <= 3 {
            o.f32s(&format!("stereo-to-mono-{frames}-out"), &mono);
        }
    }
    // Both rails through the downmix, where the average of two full-scale
    // samples of opposite sign is the one value a listener would hear as a
    // click if it were wrong.
    let rails: [f32; 8] = [1.0, -1.0, -1.0, 1.0, 1.0, 1.0, -1.0, -1.0];
    let mut down = [0.0f32; 4];
    channel_convert_stereo_to_mono(&rails, &mut down);
    o.f32s("downmix-rails", &down);
}

/// The planar path: what the resampler is fed, and what comes back out of it.
fn planar(o: &mut Out) {
    o.section("planar append and interleave");
    for (client_channels, device_channels) in [(1usize, 1usize), (2, 2), (1, 2), (2, 1)] {
        let frames = 7usize;
        let slot = slot_bytes(frames, client_channels, 2);
        let mut decoded = vec![0.0f32; frames * client_channels];
        decode_i16_to_f32(&slot, &mut decoded);
        let mut accum: Vec<Vec<f32>> =
            (0..device_channels).map(|_| Vec::with_capacity(frames * 3)).collect();
        append_planar(&decoded, client_channels, &mut accum);
        // A second period on top, because the buffer accumulates across calls
        // and the append has to land after what is already there.
        append_planar(&decoded, client_channels, &mut accum);
        for (ch, plane) in accum.iter().enumerate() {
            o.f32s(&format!("planar-{client_channels}to{device_channels}-ch{ch}"), plane);
        }
        let mut out = vec![0.0f32; frames * 2 * device_channels];
        shell_interleave(&accum, frames * 2, &mut out);
        o.f32s(&format!("interleave-{client_channels}to{device_channels}"), &out);
    }
}

/// Volume: what crosses the trust boundary, and the ramp that carries it.
fn gains(o: &mut Out) {
    o.section("gain: the trust boundary");
    for raw in [
        0.0f32,
        -0.0,
        1.0,
        0.5,
        -1.0,
        2.0,
        1.0000001,
        -0.0000001,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        -f32::NAN,
        f32::MIN_POSITIVE,
        f32::MAX,
        f32::MIN,
    ] {
        let word = match Gain::from_wire(raw) {
            Some(g) => format!("{:08x}", g.raw().to_bits()),
            None => "refused".to_string(),
        };
        o.line(&format!("from-wire {:08x} {word}", raw.to_bits()));
    }

    o.section("gain: the ramp");
    // Every shape the ramp is asked for: the shipped 5 ms at 44100, a ramp of
    // one frame, a retarget mid-ramp (which is a volume change landing on a
    // connect), and an advance past what is left.
    let mut ramp = GainRamp::new(Gain::SILENT);
    ramp.set_target(Gain::UNITY, 8);
    let mut levels: Vec<f32> = Vec::new();
    for _ in 0..12 {
        levels.push(ramp.next());
    }
    o.f32s("ramp-in-8", &levels);
    o.line(&format!("ramp-in-8-idle {}", ramp.is_idle()));

    let mut ramp = GainRamp::new(Gain::UNITY);
    ramp.set_target(Gain::SILENT, 220);
    let levels: Vec<f32> = (0..220).map(|_| ramp.next()).collect();
    let mut d = Digest::new();
    for x in &levels {
        d.f32(*x);
    }
    o.line(&format!("ramp-out-220-digest {}", d.seal()));
    o.f32s("ramp-out-220-head", &levels[..8]);
    o.f32s("ramp-out-220-tail", &levels[212..]);
    o.line(&format!("ramp-out-220-level {:08x}", ramp.level().to_bits()));

    let mut ramp = GainRamp::new(Gain::SILENT);
    ramp.set_target(Gain::UNITY, 220);
    for _ in 0..100 {
        ramp.next();
    }
    ramp.set_target(Gain::from_wire(0.25).unwrap(), 220);
    let levels: Vec<f32> = (0..8).map(|_| ramp.next()).collect();
    o.f32s("ramp-retarget", &levels);

    let mut ramp = GainRamp::new(Gain::UNITY);
    ramp.set_target(Gain::SILENT, 220);
    ramp.advance_frames(128);
    o.line(&format!("advance-128 {:08x} idle={}", ramp.level().to_bits(), ramp.is_idle()));
    ramp.advance_frames(1000);
    o.line(&format!("advance-past {:08x} idle={}", ramp.level().to_bits(), ramp.is_idle()));
    ramp.advance_frames(1);
    o.line(&format!("advance-idle {:08x} idle={}", ramp.level().to_bits(), ramp.is_idle()));

    let mut ramp = GainRamp::new(Gain::UNITY);
    ramp.set_target(Gain::SILENT, 1);
    o.line(&format!("ramp-1 {:08x}", ramp.next().to_bits()));
    o.line(&format!("ramp-1-again {:08x}", ramp.next().to_bits()));

    let mut ramp = GainRamp::new(Gain::UNITY);
    ramp.set_target(Gain::UNITY, 220);
    let levels: Vec<f32> = (0..4).map(|_| ramp.next()).collect();
    o.f32s("ramp-to-self", &levels);
}

/// The bus. Every gain state an accumulate can be in, over lengths that are one
/// frame short of a period and one frame past it, and enough clients on one bus
/// to drive it past both rails.
fn mixing(o: &mut Out) {
    o.section("accumulate: one client, every gain state");
    for channels in [1usize, 2] {
        for frames in [1usize, 2, 3, 127, 128, 129] {
            let samples = frames * channels;
            let slot = slot_bytes(frames, channels, 2);
            let mut src = vec![0.0f32; samples];
            decode_i16_to_f32(&slot, &mut src);

            for (name, mut gain) in [
                ("unity", GainRamp::new(Gain::UNITY)),
                ("silent", GainRamp::new(Gain::SILENT)),
                ("half", GainRamp::new(Gain::from_wire(0.5).unwrap())),
            ] {
                let mut mix = vec![0.0f32; samples];
                accumulate(&mut mix, &src, channels, &mut gain);
                let mut d = Digest::new();
                for x in &mix {
                    d.f32(*x);
                }
                o.line(&format!("acc-{channels}ch-{frames}f-{name} {}", d.seal()));
                if frames <= 3 {
                    o.f32s(&format!("acc-{channels}ch-{frames}f-{name}-out"), &mix);
                }
            }

            // A ramp that finishes inside the period and one that outlasts it:
            // the first is where `is_idle` flips mid-buffer, the second is the
            // ordinary 5 ms fade.
            for (name, ramp_frames) in [("short", 2u32), ("exact", frames as u32), ("long", 220)] {
                let mut gain = GainRamp::new(Gain::SILENT);
                gain.set_target(Gain::UNITY, ramp_frames);
                let mut mix = vec![0.0f32; samples];
                accumulate(&mut mix, &src, channels, &mut gain);
                let mut d = Digest::new();
                for x in &mix {
                    d.f32(*x);
                }
                o.line(&format!(
                    "acc-{channels}ch-{frames}f-ramp-{name} {} level={:08x}",
                    d.seal(),
                    gain.level().to_bits()
                ));
            }
        }
    }

    o.section("accumulate: saturation on the shared bus");
    // Four clients at full scale on one bus is +4.0, which the quantizer has to
    // clamp rather than wrap. This is the one place a sum leaves the range a
    // single client can reach.
    let frames = 64usize;
    let channels = 2usize;
    let samples = frames * channels;
    let mut mix = vec![0.0f32; samples];
    for kind in [1u32, 1, 1, 1] {
        let slot = slot_bytes(frames, channels, kind);
        let mut src = vec![0.0f32; samples];
        decode_i16_to_f32(&slot, &mut src);
        let mut gain = GainRamp::new(Gain::UNITY);
        accumulate(&mut mix, &src, channels, &mut gain);
    }
    o.f32s("saturate-bus", &mix[..16]);
    let mut out = vec![0i16; samples];
    let mut rng = Xorshift32::new(0x5eed_face);
    shell_quantize_period(&mut out, &mix, &mut rng);
    o.i16s("saturate-out", &out[..32]);
    let mut d = Digest::new();
    for x in &out {
        d.i16(*x);
    }
    o.line(&format!("saturate-digest {}", d.seal()));

    // And the cancelling case: two clients exactly out of phase must leave the
    // bus at zero rather than at a rounding residue.
    let mut mix = vec![0.0f32; samples];
    for kind in [1u32, 4] {
        let slot = slot_bytes(frames, channels, kind);
        let mut src = vec![0.0f32; samples];
        decode_i16_to_f32(&slot, &mut src);
        let mut gain = GainRamp::new(Gain::UNITY);
        accumulate(&mut mix, &src, channels, &mut gain);
    }
    o.f32s("cancel-bus", &mix[..8]);
}

/// Which device shapes the mixer can render a period into, and which it refuses
/// by name so the machine gets the null sink instead of a dead daemon.
fn shape(o: &mut Out) {
    o.section("device shape");
    for buffers in [0usize, 1, 2, 3, 4, 6, 8, 16, 17, 32] {
        for channels in [0u16, 1, 2, 3, 6] {
            for bytes in [0usize, 1, 2, 4, 511, 512, 513, 1024] {
                let word = match period_frames(buffers, channels, bytes) {
                    Ok(frames) => format!("ok {frames}"),
                    Err(why) => format!("refused {why}"),
                };
                o.line(&format!("shape {buffers} {channels} {bytes} -> {word}"));
            }
        }
    }

    o.section("deferral floor");
    for buffers in 0usize..=16 {
        let word = match deferral_floor_nanos(buffers, 2_902_494) {
            Some(floor) => format!("{floor}"),
            None => "none".to_string(),
        };
        o.line(&format!("floor {buffers} -> {word}"));
    }
}

/// The rate arithmetic, over every rate a client may ask for.
///
/// The invariant underneath it is a buffer bound, not a nicety: `mix_client`
/// asserts a client's period fits the scratch the mix thread allocated once,
/// and the scratch is sized from `MAX_CLIENT_RATE`. A rate that broke the
/// inequality would be an assertion in the mix loop, on a client's word.
fn rates(o: &mut Out) {
    o.section("client period sizing");
    for device_rate in [44_100u32, 48_000, 96_000, 192_000] {
        for device_period_frames in [64u32, 128, 256] {
            let scratch = shell_scratch_frames(device_period_frames as usize, device_rate as usize);
            let mut d = Digest::new();
            let mut worst = 0u32;
            for client_rate in MIN_CLIENT_RATE..=MAX_CLIENT_RATE {
                let frames =
                    shell_client_period_frames(device_period_frames, client_rate, device_rate);
                d.u64(frames as u64);
                worst = worst.max(frames);
            }
            o.line(&format!(
                "sizing {device_rate} {device_period_frames} scratch={scratch} worst={worst} digest={}",
                d.seal()
            ));
        }
    }
    for (device_rate, client_rate) in [
        (44_100u32, 44_100u32),
        (44_100, 8_000),
        (44_100, 192_000),
        (44_100, 44_099),
        (44_100, 44_101),
        (48_000, 44_100),
        (48_000, 48_000),
    ] {
        o.line(&format!(
            "sizing-one {device_rate} {client_rate} -> {}",
            shell_client_period_frames(128, client_rate, device_rate)
        ));
    }
}

/// The two numbers every timing decision below the mixer is derived from, and
/// the delay-locked loop that tracks the device's own grid.
fn clock(o: &mut Out) {
    o.section("period and ramp");
    for rate in [8_000u64, 44_100, 48_000, 96_000, 192_000] {
        for frames in [64u64, 128, 256] {
            o.line(&format!(
                "period-nanos {rate} {frames} -> {}",
                shell_period_nanos(frames, rate)
            ));
        }
        o.line(&format!("ramp-frames {rate} -> {}", shell_ramp_frames(rate as u32)));
    }

    o.section("dll");
    let nominal = 2_902_494.0f64;
    let mut dll = Dll::new(nominal);
    let mut d = Digest::new();
    // A grid that runs a little fast, then a batch of four, then a stall, then
    // a reset — every input the completion path hands it.
    let mut t = 1_000_000_000.0f64;
    for step in 0..64u32 {
        let n = match step % 8 {
            0 => 4,
            3 => 2,
            _ => 1,
        };
        t += nominal * n as f64 * if step % 5 == 0 { 1.0004 } else { 0.9997 };
        dll.update(t, n);
        d.f64(dll.period);
        d.f64(dll.t_estimated.unwrap());
        if step < 6 {
            o.line(&format!(
                "dll-{step} period={:016x} est={:016x}",
                dll.period.to_bits(),
                dll.t_estimated.unwrap().to_bits()
            ));
        }
    }
    o.line(&format!("dll-digest {}", d.seal()));
    o.line(&format!("dll-period {:016x}", dll.period.to_bits()));

    // The clamp: a timestamp far off the grid may not collapse or run away the
    // period estimate.
    let mut dll = Dll::new(nominal);
    dll.update(0.0, 1);
    for k in 1..64u32 {
        dll.update(k as f64 * nominal * 8.0, 1);
    }
    o.line(&format!("dll-runaway-period {:016x}", dll.period.to_bits()));
    let mut dll = Dll::new(nominal);
    dll.update(0.0, 1);
    for k in 1..64u32 {
        dll.update(k as f64 * nominal * 0.01, 1);
    }
    o.line(&format!("dll-collapse-period {:016x}", dll.period.to_bits()));
    dll.reset();
    o.line(&format!(
        "dll-reset period={:016x} est={:?}",
        dll.period.to_bits(),
        dll.t_estimated
    ));
}

/// What the audio gate reads. Every counter has to mean exactly one thing, and
/// the run length is the one that separates a client that never had margin from
/// one that lost it.
fn stats(o: &mut Out) {
    o.section("stats");
    let script: [(bool, bool); 24] = [
        (false, false),
        (false, true),
        (true, true),
        (true, false),
        (true, false),
        (true, false),
        (true, true),
        (true, false),
        (false, false),
        (true, false),
        (true, false),
        (true, false),
        (true, false),
        (true, true),
        (true, true),
        (true, false),
        (false, true),
        (true, false),
        (true, true),
        (true, false),
        (true, false),
        (true, false),
        (true, false),
        (true, false),
    ];
    let mut stats = MixStats::default();
    for (i, (streaming, covered)) in script.iter().enumerate() {
        stats.period(*streaming, *covered);
        o.line(&format!(
            "stats-{i} underruns={} starve_run={} starve_max={}",
            stats.underruns, stats.starve_run, stats.starve_max
        ));
    }
}

/// **The whole chain, end to end.** Clients of different shapes on one bus, each
/// under its own ramp, decoded out of raw ring bytes and carried to the i16 the
/// device plays — which is the only output a listener ever hears.
///
/// Eight consecutive periods, because the ramps and the dither generator both
/// carry state across one: a scene that mixed a single period would certify
/// neither.
fn scenes(o: &mut Out) {
    o.section("scenes: the whole chain");
    for (name, device_channels, device_period_frames) in [
        ("stereo-128", 2usize, 128usize),
        ("mono-128", 1usize, 128usize),
        ("stereo-1", 2usize, 1usize),
        ("stereo-3", 2usize, 3usize),
        ("stereo-127", 2usize, 127usize),
        ("stereo-129", 2usize, 129usize),
    ] {
        let device_period_samples = device_period_frames * device_channels;
        let scratch = shell_scratch_frames(device_period_frames, 44_100);

        // Four clients: one at the device's own shape and full gain, one mono
        // fading in, one stereo fading out, and one silent. Between them they
        // cover both channel conversions, both ramp directions and the idle
        // fast path.
        let mut clients: Vec<(usize, u32, GainRamp)> = Vec::new();
        clients.push((2, 1, GainRamp::new(Gain::UNITY)));
        let mut fading_in = GainRamp::new(Gain::SILENT);
        fading_in.set_target(Gain::UNITY, 220);
        clients.push((1, 2, fading_in));
        let mut fading_out = GainRamp::new(Gain::UNITY);
        fading_out.set_target(Gain::SILENT, 220);
        clients.push((2, 3, fading_out));
        clients.push((2, 0, GainRamp::new(Gain::from_wire(0.5).unwrap())));

        let mut decode_buf = vec![0.0f32; scratch * 2];
        let mut convert_buf = vec![0.0f32; scratch * 2];
        let mut mix = vec![0.0f32; device_period_samples];
        let mut dma = vec![0i16; device_period_samples];
        let mut rng = Xorshift32::new(0x0123_4567);
        let mut d = Digest::new();

        for period in 0..8usize {
            mix.iter_mut().for_each(|s| *s = 0.0);
            for (client_channels, kind, gain) in clients.iter_mut() {
                let slot = slot_bytes(device_period_frames, *client_channels, *kind + period as u32 % 2);
                shell_mix_slot(
                    &mut mix,
                    &slot,
                    &mut decode_buf,
                    &mut convert_buf,
                    *client_channels,
                    device_channels,
                    device_period_frames,
                    gain,
                );
            }
            shell_quantize_period(&mut dma, &mix, &mut rng);
            for x in &dma {
                d.i16(*x);
            }
            if period == 0 || period == 7 {
                o.i16s(
                    &format!("scene-{name}-p{period}"),
                    &dma[..device_period_samples.min(32)],
                );
            }
        }
        o.line(&format!("scene-{name}-digest {}", d.seal()));
        for (i, (_, _, gain)) in clients.iter().enumerate() {
            o.line(&format!("scene-{name}-gain{i} {:08x}", gain.level().to_bits()));
        }
    }

    o.section("scenes: a client that supplies nothing");
    // The ramp still advances across a period of silence, or a closing client
    // would never finish fading and never be removed.
    let mut gain = GainRamp::new(Gain::UNITY);
    gain.set_target(Gain::SILENT, 220);
    for period in 0..3usize {
        gain.advance_frames(128);
        o.line(&format!("starved-{period} {:08x} idle={}", gain.level().to_bits(), gain.is_idle()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The gate this crate exists behind.**
    ///
    /// `fixtures/mix-corpus.txt` was written by `userland/soundd/src/main.rs`
    /// before a line of it moved here — the same generator above, over the same
    /// inputs, calling soundd's own inline functions. Every value in it is the
    /// bits the shipped mixer produced. If this crate reproduces the file byte
    /// for byte then the extraction changed nothing a listener could hear, and
    /// if it does not then it did.
    ///
    /// A red here is never fixed by regenerating the fixture. It is a report
    /// that the arithmetic moved, and what a speaker plays is the owner's to
    /// change.
    #[test]
    fn the_corpus_is_reproduced_bit_for_bit() {
        let captured = include_str!("../fixtures/mix-corpus.txt");
        let now = transcript();
        if now == captured {
            return;
        }
        let mut first = String::new();
        for (n, (a, b)) in now.lines().zip(captured.lines()).enumerate() {
            if a != b {
                first = format!("line {}:\n  now      {a}\n  captured {b}", n + 1);
                break;
            }
        }
        if first.is_empty() {
            first = format!(
                "the transcript is {} lines and the fixture is {}",
                now.lines().count(),
                captured.lines().count()
            );
        }
        panic!(
            "this crate no longer computes what userland/soundd/src/main.rs computed.\n{first}\n\
             This is a change to what a speaker plays. Do not regenerate the fixture."
        );
    }

    /// The composites this crate publishes are the compositions the shell used
    /// to write inline — the corpus above exercises the inline ones, so this is
    /// what carries its verdict onto the functions soundd actually calls.
    #[test]
    fn the_crate_agrees_with_the_captured_shell() {
        for device_rate in [44_100u32, 48_000, 96_000, 192_000] {
            for frames in [1u32, 3, 64, 127, 128, 129, 256] {
                assert_eq!(
                    scratch_frames(frames as usize, device_rate as usize),
                    shell_scratch_frames(frames as usize, device_rate as usize),
                );
                assert_eq!(
                    period_nanos(frames as u64, device_rate as u64),
                    shell_period_nanos(frames as u64, device_rate as u64),
                );
                assert_eq!(ramp_frames(device_rate), shell_ramp_frames(device_rate));
                for client_rate in [MIN_CLIENT_RATE, 22_050, 44_100, 48_000, MAX_CLIENT_RATE] {
                    assert_eq!(
                        client_period_frames(frames, client_rate, device_rate),
                        shell_client_period_frames(frames, client_rate, device_rate),
                    );
                }
            }
        }

        for (client_channels, device_channels) in [(1usize, 1usize), (2, 2), (1, 2), (2, 1)] {
            for frames in [1usize, 3, 127, 128, 129] {
                let slot = slot_bytes(frames, client_channels, 2);
                let samples = frames * client_channels;

                let mut shell_mix = vec![0.0f32; frames * device_channels];
                let mut decode_buf = vec![0.0f32; samples.max(frames * device_channels)];
                let mut convert_buf = vec![0.0f32; frames * 2];
                let mut gain = GainRamp::new(Gain::SILENT);
                gain.set_target(Gain::UNITY, 220);
                shell_mix_slot(
                    &mut shell_mix,
                    &slot,
                    &mut decode_buf,
                    &mut convert_buf,
                    client_channels,
                    device_channels,
                    frames,
                    &mut gain,
                );

                let mut crate_mix = vec![0.0f32; frames * device_channels];
                let mut decoded = vec![0.0f32; samples];
                decode_i16_to_f32(&slot, &mut decoded);
                let mut convert_buf = vec![0.0f32; frames * 2];
                let mut crate_gain = GainRamp::new(Gain::SILENT);
                crate_gain.set_target(Gain::UNITY, 220);
                mix_interleaved(
                    &mut crate_mix,
                    &decoded,
                    &mut convert_buf,
                    client_channels,
                    device_channels,
                    &mut crate_gain,
                );

                for (a, b) in crate_mix.iter().zip(shell_mix.iter()) {
                    assert_eq!(a.to_bits(), b.to_bits(), "{client_channels}→{device_channels}");
                }
                assert_eq!(crate_gain.level().to_bits(), gain.level().to_bits());

                // The DMA half, generator state included.
                let mut shell_out = vec![0i16; shell_mix.len()];
                let mut rng = Xorshift32::new(11);
                shell_quantize_period(&mut shell_out, &shell_mix, &mut rng);
                let mut crate_out = vec![0i16; crate_mix.len()];
                let mut crate_rng = Xorshift32::new(11);
                quantize_period(&mut crate_out, &crate_mix, &mut crate_rng);
                assert_eq!(crate_out, shell_out);
                assert_eq!(rng.next().to_bits(), crate_rng.next().to_bits());
            }
        }

        // And the resampler's planar tail.
        for channels in [1usize, 2] {
            let planar: Vec<Vec<f32>> =
                (0..channels).map(|c| (0..9).map(|i| (i + c * 9) as f32 * 0.01).collect()).collect();
            let mut shell_out = vec![0.0f32; 7 * channels];
            shell_interleave(&planar, 7, &mut shell_out);
            let mut crate_out = vec![0.0f32; 7 * channels];
            interleave(&planar, 7, &mut crate_out);
            assert_eq!(crate_out, shell_out);
        }
    }

    /// The corpus is only worth its file size if it is a fine sieve. A change
    /// of one LSB anywhere in the arithmetic has to move it — shown, rather
    /// than assumed, on the two places a plausible refactor would land: the
    /// scale the quantizer rounds on, and the state of the dither generator.
    #[test]
    fn a_one_lsb_change_anywhere_moves_the_transcript() {
        let captured = include_str!("../fixtures/mix-corpus.txt");

        // The corpus records `roundtrip-changed 0`. Quantizing by 32767 while
        // decoding by 32768 — the asymmetry `I16_SCALE`'s comment warns about —
        // moves that line to a number, so the line is a live sieve and not a
        // constant.
        let mut changed = 0u32;
        for s in i16::MIN..=i16::MAX {
            let mut decoded = [0.0f32; 1];
            decode_i16_to_f32(&s.to_le_bytes(), &mut decoded);
            let wrong = round_ties_away(decoded[0] * 32767.0).clamp(-32768.0, 32767.0) as i16;
            if wrong != s {
                changed += 1;
            }
        }
        assert_eq!(
            changed, 32703,
            "the wrong scale has to change most of the domain, or the corpus proves nothing",
        );
        assert!(captured.contains("roundtrip-changed 0"));

        // A dither generator one draw out of step: every value after it
        // differs, and the corpus holds five runs of them.
        let mut a = Xorshift32::new(1);
        let mut b = Xorshift32::new(1);
        b.next();
        assert_ne!(a.next().to_bits(), b.next().to_bits());

        // And the fixture is actually being read, rather than an empty string
        // comparing equal to an empty transcript.
        assert!(captured.len() > 40_000, "the fixture is {} bytes", captured.len());
        assert!(transcript().len() > 40_000);
    }
}
