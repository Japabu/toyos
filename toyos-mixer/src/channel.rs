//! Mono and stereo, which is the whole of what this mixer converts between.
//!
//! Three shapes meet here: the interleaved period a client wrote, the planar
//! buffers a resampler consumes and produces, and the interleaved bus. A
//! conversion that is not one of the four combinations of one and two channels
//! is a panic by name rather than a silently wrong downmix — `period_frames`
//! refuses a device with any other count before a stream exists, and
//! `reject_open` refuses a client with one.

use alloc::vec::Vec;

/// One channel to two: the same sample in both, which is what a mono source
/// played on a stereo device is.
pub fn channel_convert_mono_to_stereo(src: &[f32], dst: &mut [f32]) {
    for i in 0..src.len() {
        dst[i * 2] = src[i];
        dst[i * 2 + 1] = src[i];
    }
}

/// Two channels to one: the average, so two full-scale channels downmix to full
/// scale rather than to twice it.
pub fn channel_convert_stereo_to_mono(src: &[f32], dst: &mut [f32]) {
    for i in 0..dst.len() {
        dst[i] = (src[i * 2] + src[i * 2 + 1]) * 0.5;
    }
}

/// Deinterleave one decoded client period into the resampler's planar
/// accumulation buffers, channel-converting on the way.
pub fn append_planar(decoded: &[f32], client_channels: usize, accum: &mut [Vec<f32>]) {
    let device_channels = accum.len();
    let frames = decoded.len() / client_channels;
    for ch in accum.iter() {
        assert!(ch.len() + frames <= ch.capacity(), "resampler accum overflow");
    }
    match (client_channels, device_channels) {
        (c, d) if c == d => {
            for frame in 0..frames {
                for ch in 0..c {
                    accum[ch].push(decoded[frame * c + ch]);
                }
            }
        }
        (1, 2) => {
            for &s in decoded {
                accum[0].push(s);
                accum[1].push(s);
            }
        }
        (2, 1) => {
            for frame in 0..frames {
                accum[0].push((decoded[frame * 2] + decoded[frame * 2 + 1]) * 0.5);
            }
        }
        (c, d) => panic!("soundd: unsupported channel conversion {c}→{d}"),
    }
}

/// The planar frames a resampler produced, laid back out interleaved for the
/// bus.
///
/// `frames` rather than `planar[0].len()`: a resampler reports what it produced
/// and its output buffers are allocated for the worst case, so the tail of each
/// plane is last period's audio.
pub fn interleave(planar: &[Vec<f32>], frames: usize, out: &mut [f32]) {
    let channels = planar.len();
    for frame in 0..frames {
        for ch in 0..channels {
            out[frame * channels + ch] = planar[ch][frame];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A mono client on a stereo device is centred, which means identical
    /// samples and not a half-power pan: soundd has no panning, and a channel
    /// that differed from its twin by a rounding step would be an image nobody
    /// asked for.
    #[test]
    fn a_mono_client_is_the_same_sample_in_both_channels() {
        let src = [1.0f32, -1.0, 0.0, 0.5];
        let mut dst = [0.0f32; 8];
        channel_convert_mono_to_stereo(&src, &mut dst);
        for (i, s) in src.iter().enumerate() {
            assert_eq!(dst[i * 2].to_bits(), s.to_bits());
            assert_eq!(dst[i * 2 + 1].to_bits(), s.to_bits());
        }
    }

    /// The downmix is the average, so two full-scale channels reach full scale
    /// and not twice it — and two channels exactly out of phase reach silence
    /// rather than a residue.
    #[test]
    fn the_downmix_averages_rather_than_sums() {
        let src = [1.0f32, 1.0, -1.0, -1.0, 1.0, -1.0, 0.5, -0.25];
        let mut dst = [0.0f32; 4];
        channel_convert_stereo_to_mono(&src, &mut dst);
        assert_eq!(dst[0].to_bits(), 1.0f32.to_bits());
        assert_eq!(dst[1].to_bits(), (-1.0f32).to_bits());
        assert_eq!(dst[2].to_bits(), 0.0f32.to_bits());
        assert_eq!(dst[3].to_bits(), 0.125f32.to_bits());
    }

    /// The planar append is the interleaved conversion, one channel per plane —
    /// two paths to one answer, and a resampling client and a passthrough one
    /// must not hear different downmixes.
    #[test]
    fn planar_and_interleaved_convert_alike() {
        let decoded = [1.0f32, -1.0, 0.25, -0.75, 0.5, 0.5];

        let mut accum: Vec<Vec<f32>> = vec![Vec::with_capacity(8)];
        append_planar(&decoded, 2, &mut accum);
        let mut flat = [0.0f32; 3];
        channel_convert_stereo_to_mono(&decoded, &mut flat);
        assert_eq!(accum[0], flat);

        let mono = [0.25f32, -0.5, 1.0];
        let mut accum: Vec<Vec<f32>> =
            vec![Vec::with_capacity(8), Vec::with_capacity(8)];
        append_planar(&mono, 1, &mut accum);
        let mut wide = [0.0f32; 6];
        channel_convert_mono_to_stereo(&mono, &mut wide);
        let mut back = [0.0f32; 6];
        interleave(&accum, 3, &mut back);
        assert_eq!(back, wide);
    }

    /// The buffer is the caller's and the resampler's requirement varies per
    /// call, so an append that outgrew it would reallocate — inside the mix
    /// loop, on the RT band. It is a refusal instead.
    #[test]
    #[should_panic(expected = "resampler accum overflow")]
    fn an_append_past_the_reserved_capacity_is_refused() {
        let mut accum: Vec<Vec<f32>> = vec![Vec::with_capacity(2)];
        append_planar(&[0.0, 0.0, 0.0], 1, &mut accum);
    }

    /// Appending twice lands after what is already there: the accumulation is
    /// what lets a resampler consume a varying number of frames per call.
    #[test]
    fn an_append_lands_after_what_is_already_there() {
        let mut accum: Vec<Vec<f32>> = vec![Vec::with_capacity(8)];
        append_planar(&[1.0, 2.0], 1, &mut accum);
        append_planar(&[3.0, 4.0], 1, &mut accum);
        assert_eq!(accum[0], [1.0, 2.0, 3.0, 4.0]);
    }

    /// Only what the resampler says it produced is read, so the stale tail of an
    /// over-allocated plane never reaches the bus.
    #[test]
    fn the_interleave_reads_only_the_frames_produced() {
        let planar = vec![vec![1.0f32, 2.0, 99.0], vec![-1.0f32, -2.0, -99.0]];
        let mut out = [0.0f32; 4];
        interleave(&planar, 2, &mut out);
        assert_eq!(out, [1.0, -1.0, 2.0, -2.0]);
    }
}
