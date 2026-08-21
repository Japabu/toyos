//! One stream: its ring, its ramp, and how it ended.
//!
//! A client is a control connection, a shared-memory ring soundd made for it, a
//! signal pipe soundd kept the write end of, and a gain. This module is where
//! all four are assembled ([`open_stream`]) and where one period of the result
//! reaches the bus ([`mix_client`]).
//!
//! **What the samples then become is `toyos-mixer`'s and not this file's.**
//! [`mix_client`] is the shared memory and the resampler; the decode, the
//! channel conversion, the interleave and the sum are calls into the crate,
//! whose corpus holds the answer each of them used to give inline.
//!
//! The resampler is `rubato`'s, and it is the one thing on this path that
//! nothing here certifies — what is certified is everything on both sides of it.

use rubato::{
    Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use toyos::audio::{
    AudioSlotReader, StreamOpenRequest, StreamOpenResponse, MSG_STREAM_OPENED,
};
use toyos::shm::SharedMemory;
use toyos::Connection;
use toyos_abi::audio::AudioSlotHeader;
use toyos_abi::syscall;
use toyos_abi::RawHandle;
use toyos_mixer::{
    accumulate, append_planar, client_period_frames, decode_i16_to_f32, interleave,
    mix_interleaved, Gain, GainRamp,
};


pub(crate) struct ClientResampler {
    resampler: SincFixedOut<f32>,
    /// Planar (per-channel) client audio awaiting resampling. SincFixedOut
    /// consumes a varying `input_frames_next()` per call, so slots are pulled
    /// into this buffer on demand instead of fed one fixed chunk per cycle.
    accum: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

/// How a stream ended, as far as soundd can honestly tell.
///
/// Two things witness a client leaving and they race: the control thread reads
/// the peer, and the mix loop finds the signal pipe gone on its next write.
/// Both start the same ramp, so no audio differs — but only the first of them
/// *knows* anything, and soundd used to report the second as a death. A clean
/// exit and a crash tear down the same descriptors the same way; the kernel's
/// `exit:` line carries the code and nothing on this side can tell them apart,
/// so `died` was a false positive at 11% of ordinary disconnects (5 of 44
/// runs). Each variant below is something soundd observed rather than inferred,
/// and the cause is left to the log that has it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Departure {
    /// `MSG_STREAM_CLOSE`: the client said so itself, which is the one reason
    /// nothing can improve on.
    Closed,
    /// soundd ended it — a protocol violation, or a volume that is not a
    /// number. The only departure soundd itself caused.
    Refused,
    /// The control connection ended without a close. The client's process is
    /// gone; whether it exited or crashed is not knowable here.
    Disconnected,
    /// The signal pipe broke, which says the client's descriptor table is gone
    /// and nothing about why. The weakest of the four, and the only one the
    /// others replace.
    SignalPipeGone,
}

impl Departure {
    /// The stronger of two witnesses, so the two may arrive in either order and
    /// land on the same word.
    ///
    /// What the control thread read beats what the mix loop found broken —
    /// it read the peer, where the mix loop only found a descriptor missing —
    /// and nothing beats the client's own close. Idempotent, and a witness
    /// never weakens what is already known.
    fn refine(self, other: Departure) -> Departure {
        if other.rank() < self.rank() { other } else { self }
    }

    /// How much this witness knows. Lower is stronger.
    fn rank(self) -> u8 {
        match self {
            Departure::Closed => 0,
            Departure::Refused => 1,
            Departure::Disconnected => 2,
            Departure::SignalPipeGone => 3,
        }
    }
}

impl core::fmt::Display for Departure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Departure::Closed => "closed",
            Departure::Refused => "refused",
            Departure::Disconnected => "disconnected",
            Departure::SignalPipeGone => "signal pipe gone",
        })
    }
}

pub(crate) struct ClientStream {
    pub(crate) client_id: usize,
    pub(crate) slot_reader: AudioSlotReader,
    /// The write end of the signal pipe. soundd makes both ends and sends the
    /// read end to the client, so §5.7's crash detection is by construction:
    /// the moment the client's table goes, the read end goes with it and the
    /// next signal breaks.
    pub(crate) signal_write: RawHandle,
    pub(crate) gain: GainRamp,
    pub(crate) client_channels: u16,
    pub(crate) client_period_frames: u32,
    pub(crate) resampler: Option<ClientResampler>,
    /// Latched by the first period this client supplies.
    pub(crate) delivered: bool,
    /// How this stream ended, once anything has witnessed it. `None` while it
    /// is live: the ramp-out starts when this is first set, and the stream is
    /// dropped when that ramp reaches idle.
    pub(crate) departure: Option<Departure>,
}

impl ClientStream {
    /// The window in which a period this client failed to cover is starvation
    /// rather than protocol: from the first period it delivered until it asks
    /// to close.
    ///
    /// Outside it, silence is the design working. `MSG_STREAM_OPEN` arrives
    /// before the client has any audio — it still has to spawn its callback
    /// thread — and after a close §5.5's ramp is deliberately fading it out,
    /// so it is entitled to stop filling.
    pub(crate) fn is_streaming(&self) -> bool {
        self.delivered && self.departure.is_none()
    }

    /// Record a departure, and start §7.4's ramp-out the first time one is
    /// known.
    ///
    /// A later witness refines the word and leaves the ramp alone: it is
    /// already aimed at silence, and re-targeting it recomputes the step from
    /// the gain reached so far, which stretches a 5 ms fade by however much of
    /// it had already run.
    pub(crate) fn depart(&mut self, how: Departure, ramp_frames: u32) {
        match self.departure {
            None => {
                self.gain.set_target(Gain::SILENT, ramp_frames);
                self.departure = Some(how);
            }
            Some(known) => self.departure = Some(known.refine(how)),
        }
    }
}

pub(crate) fn open_stream(
    client_id: usize,
    req: &StreamOpenRequest,
    control: &Connection,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: u32,
    slot_count: u32,
    ramp_frames: u32,
) -> Option<ClientStream> {
    let client_period_frames =
        client_period_frames(device_period_frames, req.sample_rate, device_sample_rate);

    let sample_size: u32 = 2; // FORMAT_S16LE, validated before open_stream
    let client_frame_size = req.channels as u32 * sample_size;
    let client_period_bytes = client_period_frames * client_frame_size;

    let shm_size = AudioSlotHeader::SIZE as u32 + slot_count * client_period_bytes;
    // The ring is this client's and no stream exists without it, so both
    // refusals end the open rather than the daemon: a client that exited
    // between asking and being served cannot be granted memory, and neither
    // can one that asked while the machine had none.
    let shm = match SharedMemory::create(shm_size as usize) {
        Ok(shm) => shm,
        Err(e) => {
            say!("soundd: no {shm_size}-byte ring for client {client_id} ({e:?})");
            return None;
        }
    };
    let client_shm = match shm.share() {
        Ok(h) => h,
        Err(e) => {
            say!("soundd: cannot share the ring with client {client_id} ({e:?})");
            return None;
        }
    };

    unsafe {
        let hdr = &*(shm.as_ptr() as *const AudioSlotHeader);
        hdr.write_idx.store(0, core::sync::atomic::Ordering::Relaxed);
        hdr.read_idx.store(0, core::sync::atomic::Ordering::Relaxed);
    }

    // **soundd makes the pipe and keeps the write end.** That is what makes a
    // dead client detectable without bookkeeping: the read end it is sent goes
    // when its table does, and the next signal answers `Gone`. The client used
    // to make the pipe and name it by an id, because an id was only openable
    // by a peer of its creator and the peer relation ran one way.
    let (signal_read, signal_write) = match toyos::pipe_pair() {
        Ok(ends) => ends,
        Err(e) => {
            syscall::close(client_shm);
            say!("soundd: no signal pipe for client {client_id} ({e:?})");
            return None;
        }
    };

    let slot_reader = AudioSlotReader::new(shm, client_period_bytes, slot_count);

    // Handles first, then the frame that announces them — `send_with_handles`
    // is that order, and a client reading the frame is guaranteed to find
    // them. Both are moved whether or not this succeeds.
    if control.send_with_handles(
        &[client_shm, signal_read.into_raw()],
        MSG_STREAM_OPENED,
        &StreamOpenResponse {
            client_period_frames,
            client_period_bytes,
            device_sample_rate,
            device_channels,
            slot_count: slot_count as u16,
        },
    ).is_err() {
        // Client died mid-open; the dropped control connection removes it.
        say!("soundd: client {client_id} vanished during stream open");
    }

    let resampler = if req.sample_rate != device_sample_rate {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let resample_ratio = device_sample_rate as f64 / req.sample_rate as f64;
        let resampler = SincFixedOut::<f32>::new(
            resample_ratio,
            2.0,
            params,
            device_period_frames as usize,
            device_channels as usize,
        ).expect("failed to create resampler");
        // The pull loop tops accum up to input_frames_next() one slot at a
        // time, so it peaks below input_frames_max + one client period.
        let accum_capacity = resampler.input_frames_max() + client_period_frames as usize;
        let accum = (0..device_channels as usize)
            .map(|_| Vec::with_capacity(accum_capacity))
            .collect();
        let output = resampler.output_buffer_allocate(true);
        Some(ClientResampler { resampler, accum, output })
    } else {
        None
    };

    let mut gain = GainRamp::new(Gain::SILENT);
    gain.set_target(Gain::UNITY, ramp_frames);

    Some(ClientStream {
        client_id,
        slot_reader,
        signal_write: signal_write.into_raw(),
        gain,
        client_channels: req.channels,
        client_period_frames,
        resampler,
        delivered: false,
        departure: None,
    })
}

/// Mix one period of `stream` into the bus. Returns false when the client's
/// ring could not supply a full period (silence mixed instead).
///
/// Slots are consumed peek→copy-out→advance: read_idx is published only after
/// the slot data has been decoded out of shared memory, so a concurrently
/// filling client can never overwrite a slot soundd is still reading.
pub(crate) fn mix_client(
    stream: &mut ClientStream,
    mix_f32: &mut [f32],
    decode_buf: &mut [f32],
    convert_buf: &mut [f32],
    device_channels: usize,
    device_period_frames: usize,
) -> bool {
    let client_frames = stream.client_period_frames as usize;
    let client_channels = stream.client_channels as usize;
    let client_samples = client_frames * client_channels;
    assert!(client_samples <= decode_buf.len());

    if let Some(rs) = stream.resampler.as_mut() {
        // Pull slots until the resampler's varying input requirement is met.
        // Consuming on demand (instead of one slot per cycle) keeps the
        // accumulation bounded: surplus frames from the ceil() slot sizing
        // simply delay the next slot consumption.
        loop {
            let needed = rs.resampler.input_frames_next();
            if rs.accum[0].len() >= needed {
                break;
            }
            let Some(slot) = stream.slot_reader.peek() else {
                stream.gain.advance_frames(device_period_frames as u32);
                return false;
            };
            decode_i16_to_f32(slot.data(), &mut decode_buf[..client_samples]);
            slot.advance();
            append_planar(&decode_buf[..client_samples], client_channels, &mut rs.accum);
        }

        let (consumed, produced) = rs.resampler
            .process_into_buffer(&rs.accum, &mut rs.output, None)
            .expect("resampler process failed");
        assert_eq!(produced, device_period_frames);
        for ch in rs.accum.iter_mut() {
            ch.drain(..consumed);
        }

        let out_samples = produced * device_channels;
        assert!(out_samples <= convert_buf.len());
        interleave(&rs.output, produced, &mut convert_buf[..out_samples]);
        accumulate(mix_f32, &convert_buf[..out_samples], device_channels, &mut stream.gain);
        return true;
    }

    let Some(slot) = stream.slot_reader.peek() else {
        stream.gain.advance_frames(device_period_frames as u32);
        return false;
    };
    decode_i16_to_f32(slot.data(), &mut decode_buf[..client_samples]);
    slot.advance();

    mix_interleaved(
        mix_f32,
        &decode_buf[..client_samples],
        convert_buf,
        client_channels,
        device_channels,
        &mut stream.gain,
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race the `died` line lost: two witnesses, either order, one word.
    ///
    /// The control thread read the peer; the mix loop only found a descriptor
    /// missing. Whichever arrives first, the removal must be reported with what
    /// was actually established — and a clean exit must never be reported as a
    /// death, which is what `SignalPipeGone` refuses to claim.
    #[test]
    fn the_stronger_witness_wins_in_either_order() {
        use Departure::*;
        for (a, b) in [(Closed, SignalPipeGone), (Refused, SignalPipeGone), (Disconnected, SignalPipeGone)] {
            assert_eq!(a.refine(b), a, "{b} must not replace {a}");
            assert_eq!(b.refine(a), a, "{a} must replace {b}");
        }
        // A client that asked to close is not downgraded by the connection it
        // then dropped.
        assert_eq!(Closed.refine(Disconnected), Closed);
        assert_eq!(Disconnected.refine(Closed), Closed);
        // Idempotent, so a repeated witness — the mix loop writes a broken pipe
        // every period until the ramp finishes — changes nothing.
        for how in [Closed, Refused, Disconnected, SignalPipeGone] {
            assert_eq!(how.refine(how), how);
        }
    }
}
