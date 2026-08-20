//! The shapes a period can have, and the sizes every buffer is derived from.
//!
//! A device arrives with a pipeline depth, a channel count and a period size,
//! and a client arrives with a sample rate. Everything the mix loop allocates —
//! the bus, the decode scratch, the convert scratch, the client's ring — is
//! arithmetic on those four numbers, and an arithmetic that is off by one is an
//! assertion in the mix loop at a client's choosing rather than a refusal at
//! open. That is why the sizes are here and checked on the host.

/// Of a pipeline's periods, how many soundd keeps in reserve rather than
/// spending on a client that is still filling (§4).
///
/// Policy, not physics, with the same standing as the kernel's `MAX_USER_STR`:
/// of the shipped pipeline's 8 periods, soundd waits on a client for at most 3
/// and always keeps 5 unplayed. It cannot be derived from worst-case wake
/// lateness — the recorded worst exceeds two whole pipelines, so no floor
/// inside the pipeline covers it. Move it only with a full re-baseline.
pub const DEFERRAL_RESERVE: usize = 5;

/// The deepest pipeline the mix loop can hold.
///
/// Its free list is a `u32` bitmask, so `1u32 << num_buffers` has to fit; with
/// the power-of-two rule below, 16 is the deepest that does.
pub const MAX_PIPELINE: usize = 16;

/// The rates a client may ask for. Outside them the open is refused, which is
/// what keeps [`client_period_frames`] inside [`scratch_frames`].
pub const MIN_CLIENT_RATE: u32 = 8_000;
pub const MAX_CLIENT_RATE: u32 = 192_000;

/// How much unplayed audio must still be on the wire before the mix loop may
/// defer a buffer for a client that is mid-refill (§4), or `None` on a pipeline
/// with nothing to spend.
///
/// **The `None` used to be a startup panic** — `assert!(num_buffers > 5)`, a
/// device shape killing the daemon that serves every client on the machine, in
/// the class of the NVMe and xHCI zero-device panics. §4 already says what to
/// do instead and always did: *on a pipeline of five or fewer buffers the
/// deferral policy is disabled and every free buffer is mixed immediately*. A
/// reserve that is the whole pipeline is not a reserve, and mixing at once is
/// what soundd does when it cannot afford to wait.
pub fn deferral_floor_nanos(num_buffers: usize, period_nanos: u64) -> Option<u64> {
    (num_buffers > DEFERRAL_RESERVE).then(|| DEFERRAL_RESERVE as u64 * period_nanos)
}

/// A device shape soundd cannot render a period into.
///
/// Every arm is a constraint the mix loop's own arithmetic imposes, named where
/// it is imposed. A shape that trips one is refused by name and the machine
/// gets the null sink (§6): requirement 6 is that soundd always runs and always
/// accepts streams, and it does not except itself from that when the surprise
/// is a device rather than an absence. Silence a client can play into beats a
/// dead daemon whose every connect is refused for the machine's lifetime.
pub enum Shape {
    /// A pipeline of one has no depth: `min_drain_nanos` is zero, so §5.9's
    /// drain count could not tell a stall from ordinary operation.
    Shallow(usize),
    /// Deeper than [`MAX_PIPELINE`].
    Deep(usize),
    /// `slot_count` is the pipeline depth and the client ring's indices are
    /// free-running mod 2^32 (§3), so the depth has to divide that evenly.
    Uneven(usize),
    /// The mixer converts mono and stereo, and nothing else.
    Channels(u16),
    /// A period that is not a whole number of frames — including a period of
    /// no bytes at all, which would divide by zero on the way to the frame
    /// count.
    PartialFrame { period_bytes: usize, frame_bytes: usize },
}

impl core::fmt::Display for Shape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Shape::Shallow(n) => write!(f, "a {n}-period pipeline has no depth to drain"),
            Shape::Deep(n) => {
                write!(f, "{n} periods is deeper than the {MAX_PIPELINE} a free list holds")
            }
            Shape::Uneven(n) => write!(
                f,
                "{n} periods is not a power of two, and a client ring's indices wrap mod 2^32"
            ),
            Shape::Channels(c) => write!(f, "{c} channels is neither mono nor stereo"),
            Shape::PartialFrame { period_bytes, frame_bytes } => {
                write!(f, "a {period_bytes}-byte period is not a whole number of {frame_bytes}-byte frames")
            }
        }
    }
}

/// The frames in one device period, or why soundd cannot serve this device.
///
/// The arithmetic that could fault lives inside the check, so no caller can
/// perform it before asking.
pub fn period_frames(
    num_buffers: usize,
    device_channels: u16,
    device_period_bytes: usize,
) -> Result<usize, Shape> {
    if num_buffers < 2 {
        return Err(Shape::Shallow(num_buffers));
    }
    if num_buffers > MAX_PIPELINE {
        return Err(Shape::Deep(num_buffers));
    }
    if !num_buffers.is_power_of_two() {
        return Err(Shape::Uneven(num_buffers));
    }
    if device_channels != 1 && device_channels != 2 {
        return Err(Shape::Channels(device_channels));
    }
    let frame_bytes = device_channels as usize * 2;
    if device_period_bytes == 0 || !device_period_bytes.is_multiple_of(frame_bytes) {
        return Err(Shape::PartialFrame { period_bytes: device_period_bytes, frame_bytes });
    }
    Ok(device_period_bytes / frame_bytes)
}

/// How many of this client's frames cover one device period.
///
/// Rounded **up**, so a resampling client always has at least one device period
/// of input in each slot; the surplus the ceiling leaves is what
/// `mix_client`'s pull loop carries to the next cycle rather than discarding.
pub fn client_period_frames(
    device_period_frames: u32,
    client_rate: u32,
    device_rate: u32,
) -> u32 {
    if client_rate != device_rate {
        (device_period_frames as u64 * client_rate as u64).div_ceil(device_rate as u64) as u32
    } else {
        device_period_frames
    }
}

/// The widest client period the mix thread's scratch buffers have to hold.
///
/// Sized from [`MAX_CLIENT_RATE`] once, at startup, so the mix path never
/// allocates — and `the_scratch_covers_every_rate_a_client_may_ask_for` is what
/// says the once is enough.
pub fn scratch_frames(device_period_frames: usize, device_rate: usize) -> usize {
    (device_period_frames * MAX_CLIENT_RATE as usize).div_ceil(device_rate)
}

/// One device period in nanoseconds. The grid everything the mix loop predicts
/// is measured against.
pub fn period_nanos(device_period_frames: u64, device_rate: u64) -> u64 {
    (device_period_frames * 1_000_000_000) / device_rate
}

/// The ~5 ms connect/disconnect/volume ramp, in frames of this device.
pub fn ramp_frames(device_rate: u32) -> u32 {
    device_rate * 5 / 1000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One period of the shipped 44100 Hz stereo device, in nanoseconds.
    const PERIOD_NS: u64 = 2_902_494;

    /// §4's shallow-pipeline clause: *on a pipeline of five or fewer buffers
    /// the deferral policy is disabled and every free buffer is mixed
    /// immediately*. It was an `assert!(num_buffers > 5)` — a startup panic —
    /// for as long as the spec said this.
    #[test]
    fn a_pipeline_with_nothing_to_reserve_disables_deferral() {
        for shallow in 1..=DEFERRAL_RESERVE {
            assert_eq!(
                deferral_floor_nanos(shallow, PERIOD_NS),
                None,
                "a {shallow}-period pipeline has no {DEFERRAL_RESERVE} periods to keep back, \
                 so there is no floor to defer above"
            );
        }
    }

    /// The other side of the same rule, and the number gate A's baseline was
    /// recorded against: eight periods, five held.
    #[test]
    fn the_shipped_pipeline_keeps_five_periods_in_reserve() {
        assert_eq!(deferral_floor_nanos(8, PERIOD_NS), Some(5 * PERIOD_NS));
        assert_eq!(deferral_floor_nanos(6, PERIOD_NS), Some(5 * PERIOD_NS));
    }

    /// Both shipped devices — virtio-sound and HDA — present the same shape,
    /// and it is served rather than refused. A refusal here would put every
    /// machine on the null sink.
    #[test]
    fn the_shipped_device_shape_is_served() {
        assert!(matches!(period_frames(8, 2, 512), Ok(128)));
    }

    /// Every constraint the mix loop imposes is refused by name rather than by
    /// a panic, and the arithmetic that would fault never runs: a zero channel
    /// count used to divide by zero one line before the assert that was
    /// supposed to catch it.
    #[test]
    fn a_shape_the_mixer_cannot_render_is_refused_and_nothing_divides_by_zero() {
        assert!(matches!(period_frames(1, 2, 512), Err(Shape::Shallow(1))));
        assert!(matches!(period_frames(32, 2, 512), Err(Shape::Deep(32))));
        assert!(matches!(period_frames(6, 2, 512), Err(Shape::Uneven(6))));
        assert!(matches!(period_frames(8, 0, 512), Err(Shape::Channels(0))));
        assert!(matches!(period_frames(8, 6, 512), Err(Shape::Channels(6))));
        assert!(matches!(period_frames(8, 2, 0), Err(Shape::PartialFrame { .. })));
        assert!(matches!(period_frames(8, 2, 513), Err(Shape::PartialFrame { .. })));
        // A shape the free list *can* hold, which is what makes the ceiling a
        // ceiling rather than a refusal of everything unusual.
        assert!(matches!(period_frames(16, 1, 512), Ok(256)));
    }

    /// **The inequality the mix loop's scratch rests on.** `mix_client` asserts
    /// a client's decoded period fits `decode_buf`, and `decode_buf` is
    /// [`scratch_frames`] wide by two channels, allocated once at startup from
    /// [`MAX_CLIENT_RATE`]. Every rate a client may ask for is checked against
    /// it here, because the alternative is that assertion firing inside the mix
    /// loop on a number a client chose.
    #[test]
    fn the_scratch_covers_every_rate_a_client_may_ask_for() {
        for device_rate in [44_100u32, 48_000, 96_000, 176_400, 192_000] {
            for device_period_frames in [1u32, 3, 64, 127, 128, 129, 256, 1024] {
                let scratch =
                    scratch_frames(device_period_frames as usize, device_rate as usize);
                for client_rate in [
                    MIN_CLIENT_RATE,
                    11_025,
                    22_050,
                    44_100,
                    48_000,
                    96_000,
                    device_rate - 1,
                    device_rate,
                    device_rate + 1,
                    MAX_CLIENT_RATE - 1,
                    MAX_CLIENT_RATE,
                ] {
                    if !(MIN_CLIENT_RATE..=MAX_CLIENT_RATE).contains(&client_rate) {
                        continue;
                    }
                    let frames =
                        client_period_frames(device_period_frames, client_rate, device_rate);
                    assert!(
                        frames as usize <= scratch,
                        "a {client_rate} Hz client needs {frames} frames per \
                         {device_period_frames}-frame period at {device_rate} Hz, and the \
                         scratch holds {scratch}",
                    );
                }
                // And the device's own period fits, which is what the resampled
                // path writes into the convert buffer.
                assert!(device_period_frames as usize <= scratch);
            }
        }
    }

    /// The exhaustive form of the same claim over the rates the shipped devices
    /// run at: every one of the 184,001 a client may name.
    #[test]
    fn no_accepted_rate_overruns_the_shipped_scratch() {
        for device_rate in [44_100u32, 48_000] {
            for device_period_frames in [128u32, 256] {
                let scratch =
                    scratch_frames(device_period_frames as usize, device_rate as usize);
                for client_rate in MIN_CLIENT_RATE..=MAX_CLIENT_RATE {
                    let frames =
                        client_period_frames(device_period_frames, client_rate, device_rate);
                    assert!(frames as usize <= scratch, "{client_rate} Hz needs {frames}");
                    assert!(frames > 0, "{client_rate} Hz sized a period of nothing");
                }
            }
        }
    }

    /// The rate the device itself runs at is a passthrough, exactly: no
    /// resampler is built for it, so a period that came out one frame short
    /// would be a client whose ring never lines up with the device's.
    #[test]
    fn a_client_at_the_device_s_own_rate_gets_the_device_s_period() {
        for rate in [8_000u32, 44_100, 48_000, 192_000] {
            for frames in [1u32, 128, 1024] {
                assert_eq!(client_period_frames(frames, rate, rate), frames);
            }
        }
    }

    /// The two numbers derived straight from the device's rate, at the shapes
    /// that ship. Both are floors, and the corpus holds the whole table.
    #[test]
    fn the_shipped_grid_and_ramp() {
        assert_eq!(period_nanos(128, 44_100), 2_902_494);
        assert_eq!(period_nanos(128, 48_000), 2_666_666);
        assert_eq!(ramp_frames(44_100), 220);
        assert_eq!(ramp_frames(48_000), 240);
    }
}
