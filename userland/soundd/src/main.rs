use toyos_abi::audio::{AudioCompletionRecord, AudioInfo, AudioSlotHeader};
use toyos_abi::Fd;
use toyos::audio::{
    AudioSlotReader, StreamOpenRequest, StreamOpenResponse, StreamSetVolume, FORMAT_S16LE,
    MSG_STREAM_OPEN, MSG_STREAM_OPENED, MSG_STREAM_SET_VOLUME, MSG_STREAM_CLOSE, MSG_STREAM_ERROR,
};
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::services;
use toyos::shm::SharedMemory;
use toyos::{AudioDev, Connection};
use toyos_abi::syscall;

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rubato::{Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction};

const MIN_CLIENT_RATE: u32 = 8_000;
const MAX_CLIENT_RATE: u32 = 192_000;
const STATS_INTERVAL_NANOS: u64 = 2_000_000_000;

// ---------------------------------------------------------------------------
// Per-client state
// ---------------------------------------------------------------------------

struct ClientResampler {
    resampler: SincFixedOut<f32>,
    /// Planar (per-channel) client audio awaiting resampling. SincFixedOut
    /// consumes a varying `input_frames_next()` per call, so slots are pulled
    /// into this buffer on demand instead of fed one fixed chunk per cycle.
    accum: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

struct GainRamp {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl GainRamp {
    fn new(initial: f32) -> Self {
        Self { current: initial, target: initial, step: 0.0, remaining: 0 }
    }

    fn set_target(&mut self, target: f32, ramp_frames: u32) {
        self.target = target;
        self.step = (target - self.current) / ramp_frames as f32;
        self.remaining = ramp_frames;
    }

    /// Gain for the next frame (both channels of a frame get the same gain).
    fn next(&mut self) -> f32 {
        if self.remaining > 0 {
            self.current += self.step;
            self.remaining -= 1;
            if self.remaining == 0 { self.current = self.target; }
        }
        self.current
    }

    /// Advance the ramp across a period of silence (§7.3: the ramp applies to
    /// silence when the ring is empty — otherwise a drained closing client
    /// would never finish its ramp and never be removed).
    fn advance_frames(&mut self, frames: u32) {
        let n = frames.min(self.remaining);
        self.current += self.step * n as f32;
        self.remaining -= n;
        if self.remaining == 0 { self.current = self.target; }
    }

    fn is_idle(&self) -> bool { self.remaining == 0 }

    fn level(&self) -> f32 { self.current }
}

struct ClientStream {
    client_id: usize,
    slot_reader: AudioSlotReader,
    signal_write_fd: Fd,
    signal_read_fd: Fd,
    gain: GainRamp,
    client_channels: u16,
    client_period_frames: u32,
    resampler: Option<ClientResampler>,
    pending_removal: bool,
}

// ---------------------------------------------------------------------------
// Lock-free SPSC command queue (control thread → mix thread)
// ---------------------------------------------------------------------------

const CMD_RING_SIZE: u32 = 64;

enum MixCommand {
    AddClient(Box<ClientStream>),
    RemoveClient(usize),
    SetVolume { client_id: usize, target: f32 },
}

struct CommandRing {
    slots: std::cell::UnsafeCell<[Option<MixCommand>; CMD_RING_SIZE as usize]>,
    write_idx: AtomicU32,
    read_idx: AtomicU32,
}

unsafe impl Send for CommandRing {}
unsafe impl Sync for CommandRing {}

impl CommandRing {
    fn new() -> Self {
        Self {
            slots: std::cell::UnsafeCell::new(std::array::from_fn(|_| None)),
            write_idx: AtomicU32::new(0),
            read_idx: AtomicU32::new(0),
        }
    }

    /// Panics when full: a dropped command means a ghost client (leaked shm,
    /// app waiting forever) — a bug, not a load condition.
    fn push(&self, cmd: MixCommand) {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        assert!(w.wrapping_sub(r) < CMD_RING_SIZE, "soundd: command ring full");
        let idx = (w % CMD_RING_SIZE) as usize;
        unsafe { (*self.slots.get())[idx] = Some(cmd); }
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
    }

    fn pop(&self) -> Option<MixCommand> {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w == r { return None; }
        let idx = (r % CMD_RING_SIZE) as usize;
        let cmd = unsafe { (*self.slots.get())[idx].take() };
        self.read_idx.store(r.wrapping_add(1), Ordering::Release);
        cmd
    }
}

// ---------------------------------------------------------------------------
// Mixing helpers
// ---------------------------------------------------------------------------

fn decode_i16_to_f32(src: &[u8], dst: &mut [f32]) {
    for i in 0..dst.len() {
        let sample = i16::from_le_bytes([src[i * 2], src[i * 2 + 1]]);
        dst[i] = sample as f32 / 32768.0;
    }
}

fn channel_convert_mono_to_stereo(src: &[f32], dst: &mut [f32]) {
    for i in 0..src.len() {
        dst[i * 2] = src[i];
        dst[i * 2 + 1] = src[i];
    }
}

fn channel_convert_stereo_to_mono(src: &[f32], dst: &mut [f32]) {
    for i in 0..dst.len() {
        dst[i] = (src[i * 2] + src[i * 2 + 1]) * 0.5;
    }
}

/// Deinterleave one decoded client period into the resampler's planar
/// accumulation buffers, channel-converting on the way.
fn append_planar(decoded: &[f32], client_channels: usize, accum: &mut [Vec<f32>]) {
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

/// Gain-scale `src` and add it onto the mix bus. The ramp steps per frame so
/// both channels of a frame get the same gain and a 5ms ramp lasts 5ms.
fn accumulate(mix: &mut [f32], src: &[f32], channels: usize, gain: &mut GainRamp) {
    assert_eq!(src.len(), mix.len());
    if gain.is_idle() {
        let g = gain.level();
        if g == 1.0 {
            for (m, s) in mix.iter_mut().zip(src) { *m += s; }
        } else if g > 0.0 {
            for (m, s) in mix.iter_mut().zip(src) { *m += s * g; }
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

// ---------------------------------------------------------------------------
// TPDF dither
// ---------------------------------------------------------------------------

struct Xorshift32(u32);

impl Xorshift32 {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f32) / (u32::MAX as f32) - 0.5
    }
}

/// §5.4. TPDF dither is defined against a **round-to-nearest** quantizer —
/// that pairing is what makes the error zero-mean and its variance
/// signal-independent. Truncating instead (`as i16` rounds toward zero) biases
/// every sample 0.5 LSB toward zero and swallows the dither whole inside
/// ±1 LSB, giving a 2-LSB dead zone at the zero crossing and a noise floor
/// that collapses with the signal: exactly the crossover distortion and
/// noise-floor modulation dither exists to remove.
fn dither_and_quantize(sample: f32, rng: &mut Xorshift32) -> i16 {
    let dither = rng.next() + rng.next(); // triangular PDF in [-1.0, 1.0]
    (sample * 32767.0 + dither).round().clamp(-32768.0, 32767.0) as i16
}

// ---------------------------------------------------------------------------
// Mix-thread accounting
// ---------------------------------------------------------------------------

/// Counters for one reporting window. A window covers streaming only: it is
/// zeroed when the first client arrives and flushed when the last one leaves,
/// so no number here is diluted by the idle path — where soundd waits on raw
/// completion IRQs with no timer, and a batched IRQ looks exactly like a
/// missed deadline. The audio gate reads these
/// (`tests/audio-baseline.toml`), so they have to mean one thing only.
#[derive(Default)]
struct MixStats {
    wakes: u32,
    completions: u32,
    submitted: u32,
    /// Periods submitted with no client audio behind them: silence that
    /// actually went on the wire while a client was streaming.
    underruns: u32,
    /// Cycles that found the whole DMA pipeline free (§5.9).
    drains: u32,
    max_wake_lat_ns: u64,
    max_batch: u32,
}

impl MixStats {
    fn report(&self, clients: usize) {
        eprintln!("soundd: wakes={} completions={} submitted={} underruns={} drains={} max_wake_lat_us={} max_batch={} clients={}",
            self.wakes, self.completions, self.submitted, self.underruns, self.drains,
            self.max_wake_lat_ns / 1_000, self.max_batch, clients);
    }
}

// ---------------------------------------------------------------------------
// DLL timer
// ---------------------------------------------------------------------------

struct Dll {
    t_estimated: Option<f64>,
    period: f64,
    nominal_period: f64,
    bw: f64,
}

impl Dll {
    fn new(nominal_period_nanos: f64) -> Self {
        Self { t_estimated: None, period: nominal_period_nanos, nominal_period: nominal_period_nanos, bw: 0.03 }
    }

    /// Forget the estimate after a pipeline re-prime (§5.9); the next
    /// completion record re-initializes it.
    fn reset(&mut self) {
        self.t_estimated = None;
        self.period = self.nominal_period;
    }

    /// Feed one completion record: `n_periods` buffers finished with a single
    /// interrupt at `t_actual`. The batch timestamp belongs to the *last* of
    /// the n grid points, so the prediction error is measured against
    /// `t_estimated + (n-1)·period`.
    fn update(&mut self, t_actual: f64, n_periods: u32) {
        match self.t_estimated {
            None => {
                self.t_estimated = Some(t_actual + self.period);
            }
            Some(t_est) => {
                let predicted = t_est + (n_periods - 1) as f64 * self.period;
                let error = t_actual - predicted;
                let next = predicted + self.period + self.bw * error;
                // Clamp period to [50%, 200%] of nominal to prevent collapse
                self.period = (self.period + self.bw * self.bw * error)
                    .clamp(self.nominal_period * 0.5, self.nominal_period * 2.0);
                self.t_estimated = Some(next);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stream setup
// ---------------------------------------------------------------------------

fn open_stream(
    client_id: usize,
    client_pid: u32,
    req: &StreamOpenRequest,
    control: &Connection,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: u32,
    slot_count: u32,
    ramp_frames: u32,
) -> ClientStream {
    let client_period_frames = if req.sample_rate != device_sample_rate {
        ((device_period_frames as u64 * req.sample_rate as u64 + device_sample_rate as u64 - 1)
            / device_sample_rate as u64) as u32
    } else {
        device_period_frames
    };

    let sample_size: u32 = 2; // FORMAT_S16LE, validated before open_stream
    let client_frame_size = req.channels as u32 * sample_size;
    let client_period_bytes = client_period_frames * client_frame_size;

    let shm_size = AudioSlotHeader::SIZE as u32 + slot_count * client_period_bytes;
    let shm = SharedMemory::allocate(shm_size as usize);
    shm.grant(client_pid);
    let shm_token = shm.token();

    unsafe {
        let hdr = &*(shm.as_ptr() as *const AudioSlotHeader);
        hdr.write_idx.store(0, core::sync::atomic::Ordering::Relaxed);
        hdr.read_idx.store(0, core::sync::atomic::Ordering::Relaxed);
    }

    let pipe_fds = syscall::pipe();
    let signal_pipe_id = syscall::pipe_id(pipe_fds.read).expect("pipe_id failed");

    let slot_reader = AudioSlotReader::new(shm, client_period_bytes, slot_count);

    if control.send(MSG_STREAM_OPENED, &StreamOpenResponse {
        shm_token,
        signal_pipe_id,
        client_period_frames,
        client_period_bytes,
        device_sample_rate,
        device_channels,
        slot_count: slot_count as u16,
    }).is_err() {
        // Client died mid-open; the dropped control connection removes it.
        eprintln!("soundd: client {client_id} vanished during stream open");
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

    let mut gain = GainRamp::new(0.0);
    gain.set_target(1.0, ramp_frames);

    ClientStream {
        client_id,
        slot_reader,
        signal_write_fd: pipe_fds.write,
        signal_read_fd: pipe_fds.read,
        gain,
        client_channels: req.channels,
        client_period_frames,
        resampler,
        pending_removal: false,
    }
}

// ---------------------------------------------------------------------------
// Mix thread
// ---------------------------------------------------------------------------

/// Mix one period of `stream` into the bus. Returns false when the client's
/// ring could not supply a full period (silence mixed instead).
///
/// Slots are consumed peek→copy-out→advance: read_idx is published only after
/// the slot data has been decoded out of shared memory, so a concurrently
/// filling client can never overwrite a slot soundd is still reading.
fn mix_client(
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
        for frame in 0..produced {
            for ch in 0..device_channels {
                convert_buf[frame * device_channels + ch] = rs.output[ch][frame];
            }
        }
        accumulate(mix_f32, &convert_buf[..out_samples], device_channels, &mut stream.gain);
        return true;
    }

    let Some(slot) = stream.slot_reader.peek() else {
        stream.gain.advance_frames(device_period_frames as u32);
        return false;
    };
    decode_i16_to_f32(slot.data(), &mut decode_buf[..client_samples]);
    slot.advance();

    let src: &[f32] = if client_channels != device_channels {
        let out_samples = client_frames * device_channels;
        assert!(out_samples <= convert_buf.len());
        match (client_channels, device_channels) {
            (1, 2) => channel_convert_mono_to_stereo(&decode_buf[..client_samples], &mut convert_buf[..out_samples]),
            (2, 1) => channel_convert_stereo_to_mono(&decode_buf[..client_samples], &mut convert_buf[..out_samples]),
            (c, d) => panic!("soundd: unsupported channel conversion {c}→{d}"),
        }
        &convert_buf[..out_samples]
    } else {
        &decode_buf[..client_samples]
    };
    accumulate(mix_f32, src, device_channels, &mut stream.gain);
    true
}

fn mix_thread(
    audio_dev: AudioDev,
    cmd_ring: &CommandRing,
    cmd_pipe_read: Fd,
    dma_ptrs: Vec<*mut u8>,
    num_buffers: usize,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_bytes: usize,
    device_period_frames: usize,
    ramp_frames: u32,
) {
    let device_period_samples = device_period_frames * device_channels as usize;
    let period_nanos = (device_period_frames as u64 * 1_000_000_000) / device_sample_rate as u64;

    let mut streams: Vec<ClientStream> = Vec::new();
    let mut free_mask: u32 = 0;

    // Startup prime: nothing is in flight yet, so there is no free buffer for
    // the mix loop to fill and no client whose audio could fill it. This is
    // the only place silence is submitted unconditionally.
    for i in 0..num_buffers {
        let buf = unsafe { core::slice::from_raw_parts_mut(dma_ptrs[i], device_period_bytes) };
        buf.fill(0);
        // A swallowed submit failure would strand the buffer outside both
        // free_mask and the kernel's inflight set — silent pipeline shrink.
        toyos::audio::audio_submit(i as u32, device_period_bytes as u32)
            .unwrap_or_else(|e| panic!("soundd: audio_submit({i}) failed: {e}"));
    }

    syscall::set_rt_priority(true);

    let poller = Poller::new(64);
    let mut mix_f32 = vec![0.0f32; device_period_samples];
    // Sized for the highest client rate accepted at stream open, so the mix
    // path never allocates.
    let max_client_frames = (device_period_frames * MAX_CLIENT_RATE as usize)
        .div_ceil(device_sample_rate as usize);
    let mut decode_buf = vec![0.0f32; max_client_frames * 2];
    let mut convert_buf = vec![0.0f32; max_client_frames * 2];
    let mut dither_rng = Xorshift32((syscall::clock_nanos() as u32) | 1);
    let mut dll = Dll::new(period_nanos as f64);
    let mut records = [AudioCompletionRecord { mask: 0, _pad: 0, timestamp_nanos: 0 }; 16];

    const TOKEN_AUDIO: u64 = u64::MAX - 1;
    const TOKEN_CMD: u64 = u64::MAX - 2;

    let mut stats = MixStats::default();
    let mut next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;

    loop {
        let was_streaming = !streams.is_empty();

        // Signal all clients BEFORE the io_uring wait. Priority inheritance
        // boosts them to RT; they fill their ring slots while soundd is blocked
        // in the poller wait below. A write error means the client is gone —
        // its removal arrives via the dropped control connection.
        for stream in streams.iter() {
            let _ = syscall::write_nonblock(stream.signal_write_fd, &[1]);
        }

        let timeout = if streams.is_empty() {
            u64::MAX
        } else {
            match dll.t_estimated {
                None => period_nanos,
                Some(t_est) => {
                    let now = syscall::clock_nanos() as f64;
                    let target = if t_est > now {
                        t_est
                    } else {
                        // Past due: arm for the next future grid point, not a
                        // blind full period from now.
                        let k = ((now - t_est) / dll.period).floor() + 1.0;
                        t_est + k * dll.period
                    };
                    // timeout 0 is the kernel's non-blocking sentinel
                    ((target - now) as u64).max(1)
                }
            }
        };

        poller.poll_add(&audio_dev, IORING_POLL_IN, TOKEN_AUDIO);
        poller.poll_add_fd(cmd_pipe_read, IORING_POLL_IN, TOKEN_CMD);

        let mut cmd_ready = false;
        poller.wait(1, timeout, |token| match token {
            TOKEN_AUDIO => {}
            TOKEN_CMD => cmd_ready = true,
            other => panic!("soundd: unexpected poll token {other}"),
        });

        if was_streaming {
            stats.wakes += 1;
        }

        if cmd_ready {
            let mut drain = [0u8; 64];
            while matches!(syscall::read_nonblock(cmd_pipe_read, &mut drain), Ok(n) if n == drain.len()) {}
        }
        while let Some(cmd) = cmd_ring.pop() {
            match cmd {
                MixCommand::AddClient(client) => {
                    eprintln!("soundd: client {} connected (id={})", streams.len(), client.client_id);
                    let _ = syscall::write_nonblock(client.signal_write_fd, &[1]);
                    streams.push(*client);
                }
                MixCommand::RemoveClient(id) => {
                    if let Some(s) = streams.iter_mut().find(|s| s.client_id == id) {
                        s.gain.set_target(0.0, ramp_frames);
                        s.pending_removal = true;
                    }
                }
                MixCommand::SetVolume { client_id, target } => {
                    if let Some(s) = streams.iter_mut().find(|s| s.client_id == client_id) {
                        s.gain.set_target(target, ramp_frames);
                    }
                }
            }
        }

        if !was_streaming && !streams.is_empty() {
            stats = MixStats::default();
            next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;
        }

        let n_records = match audio_dev.read_completions(&mut records) {
            Ok(n) => n,
            Err(toyos_abi::syscall::SyscallError::WouldBlock) => 0,
            Err(e) => panic!("soundd: read_completions failed: {e:?}"),
        };
        if n_records > 0 {
            if let Some(t_est) = dll.t_estimated {
                let lateness = syscall::clock_nanos().saturating_sub(t_est as u64);
                if !streams.is_empty() {
                    stats.max_wake_lat_ns = stats.max_wake_lat_ns.max(lateness);
                }
            }
            let mut wake_completions = 0u32;
            for rec in &records[..n_records] {
                let n = rec.mask.count_ones();
                assert!(n > 0, "soundd: completion record with empty mask");
                assert_eq!(free_mask & rec.mask, 0, "soundd: repeated completion for free buffer");
                free_mask |= rec.mask;
                wake_completions += n;
                dll.update(rec.timestamp_nanos as f64, n);
            }
            if !streams.is_empty() {
                stats.completions += wake_completions;
                stats.max_batch = stats.max_batch.max(wake_completions);
            }
        }

        // §5.9: every buffer free means the pipeline fully drained — a
        // catastrophic stall. What died with it is the *clock*, not the audio:
        // the device restarts its period grid from whatever we submit next, so
        // the DLL estimate is meaningless and must be dropped, or the next
        // update reads the discontinuity as clock drift and drags the period.
        //
        // The buffers are refilled by the ordinary mix loop below, like any
        // other free buffer. A drain is not a reason to discard the client
        // audio already sitting in the ring: submitting a full pipeline of
        // silence would cost `num_buffers` periods of audible dropout for any
        // stall, however brief, and delay that client audio by the same
        // amount. Recovery costs only the periods the clients cannot cover.
        if free_mask.count_ones() as usize == num_buffers {
            if !streams.is_empty() {
                stats.drains += 1;
            }
            dll.reset();
        }

        while free_mask != 0 {
            let idx = free_mask.trailing_zeros() as usize;
            assert!(idx < num_buffers, "soundd: completion for nonexistent buffer {idx}");
            free_mask &= !(1 << idx);

            mix_f32.fill(0.0);

            let mut any_data = false;
            for stream in streams.iter_mut() {
                any_data |= mix_client(
                    stream,
                    &mut mix_f32,
                    &mut decode_buf,
                    &mut convert_buf,
                    device_channels as usize,
                    device_period_frames,
                );
            }

            let dma_buf = unsafe {
                core::slice::from_raw_parts_mut(dma_ptrs[idx] as *mut i16, device_period_samples)
            };
            for i in 0..device_period_samples {
                dma_buf[i] = dither_and_quantize(mix_f32[i], &mut dither_rng);
            }

            toyos::audio::audio_submit(idx as u32, device_period_bytes as u32)
                .unwrap_or_else(|e| panic!("soundd: audio_submit({idx}) failed: {e}"));
            if !streams.is_empty() {
                stats.submitted += 1;
                if !any_data { stats.underruns += 1; }
            }
        }

        // Disconnected clients leave only after their ramp-down finishes;
        // paused clients (§6.4) just mix silence and are never removed here.
        streams.retain(|s| {
            if s.pending_removal && s.gain.is_idle() {
                eprintln!("soundd: client {} removed", s.client_id);
                syscall::close(s.signal_write_fd);
                syscall::close(s.signal_read_fd);
                false
            } else {
                true
            }
        });

        // Flushing on the last disconnect closes the streaming phase out
        // completely: the tail between the final periodic window and the
        // client leaving is short-lived audio like a test tone's whole
        // second half, and dropping it would leave the gate blind to it.
        let now_ns = syscall::clock_nanos();
        if was_streaming && streams.is_empty() {
            stats.report(0);
            stats = MixStats::default();
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
        } else if now_ns >= next_stats_ns {
            if !streams.is_empty() {
                stats.report(streams.len());
                stats = MixStats::default();
            }
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
        }
    }
}

// ---------------------------------------------------------------------------
// Control thread
// ---------------------------------------------------------------------------

/// Reassembles one framed control message across nonblocking reads. The
/// control thread must never block on a client: a client parking a partial
/// header would otherwise wedge accept and volume/close/disconnect handling
/// for every other client (the mix thread is unaffected either way).
struct MsgBuf {
    buf: [u8; Self::MAX],
    len: usize,
}

impl MsgBuf {
    const HDR: usize = core::mem::size_of::<toyos::ipc::IpcHeader>();
    const PAYLOAD_MAX: usize = core::mem::size_of::<StreamOpenRequest>();
    const MAX: usize = Self::HDR + Self::PAYLOAD_MAX;

    fn new() -> Self {
        Self { buf: [0; Self::MAX], len: 0 }
    }

    fn payload_len(&self) -> usize {
        u32::from_le_bytes(self.buf[4..8].try_into().unwrap()) as usize
    }

    /// Pull bytes until a full message is buffered or the pipe runs dry.
    /// `Ok(Some(_))` is a complete `(msg_type, payload_len, payload)`;
    /// `Ok(None)` parks a partial message until the next readiness event;
    /// `Err(())` means EOF, a read error, or an oversized length — the
    /// caller must disconnect the client.
    fn recv(&mut self, conn: &Connection) -> Result<Option<(u32, usize, [u8; Self::PAYLOAD_MAX])>, ()> {
        loop {
            if self.len >= Self::HDR {
                let plen = self.payload_len();
                if self.len == Self::HDR + plen {
                    let msg_type = u32::from_le_bytes(self.buf[0..4].try_into().unwrap());
                    let mut payload = [0u8; Self::PAYLOAD_MAX];
                    payload[..plen].copy_from_slice(&self.buf[Self::HDR..Self::HDR + plen]);
                    self.len = 0;
                    return Ok(Some((msg_type, plen, payload)));
                }
            }
            let needed = if self.len < Self::HDR {
                Self::HDR
            } else {
                Self::HDR + self.payload_len()
            };
            match conn.read_nonblock(&mut self.buf[self.len..needed]) {
                Ok(0) => return Err(()),
                Ok(n) => {
                    self.len += n;
                    // Reads stop at the header boundary, so the length field
                    // is validated the moment it completes — before it can
                    // size a payload read.
                    if self.len == Self::HDR && self.payload_len() > Self::PAYLOAD_MAX {
                        return Err(());
                    }
                }
                Err(syscall::SyscallError::WouldBlock) => return Ok(None),
                Err(_) => return Err(()),
            }
        }
    }
}

fn reject_open(req: &StreamOpenRequest) -> Option<&'static str> {
    if req.format != FORMAT_S16LE {
        return Some("unsupported sample format");
    }
    if req.channels != 1 && req.channels != 2 {
        return Some("unsupported channel count");
    }
    if !(MIN_CLIENT_RATE..=MAX_CLIENT_RATE).contains(&req.sample_rate) {
        return Some("unsupported sample rate");
    }
    None
}

fn control_thread(
    listener: toyos::Listener,
    cmd_ring: &CommandRing,
    cmd_pipe_write: Fd,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: u32,
    slot_count: u32,
    ramp_frames: u32,
) {
    let poller = Poller::new(32);

    struct ControlClient {
        conn: Connection,
        msg: MsgBuf,
        // Set once MSG_STREAM_OPEN succeeds; accepted-but-silent connections
        // stay pending so they cannot stall the control plane.
        stream_idx: Option<usize>,
    }

    let mut clients: Vec<ControlClient> = Vec::new();
    let mut client_pids: Vec<u32> = Vec::new();
    let mut next_idx: usize = 0;

    const TOKEN_LISTENER: u64 = u64::MAX;

    loop {
        poller.poll_add(&listener, IORING_POLL_IN, TOKEN_LISTENER);
        for (i, client) in clients.iter().enumerate() {
            poller.poll_add(&client.conn, IORING_POLL_IN, i as u64);
        }

        let mut ready: Vec<u64> = Vec::new();
        poller.wait(1, u64::MAX, |t| ready.push(t));

        if ready.contains(&TOKEN_LISTENER) {
            match services::accept(&listener) {
                Ok(accepted) => {
                    clients.push(ControlClient { conn: accepted.conn, msg: MsgBuf::new(), stream_idx: None });
                    client_pids.push(accepted.client_pid);
                }
                Err(e) => eprintln!("soundd: accept failed: {e:?}"),
            }
        }

        let mut dead: Vec<usize> = Vec::new();
        for i in 0..clients.len() {
            if !ready.contains(&(i as u64)) {
                continue;
            }
            'msgs: loop {
                let c = &mut clients[i];
                let (msg_type, plen, payload) = match c.msg.recv(&c.conn) {
                    Ok(Some(m)) => m,
                    Ok(None) => break 'msgs,
                    Err(()) => {
                        if let Some(idx) = c.stream_idx {
                            cmd_ring.push(MixCommand::RemoveClient(idx));
                            let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                        }
                        dead.push(i);
                        break 'msgs;
                    }
                };
                match (msg_type, clients[i].stream_idx) {
                    (MSG_STREAM_OPEN, None) if plen == core::mem::size_of::<StreamOpenRequest>() => {
                        let req = StreamOpenRequest {
                            sample_rate: u32::from_le_bytes(payload[0..4].try_into().unwrap()),
                            channels: u16::from_le_bytes(payload[4..6].try_into().unwrap()),
                            format: u16::from_le_bytes(payload[6..8].try_into().unwrap()),
                        };
                        if let Some(reason) = reject_open(&req) {
                            eprintln!("soundd: rejecting stream ({reason}): {}Hz {}ch fmt={}",
                                req.sample_rate, req.channels, req.format);
                            let _ = clients[i].conn.signal(MSG_STREAM_ERROR);
                            dead.push(i);
                            break 'msgs;
                        }
                        eprintln!("soundd: opening stream: {}Hz {}ch fmt={}",
                            req.sample_rate, req.channels, req.format);
                        let idx = next_idx;
                        next_idx += 1;
                        let client = open_stream(
                            idx,
                            client_pids[i],
                            &req,
                            &clients[i].conn,
                            device_sample_rate,
                            device_channels,
                            device_period_frames,
                            slot_count,
                            ramp_frames,
                        );
                        cmd_ring.push(MixCommand::AddClient(Box::new(client)));
                        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                        clients[i].stream_idx = Some(idx);
                    }
                    (MSG_STREAM_SET_VOLUME, Some(idx)) if plen == core::mem::size_of::<StreamSetVolume>() => {
                        let gain = f32::from_le_bytes(payload[0..4].try_into().unwrap());
                        cmd_ring.push(MixCommand::SetVolume {
                            client_id: idx,
                            target: gain.clamp(0.0, 1.0),
                        });
                        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                    }
                    (MSG_STREAM_CLOSE, Some(idx)) => {
                        cmd_ring.push(MixCommand::RemoveClient(idx));
                        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                        dead.push(i);
                        break 'msgs;
                    }
                    (other, _) => {
                        eprintln!("soundd: protocol violation (msg {other}), disconnecting client");
                        if let Some(idx) = clients[i].stream_idx {
                            cmd_ring.push(MixCommand::RemoveClient(idx));
                            let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                        }
                        dead.push(i);
                        break 'msgs;
                    }
                }
            }
        }
        for &i in dead.iter().rev() {
            clients.remove(i);
            client_pids.remove(i);
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let listener = services::listen("soundd").expect("soundd already running");

    let audio_dev = AudioDev::open().expect("soundd: no audio device");
    let info: AudioInfo = audio_dev.info().expect("soundd: failed to read audio info");

    let num_buffers = info.num_buffers as usize;
    let device_sample_rate = info.sample_rate;
    let device_channels = info.channels as u16;
    let device_period_bytes = info.period_bytes as usize;
    let device_period_frames = device_period_bytes / (device_channels as usize * 2);

    assert!(device_channels == 1 || device_channels == 2,
        "soundd: unsupported device channel count {device_channels}");
    // Ring indices wrap mod 2^32, so slot_count must divide it evenly.
    assert!(num_buffers.is_power_of_two(), "soundd: num_buffers must be a power of two");

    // Client ring depth matches the DMA pipeline depth: a wake gap can free
    // at most num_buffers periods, so a full client ring always covers it.
    let slot_count = num_buffers as u32;

    let dma_page = SharedMemory::map(info.dma_token, 2 * 1024 * 1024);
    let dma_base = dma_page.as_ptr();
    let mut dma_ptrs: Vec<*mut u8> = Vec::with_capacity(num_buffers);
    for i in 0..num_buffers {
        dma_ptrs.push(unsafe { dma_base.add(info.buf_offsets[i] as usize) });
    }

    audio_dev.start().expect("soundd: failed to start audio");

    // ~5ms connect/disconnect/volume ramp
    let ramp_frames = device_sample_rate * 5 / 1000;

    eprintln!("soundd: ready, {} buffers, {}Hz {}ch, {} bytes/period, {} frames/period",
        num_buffers, device_sample_rate, device_channels, device_period_bytes, device_period_frames);

    let cmd_ring = Arc::new(CommandRing::new());
    let cmd_pipe = syscall::pipe();

    let cmd_ring2 = cmd_ring.clone();
    std::thread::Builder::new()
        .name("soundd-ctrl".into())
        .spawn(move || {
            control_thread(
                listener,
                &cmd_ring2,
                cmd_pipe.write,
                device_sample_rate,
                device_channels,
                device_period_frames as u32,
                slot_count,
                ramp_frames,
            );
        })
        .expect("soundd: failed to spawn control thread");

    mix_thread(
        audio_dev,
        &cmd_ring,
        cmd_pipe.read,
        dma_ptrs,
        num_buffers,
        device_sample_rate,
        device_channels,
        device_period_bytes,
        device_period_frames,
        ramp_frames,
    );
}
