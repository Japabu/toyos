//! Between a client's `i16` and the bus's `f32`, and back again at the wire.
//!
//! Two conversions and a noise generator, and every one of them is audible: the
//! decode is what a client hears played back, the quantizer is where the sum of
//! every client becomes the sixteen bits the device takes, and the dither is the
//! noise floor a listener hears under a quiet passage.

/// One scale for both directions of the i16 <-> f32 conversion.
///
/// Decoding by 32768 and quantizing by 32767 is not a round trip: it is a gain
/// of 32767/32768 on everything that passes through, and 32703 of the 65536
/// i16 values come back one LSB different from what the client sent. 32768 is
/// the correct constant in both directions because it is the magnitude of
/// `i16::MIN`; the positive end is one code short of full scale, which is what
/// the clamp is for and what two's complement costs.
pub const I16_SCALE: f32 = 32768.0;

/// One client period out of its ring, as `f32` on the bus's scale.
///
/// `dst` is what is decoded and `src` has to carry two bytes for each of it;
/// the caller sizes both from the client's own period, which is why this
/// indexes rather than zips.
pub fn decode_i16_to_f32(src: &[u8], dst: &mut [f32]) {
    for i in 0..dst.len() {
        let sample = i16::from_le_bytes([src[i * 2], src[i * 2 + 1]]);
        dst[i] = sample as f32 / I16_SCALE;
    }
}

/// The dither generator.
///
/// Cheap and seedable, which is what a per-period noise source has to be. A
/// listener hears its output as the noise floor, so nothing about the sequence
/// may drift: `corpus` holds a run of it from six seeds.
///
/// **Two draws of this are a real TPDF and not an approximation of one**, which
/// is not obvious: they are *consecutive* states of one linear generator rather
/// than two independent sources, and a linear generator's successive outputs
/// are related by a map over GF(2). Measured over 100,000,000 sums
/// (`the_dither_is_a_triangular_distribution_and_not_merely_shaped_like_one`):
/// mean 7.6e-6, variance 0.166664 against the ideal 1/6, kurtosis 2.399725
/// against the ideal 2.4, and a lag-1 autocorrelation of 2.1e-4. The
/// correlation the structure could have produced is not there.
pub struct Xorshift32(u32);

impl Xorshift32 {
    /// Seeded so it can never be zero, which is the one state this generator
    /// cannot leave — a zero register xorshifts to zero forever, and the dither
    /// would be a constant.
    pub fn new(seed: u32) -> Self {
        Self(seed | 1)
    }

    /// One uniform draw in [-0.5, 0.5].
    // Not `Iterator`: this never ends, and an iterator that never returns
    // `None` is a trap for every adaptor that reads one.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f32) / (u32::MAX as f32) - 0.5
    }
}

/// §5.4. TPDF dither is defined against a **round-to-nearest** quantizer; that
/// pairing is what makes the error zero-mean and its variance
/// signal-independent. `as i16` truncates instead, which biases every sample
/// 0.5 LSB toward zero and swallows the dither whole — a 2-LSB dead zone at
/// the zero crossing and a noise floor that collapses with the signal.
pub fn dither_and_quantize(sample: f32, rng: &mut Xorshift32) -> i16 {
    let dither = rng.next() + rng.next(); // triangular PDF in [-1.0, 1.0]
    quantize(sample, dither)
}

/// Split out from `dither_and_quantize` so the scale can be checked against
/// every i16 there is without a generator in the way.
pub fn quantize(sample: f32, dither: f32) -> i16 {
    round_ties_away(sample * I16_SCALE + dither).clamp(-32768.0, 32767.0) as i16
}

/// One mixed period, dithered and quantized into the buffer the device plays.
///
/// The generator is the caller's and carries across periods on purpose: a
/// dither restarted every period is a periodic signal at the period rate, which
/// is a tone rather than a noise floor.
pub fn quantize_period(dst: &mut [i16], mix: &[f32], rng: &mut Xorshift32) {
    for i in 0..dst.len() {
        dst[i] = dither_and_quantize(mix[i], rng);
    }
}

/// `x` rounded to the nearest integer, halves away from zero.
///
/// **`core` has no `f32::round`,** and this crate names nothing outside `core`
/// and `alloc` — the property that keeps it host-tested. So the quantizer's
/// rounding is ours, and it is held to `std`'s over *every* `f32` there is
/// rather than argued about: see
/// `the_rounding_is_std_s_over_every_float_there_is`.
///
/// Infinities and NaN pass through, which is what `f32::round` does with them
/// too; the clamp in [`quantize`] is what turns the first into a rail and the
/// cast is what turns the second into silence.
pub fn round_ties_away(x: f32) -> f32 {
    let t = trunc_toward_zero(x);
    // Exact: `x` and `t` agree above the binary point, so the difference is
    // representable. For |x| >= 2^23 there is no fraction and `t` is `x`.
    let frac = x - t;
    if frac >= 0.5 {
        t + 1.0
    } else if frac <= -0.5 {
        t - 1.0
    } else {
        t
    }
}

/// `x` with its fractional part removed, keeping its sign.
///
/// By exponent rather than by arithmetic: a magic-constant round trip rounds to
/// *even* on the way, which is the tie rule this quantizer does not use.
fn trunc_toward_zero(x: f32) -> f32 {
    const SIGN: u32 = 0x8000_0000;
    let bits = x.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    if exponent < 0 {
        // |x| < 1, subnormals and both zeros: the integer part is the sign.
        return f32::from_bits(bits & SIGN);
    }
    if exponent >= 23 {
        // No fractional bits left in the significand — and the same arm carries
        // the infinities and the NaNs, whose integer part is themselves.
        return x;
    }
    let fraction = (1u32 << (23 - exponent)) - 1;
    f32::from_bits(bits & !fraction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// A client playing i16 at the device's own rate and channel count must get
    /// its own bytes back. Nothing resamples or mixes on that path, so any
    /// difference here is a gain nobody asked for.
    #[test]
    fn passthrough_is_bit_exact_for_every_i16() {
        let mut changed = 0;
        for s in i16::MIN..=i16::MAX {
            let mut decoded = [0.0f32; 1];
            decode_i16_to_f32(&s.to_le_bytes(), &mut decoded);
            if quantize(decoded[0], 0.0) != s {
                changed += 1;
            }
        }
        assert_eq!(changed, 0, "{changed} of 65536 i16 values do not survive a passthrough");
    }

    /// Both rails, named explicitly: `i16::MIN` is the value the scale is
    /// derived from, and `i16::MAX` is the one the clamp has to catch rather
    /// than wrap.
    #[test]
    fn full_scale_clamps_instead_of_wrapping() {
        assert_eq!(quantize(-1.0, 0.0), i16::MIN);
        assert_eq!(quantize(1.0, 0.0), i16::MAX);
        assert_eq!(quantize(-2.0, 0.0), i16::MIN, "overrange must clamp");
        assert_eq!(quantize(2.0, 0.0), i16::MAX, "overrange must clamp");
        // Dither may not push an in-range sample past a rail either.
        assert_eq!(quantize(-1.0, -1.0), i16::MIN);
        assert_eq!(quantize(1.0, 1.0), i16::MAX);
    }

    /// **The rounding this crate had to write for itself, held to the one it
    /// replaced over its entire domain.**
    ///
    /// `core` has no `f32::round`, so [`round_ties_away`] is ours — and a
    /// quantizer that rounds differently from the one soundd shipped is a
    /// change to what a speaker plays. All 2^32 bit patterns are walked, which
    /// is the whole input space and not a sample of it: at the quantizer, where
    /// the answer is the `i16` a device takes, the two agree at every single
    /// one.
    ///
    /// The raw rounding differs on exactly the NaNs, and only in the payload —
    /// `f32::round` canonicalises one, this keeps it — which is why the
    /// quantizer's count is the one that has to be zero and the raw count is
    /// merely reported as NaN-only.
    #[test]
    fn the_rounding_is_std_s_over_every_float_there_is() {
        const THREADS: u64 = 8;
        let span = (1u64 << 32) / THREADS;
        let mut workers = Vec::new();
        for k in 0..THREADS {
            let lo = k * span;
            let hi = if k == THREADS - 1 { 1u64 << 32 } else { lo + span };
            workers.push(std::thread::spawn(move || {
                let (mut quantized, mut raw, mut raw_not_nan) = (0u64, 0u64, 0u64);
                for bits in lo..hi {
                    let x = f32::from_bits(bits as u32);
                    let ours = round_ties_away(x);
                    let theirs = x.round();
                    if ours.to_bits() != theirs.to_bits() {
                        raw += 1;
                        if !x.is_nan() {
                            raw_not_nan += 1;
                        }
                    }
                    let a = ours.clamp(-32768.0, 32767.0) as i16;
                    let b = theirs.clamp(-32768.0, 32767.0) as i16;
                    if a != b {
                        quantized += 1;
                    }
                }
                (quantized, raw, raw_not_nan)
            }));
        }
        let (mut quantized, mut raw, mut raw_not_nan) = (0u64, 0u64, 0u64);
        for w in workers {
            let (q, r, n) = w.join().expect("a sweep of a quarter of the f32 space");
            quantized += q;
            raw += r;
            raw_not_nan += n;
        }
        assert_eq!(
            quantized, 0,
            "{quantized} of the 4294967296 f32 values quantize differently than they did \
             before `round` became ours — which is a change to what a speaker plays",
        );
        assert_eq!(
            raw_not_nan, 0,
            "{raw_not_nan} non-NaN values round differently; only a NaN's payload may differ \
             (of {raw} raw differences in total)",
        );
    }

    /// The tie rule, named rather than left to the sweep above: halves go away
    /// from zero, which is what TPDF dither is defined against.
    #[test]
    fn a_half_rounds_away_from_zero() {
        assert_eq!(round_ties_away(0.5).to_bits(), 1.0f32.to_bits());
        assert_eq!(round_ties_away(-0.5).to_bits(), (-1.0f32).to_bits());
        assert_eq!(round_ties_away(1.5).to_bits(), 2.0f32.to_bits());
        assert_eq!(round_ties_away(2.5).to_bits(), 3.0f32.to_bits(), "not to even");
        assert_eq!(round_ties_away(-2.5).to_bits(), (-3.0f32).to_bits(), "not to even");
        // The sign of a zero survives, which a naive `x + 0.5` floor would lose.
        assert_eq!(round_ties_away(-0.25).to_bits(), (-0.0f32).to_bits());
        assert_eq!(round_ties_away(0.25).to_bits(), 0.0f32.to_bits());
    }

    /// A generator that reached zero would emit a constant forever, and a
    /// constant offset under the quantizer is a DC bias rather than dither.
    #[test]
    fn the_generator_cannot_be_seeded_into_silence() {
        for seed in [0u32, 1, 2, u32::MAX, 0x8000_0000] {
            let mut rng = Xorshift32::new(seed);
            let first = rng.next();
            let mut all_same = true;
            for _ in 0..64 {
                if rng.next() != first {
                    all_same = false;
                }
            }
            assert!(!all_same, "seed {seed} produced a constant");
        }
        // The constructor is the `| 1` the mix thread used to write at the call
        // site, and it has to be exactly that: two seeds one apart share a
        // stream.
        let mut a = Xorshift32::new(0);
        let mut b = Xorshift32::new(1);
        assert_eq!(a.next().to_bits(), b.next().to_bits());
    }

    /// The draw is a uniform in [-0.5, 0.5], so two of them are a triangular
    /// PDF in [-1, 1] — and a dither wider than that would move a sample by
    /// more than an LSB, which is a distortion rather than a noise floor.
    #[test]
    fn the_draw_stays_inside_half_an_lsb() {
        let mut rng = Xorshift32::new(0x5eed_face);
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for _ in 0..1_000_000 {
            let x = rng.next();
            assert!((-0.5..=0.5).contains(&x), "{x} is outside the uniform's range");
            lo = lo.min(x);
            hi = hi.max(x);
        }
        assert!(lo < -0.49 && hi > 0.49, "the generator covered only [{lo}, {hi}]");
    }

    /// **What the dither is for, measured rather than asserted.** TPDF dither
    /// against a round-to-nearest quantizer makes the error zero-mean and its
    /// variance signal-independent, and the consequence a listener hears is
    /// that a signal *below* one LSB still comes through — as a level in the
    /// mean of the output rather than as a code.
    ///
    /// The old truncating quantizer had a two-LSB dead zone at the zero
    /// crossing; every level below would have come out as exactly zero here.
    #[test]
    fn a_signal_under_one_lsb_survives_the_quantizer() {
        for level in [0.0f32, 0.1, 0.25, 0.5, 0.75, 0.9] {
            let mut rng = Xorshift32::new(0x5eed_face);
            let sample = level / I16_SCALE;
            let mut sum = 0.0f64;
            let draws = 2_000_000;
            for _ in 0..draws {
                sum += dither_and_quantize(sample, &mut rng) as f64;
            }
            let mean = sum / draws as f64;
            assert!(
                (mean - level as f64).abs() < 0.005,
                "a {level}-LSB signal came out at {mean}",
            );
        }
    }

    /// **Two draws of one linear generator are a real triangular distribution.**
    /// They are consecutive states rather than independent sources, so the sum
    /// could have carried the generator's own structure; the moments say it does
    /// not. The numbers in [`Xorshift32`]'s header come from a 100,000,000-sum
    /// run of this, and the bounds here are loosened for the shorter one.
    #[test]
    fn the_dither_is_a_triangular_distribution_and_not_merely_shaped_like_one() {
        let mut rng = Xorshift32::new(0x5eed_face);
        let n = 4_000_000u64;
        let (mut s1, mut s2, mut s4, mut cross) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        let mut previous = 0.0f64;
        for i in 0..n {
            let d = (rng.next() + rng.next()) as f64;
            s1 += d;
            s2 += d * d;
            s4 += d * d * d * d;
            if i > 0 {
                cross += d * previous;
            }
            previous = d;
            assert!((-1.0..=1.0).contains(&d), "a dither of {d} exceeds one LSB");
        }
        let mean = s1 / n as f64;
        let var = s2 / n as f64 - mean * mean;
        let kurtosis = (s4 / n as f64) / (var * var);
        let autocorrelation = (cross / (n - 1) as f64 - mean * mean) / var;
        assert!(mean.abs() < 1e-3, "the dither has a DC bias of {mean}");
        assert!((var - 1.0 / 6.0).abs() < 1e-3, "variance {var}, ideal {}", 1.0 / 6.0);
        assert!((kurtosis - 2.4).abs() < 0.01, "kurtosis {kurtosis}, ideal 2.4 for a TPDF");
        assert!(
            autocorrelation.abs() < 5e-3,
            "successive dither values correlate at {autocorrelation}",
        );
    }

    /// A period quantized in one call is the same samples as one quantized a
    /// sample at a time, generator state included — the loop is the only thing
    /// [`quantize_period`] adds.
    #[test]
    fn a_period_quantizes_as_its_samples_do() {
        let mix: std::vec::Vec<f32> = (0..64).map(|i| (i as f32 - 32.0) / 64.0).collect();
        let mut whole = std::vec![0i16; mix.len()];
        let mut rng = Xorshift32::new(7);
        quantize_period(&mut whole, &mix, &mut rng);

        let mut rng = Xorshift32::new(7);
        let one_at_a_time: std::vec::Vec<i16> =
            mix.iter().map(|s| dither_and_quantize(*s, &mut rng)).collect();
        assert_eq!(whole, one_at_a_time);
    }
}
