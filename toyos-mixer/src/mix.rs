//! The shared bus: where every client's audio becomes one period.
//!
//! Summation and nothing else. There is no limiter, no automatic gain and no
//! headroom reserved — clients sum, and a sum past full scale is clamped once,
//! at the quantizer, where `saturate` in the corpus records exactly what four
//! full-scale clients on one bus produce. That is a policy and not an accident;
//! moving it is the owner's call, and the corpus is what makes moving it visible.

use crate::channel::{channel_convert_mono_to_stereo, channel_convert_stereo_to_mono};
use crate::gain::GainRamp;

/// Gain-scale `src` and add it onto the mix bus. The ramp steps per frame so
/// both channels of a frame get the same gain and a 5ms ramp lasts 5ms.
pub fn accumulate(mix: &mut [f32], src: &[f32], channels: usize, gain: &mut GainRamp) {
    assert_eq!(src.len(), mix.len());
    if gain.is_idle() {
        let g = gain.level();
        if g == 1.0 {
            for (m, s) in mix.iter_mut().zip(src) {
                *m += s;
            }
        } else if g > 0.0 {
            for (m, s) in mix.iter_mut().zip(src) {
                *m += s * g;
            }
        }
    } else {
        for frame in 0..src.len() / channels {
            let g = gain.next();
            for ch in 0..channels {
                mix[frame * channels + ch] += src[frame * channels + ch] * g;
            }
        }
    }
}

/// One decoded client period onto the bus: convert the channel count if it
/// differs, then accumulate under the ramp.
///
/// This is `mix_client`'s whole non-resampled tail, and the resampled path
/// reaches it too by way of `interleave`. `convert_buf` is scratch the caller
/// allocated once — the mix loop runs on the RT band and does not allocate.
pub fn mix_interleaved(
    mix: &mut [f32],
    decoded: &[f32],
    convert_buf: &mut [f32],
    client_channels: usize,
    device_channels: usize,
    gain: &mut GainRamp,
) {
    let client_frames = decoded.len() / client_channels;
    let src: &[f32] = if client_channels != device_channels {
        let out_samples = client_frames * device_channels;
        assert!(out_samples <= convert_buf.len());
        match (client_channels, device_channels) {
            (1, 2) => channel_convert_mono_to_stereo(decoded, &mut convert_buf[..out_samples]),
            (2, 1) => channel_convert_stereo_to_mono(decoded, &mut convert_buf[..out_samples]),
            (c, d) => panic!("soundd: unsupported channel conversion {c}→{d}"),
        }
        &convert_buf[..out_samples]
    } else {
        decoded
    };
    accumulate(mix, src, device_channels, gain);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gain::Gain;
    use alloc::vec;

    /// **A silent client adds nothing at all**, bit for bit. The zero-gain arm
    /// is not an optimisation: `x * 0.0` is `-0.0` for a negative `x`, and a bus
    /// of `-0.0` is a bus a later `+=` still sums correctly but a comparison
    /// against `0.0` reads differently.
    #[test]
    fn a_silent_client_leaves_the_bus_untouched() {
        let src = [1.0f32, -1.0, 0.5, -0.5];
        let mut mix = [7.0f32, -7.0, 0.0, -0.0];
        let before = mix;
        let mut gain = GainRamp::new(Gain::SILENT);
        accumulate(&mut mix, &src, 2, &mut gain);
        for (a, b) in mix.iter().zip(before.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    /// A client at unity is added rather than multiplied. `x * 1.0` is `x` for
    /// every float there is, so this is about the arithmetic being skipped and
    /// not about the answer differing — but the corpus holds the answer, so the
    /// skip has to keep it.
    #[test]
    fn a_client_at_unity_is_added_unscaled() {
        let src = [1.0f32, -1.0, 0.25, f32::MIN_POSITIVE];
        let mut mix = [0.0f32; 4];
        let mut gain = GainRamp::new(Gain::UNITY);
        accumulate(&mut mix, &src, 2, &mut gain);
        for (m, s) in mix.iter().zip(src.iter()) {
            assert_eq!(m.to_bits(), s.to_bits());
        }
    }

    /// **A frame's channels share one gain step.** They must: a ramp that
    /// stepped per *sample* would advance twice as fast on a stereo device, so
    /// a 5 ms fade would last 2.5 ms — and the two channels of one frame would
    /// be at different levels, which is a moving image rather than a fade.
    #[test]
    fn both_channels_of_a_frame_get_one_gain() {
        let frames = 32;
        let src: alloc::vec::Vec<f32> = (0..frames * 2).map(|_| 1.0f32).collect();
        let mut mix = vec![0.0f32; frames * 2];
        let mut gain = GainRamp::new(Gain::SILENT);
        gain.set_target(Gain::UNITY, frames as u32 * 4);
        accumulate(&mut mix, &src, 2, &mut gain);
        for frame in 0..frames {
            assert_eq!(
                mix[frame * 2].to_bits(),
                mix[frame * 2 + 1].to_bits(),
                "frame {frame} came out with two different gains"
            );
        }
        // And exactly `frames` steps were taken, not `frames * 2`.
        let mut alone = GainRamp::new(Gain::SILENT);
        alone.set_target(Gain::UNITY, frames as u32 * 4);
        for _ in 0..frames {
            alone.next();
        }
        assert_eq!(gain.level().to_bits(), alone.level().to_bits());
    }

    /// Clients sum. Four at full scale reach four times full scale on the bus,
    /// because the clamp is the quantizer's job and happens once.
    #[test]
    fn clients_sum_and_the_bus_is_not_clamped() {
        let src = [1.0f32, 1.0];
        let mut mix = [0.0f32; 2];
        for _ in 0..4 {
            let mut gain = GainRamp::new(Gain::UNITY);
            accumulate(&mut mix, &src, 2, &mut gain);
        }
        assert_eq!(mix, [4.0, 4.0]);
    }

    /// The composite is the parts: `mix_interleaved` may not differ from the
    /// convert-then-accumulate the mix loop used to write inline, on any of the
    /// four channel combinations.
    #[test]
    fn the_composite_is_the_conversion_and_the_sum() {
        for (client_channels, device_channels) in [(1usize, 1usize), (2, 2), (1, 2), (2, 1)] {
            let frames = 9usize;
            let decoded: alloc::vec::Vec<f32> = (0..frames * client_channels)
                .map(|i| (i as f32 * 0.13 - 0.5).clamp(-1.0, 1.0))
                .collect();

            let mut composite = vec![0.0f32; frames * device_channels];
            let mut scratch = vec![0.0f32; frames * 2];
            let mut gain = GainRamp::new(Gain::SILENT);
            gain.set_target(Gain::UNITY, 7);
            mix_interleaved(
                &mut composite,
                &decoded,
                &mut scratch,
                client_channels,
                device_channels,
                &mut gain,
            );

            let mut parts = vec![0.0f32; frames * device_channels];
            let mut scratch = vec![0.0f32; frames * 2];
            let mut gain = GainRamp::new(Gain::SILENT);
            gain.set_target(Gain::UNITY, 7);
            let src: &[f32] = if client_channels != device_channels {
                let out_samples = frames * device_channels;
                match (client_channels, device_channels) {
                    (1, 2) => channel_convert_mono_to_stereo(
                        &decoded,
                        &mut scratch[..out_samples],
                    ),
                    (2, 1) => channel_convert_stereo_to_mono(
                        &decoded,
                        &mut scratch[..out_samples],
                    ),
                    _ => unreachable!(),
                }
                &scratch[..out_samples]
            } else {
                &decoded
            };
            accumulate(&mut parts, src, device_channels, &mut gain);

            for (a, b) in composite.iter().zip(parts.iter()) {
                assert_eq!(a.to_bits(), b.to_bits(), "{client_channels}→{device_channels}");
            }
        }
    }

    /// A period that is not the bus's width is a bug in the caller, not a
    /// partial mix: half a period of one client under another's whole one is
    /// audible as a discontinuity, so it dies here.
    #[test]
    #[should_panic]
    fn a_period_of_the_wrong_width_is_refused() {
        let mut mix = [0.0f32; 4];
        let mut gain = GainRamp::new(Gain::UNITY);
        accumulate(&mut mix, &[1.0, 1.0], 2, &mut gain);
    }
}
