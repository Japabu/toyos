use toyos_abi::audio::{AudioCompletionRecord, AudioInfo, AudioSlotHeader};
use toyos_abi::Fd;
use toyos::audio::{
    AudioSlotReader, StreamOpenRequest, StreamOpenResponse, StreamSetVolume, FORMAT_S16LE,
    MSG_STREAM_OPEN, MSG_STREAM_OPENED, MSG_STREAM_SET_VOLUME, MSG_STREAM_CLOSE, MSG_STREAM_ERROR,
};
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::services;
use toyos::shm::SharedMemory;
use toyos::{AsHandle, AudioDev, Connection};
use toyos_abi::syscall;

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

mod hda;

/// The device half of the mix loop.
///
/// Two implementations and no more: `specs/hda-driver-plan.md` §8 item 10
/// forbids a framework before there are three. What differs between a card the
/// kernel drives and one this process drives is exactly the six methods below;
/// the mixer, the ramps, the DLL, the underrun accounting and the
/// suspend/resume structure are one body of code either way, which is what
/// makes gate A one instrument for both.
trait Backend {
    /// The handle a completion arrives on.
    fn handle(&self) -> Fd;

    /// Where period `idx`'s samples go. Device memory this process may write,
    /// mapped once at claim.
    fn buffer(&self, idx: usize) -> *mut u8;

    /// Completion records, oldest first, into `out`. `0` is nothing pending.
    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize;

    /// Period `idx` has played and is soundd's again.
    ///
    /// HDA's engine is cyclic and never stops on its own, so a period nobody
    /// refills is *replayed* — audible harm gate A's gap detector cannot see.
    /// Zeroing it as it frees makes a late soundd cost silence instead, which
    /// is exactly what virtio-sound's device does when it runs dry, so one
    /// instrument certifies both (§2.4). virtio's own implementation is empty
    /// for that reason and not by omission.
    fn released(&mut self, idx: usize);

    /// Period `idx` is filled with `bytes` of PCM and belongs to the device.
    fn submit(&mut self, idx: usize, bytes: usize);

    /// Stop the stream. Idempotent, and cheap when it is already stopped.
    fn stop(&mut self);
}

struct VirtioBackend {
    dev: AudioDev,
    buffers: Vec<*mut u8>,
}

impl Backend for VirtioBackend {
    fn handle(&self) -> Fd {
        self.dev.as_handle()
    }

    fn buffer(&self, idx: usize) -> *mut u8 {
        self.buffers[idx]
    }

    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize {
        match self.dev.read_completions(out) {
            Ok(n) => n,
            Err(syscall::SyscallError::WouldBlock) => 0,
            Err(e) => panic!("soundd: read_completions failed: {e:?}"),
        }
    }

    fn released(&mut self, _idx: usize) {}

    fn submit(&mut self, idx: usize, bytes: usize) {
        toyos::audio::audio_submit(idx as u32, bytes as u32)
            .unwrap_or_else(|e| panic!("soundd: audio_submit({idx}) failed: {e}"));
    }

    fn stop(&mut self) {
        self.dev.stop().expect("soundd holds the audio device claim");
    }
}

struct HdaBackend {
    hda: hda::Hda,
    buffers: Vec<*mut u8>,
    period_bytes: usize,
}

impl Backend for HdaBackend {
    fn handle(&self) -> Fd {
        self.hda.dev().as_handle()
    }

    fn buffer(&self, idx: usize) -> *mut u8 {
        self.buffers[idx]
    }

    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize {
        match self.hda.dev().completions() {
            Ok(record) => {
                out[0] = record;
                1
            }
            Err(syscall::SyscallError::WouldBlock) => 0,
            Err(e) => panic!("soundd: hda completions failed: {e:?}"),
        }
    }

    fn released(&mut self, idx: usize) {
        unsafe { core::ptr::write_bytes(self.buffers[idx], 0, self.period_bytes) };
    }

    fn submit(&mut self, _idx: usize, _bytes: usize) {
        self.hda.start();
    }

    fn stop(&mut self) {
        self.hda.stop();
    }
}

use rubato::{Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction};

const MIN_CLIENT_RATE: u32 = 8_000;
const MAX_CLIENT_RATE: u32 = 192_000;
const STATS_INTERVAL_NANOS: u64 = 2_000_000_000;

struct ClientResampler {
    resampler: SincFixedOut<f32>,
    /// Planar (per-channel) client audio awaiting resampling. SincFixedOut
    /// consumes a varying `input_frames_next()` per call, so slots are pulled
    /// into this buffer on demand instead of fed one fixed chunk per cycle.
    accum: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

/// A gain that has already crossed the trust boundary: finite, and within
/// §7.4's [0.0, 1.0].
///
/// The check has to be a type rather than a `clamp` at each call site: `clamp`
/// returns NaN unchanged, and a NaN gain reaches the *shared* mix bus through
/// `accumulate`, silencing every stream.
#[derive(Clone, Copy)]
struct Gain(f32);

impl Gain {
    const SILENT: Gain = Gain(0.0);
    const UNITY: Gain = Gain(1.0);

    /// §7.4 clamps out-of-range values, and ±inf is out of range. NaN is not a
    /// value at all, so it is refused rather than guessed at; the control
    /// thread treats a refusal as any other malformed message.
    fn from_wire(gain: f32) -> Option<Gain> {
        if gain.is_nan() {
            return None;
        }
        Some(Gain(gain.clamp(0.0, 1.0)))
    }

    fn raw(self) -> f32 {
        self.0
    }
}

struct GainRamp {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl GainRamp {
    fn new(initial: Gain) -> Self {
        Self { current: initial.raw(), target: initial.raw(), step: 0.0, remaining: 0 }
    }

    fn set_target(&mut self, target: Gain, ramp_frames: u32) {
        self.target = target.raw();
        self.step = (self.target - self.current) / ramp_frames as f32;
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
    /// soundd's own reference to the signal pipe's read end, released the
    /// moment the client proves it holds one (see the mix loop). While soundd
    /// holds it the pipe has a reader whatever the client does, and §5.7's
    /// crash detection cannot fire.
    signal_read_fd: Option<Fd>,
    gain: GainRamp,
    client_channels: u16,
    client_period_frames: u32,
    resampler: Option<ClientResampler>,
    /// Latched by the first period this client supplies.
    delivered: bool,
    pending_removal: bool,
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
    fn is_streaming(&self) -> bool {
        self.delivered && !self.pending_removal
    }
}

/// Control connections soundd will hold at once.
///
/// The control thread watches one handle per client plus the listener in a
/// single poller and io_uring rings are powers of two, so the limit is a ring
/// size minus one. 64 costs the same 2 MiB page as 32; 63 simultaneous streams
/// is already past what the mixer renders inside one 2.9 ms period, and costs
/// 189 of the kernel's 1024 fds (a control connection plus both signal pipe
/// ends per client).
const MAX_CONTROL_CLIENTS: usize = 63;

/// Deep enough that one pass of the control loop can never fill it: a pass
/// pushes at most one `AddClient` (there is one accept per wait) plus, per
/// connected client, one coalesced `SetVolume` and one `RemoveClient`.
const CMD_RING_SIZE: u32 = 256;
const _: () = assert!(CMD_RING_SIZE as usize >= 1 + 2 * MAX_CONTROL_CLIENTS);

enum MixCommand {
    AddClient(Box<ClientStream>),
    RemoveClient(usize),
    SetVolume { client_id: usize, target: Gain },
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

    /// Hands the command back when the ring is full rather than dropping it (a
    /// ghost client: leaked shm, an app waiting on a stream nothing mixes) or
    /// asserting — a client chooses the load, since the control thread drains
    /// everything it has written before yielding. See `submit`, which waits.
    #[must_use]
    fn try_push(&self, cmd: MixCommand) -> Result<(), MixCommand> {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= CMD_RING_SIZE {
            return Err(cmd);
        }
        let idx = (w % CMD_RING_SIZE) as usize;
        unsafe { (*self.slots.get())[idx] = Some(cmd); }
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
        Ok(())
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

/// One scale for both directions of the i16 <-> f32 conversion.
///
/// Decoding by 32768 and quantizing by 32767 is not a round trip: it is a gain
/// of 32767/32768 on everything that passes through, and 32703 of the 65536
/// i16 values come back one LSB different from what the client sent. 32768 is
/// the correct constant in both directions because it is the magnitude of
/// `i16::MIN`; the positive end is one code short of full scale, which is what
/// the clamp is for and what two's complement costs.
const I16_SCALE: f32 = 32768.0;

fn decode_i16_to_f32(src: &[u8], dst: &mut [f32]) {
    for i in 0..dst.len() {
        let sample = i16::from_le_bytes([src[i * 2], src[i * 2 + 1]]);
        dst[i] = sample as f32 / I16_SCALE;
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

struct Xorshift32(u32);

impl Xorshift32 {
    fn next(&mut self) -> f32 {
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
fn dither_and_quantize(sample: f32, rng: &mut Xorshift32) -> i16 {
    let dither = rng.next() + rng.next(); // triangular PDF in [-1.0, 1.0]
    quantize(sample, dither)
}

/// Split out from `dither_and_quantize` so the scale can be checked against
/// every i16 there is without a generator in the way.
fn quantize(sample: f32, dither: f32) -> i16 {
    (sample * I16_SCALE + dither).round().clamp(-32768.0, 32767.0) as i16
}

/// Counters for one reporting window. A window covers streaming only: zeroed
/// when the first client arrives, flushed when the last one leaves, so no
/// number here is diluted by the idle path — where soundd waits on raw
/// completion IRQs with no timer and a batched IRQ is indistinguishable from a
/// missed deadline. The audio gate reads these (`tests/audio-baseline.toml`),
/// so each has to mean exactly one thing.
#[derive(Default)]
struct MixStats {
    wakes: u32,
    completions: u32,
    /// Every period put on the wire in this window, underruns included.
    submitted: u32,
    /// Periods submitted with no client audio behind them *while at least one
    /// client was streaming* (`ClientStream::is_streaming`) — silence that
    /// interrupted a stream rather than preceding or following one. Strictly
    /// narrower than `submitted`, which like `wakes`/`completions`/`drains`
    /// covers the whole time soundd has clients.
    underruns: u32,
    /// Cycles that found the whole DMA pipeline free (§5.9) *and* could only
    /// have got there by soundd being late. A device that retires the pipeline
    /// faster than it plays it empties the free list without soundd having
    /// missed anything; see the count site.
    drains: u32,
    /// Worst overshoot of a DLL prediction soundd actually armed a timer on
    /// (§5.1). Waits that named no wake time contribute nothing; see the
    /// sample site.
    max_wake_lat_ns: u64,
    max_batch: u32,
    /// Free buffers left unfilled because a streaming client was still
    /// producing the period that belongs in them (§5.10) — an activity signal,
    /// not a fault, and so uncapped.
    deferred: u32,
}

impl MixStats {
    fn report(&self, clients: usize) {
        eprintln!("soundd: wakes={} completions={} submitted={} underruns={} drains={} max_wake_lat_us={} max_batch={} clients={} deferred={}",
            self.wakes, self.completions, self.submitted, self.underruns, self.drains,
            self.max_wake_lat_ns / 1_000, self.max_batch, clients, self.deferred);
    }
}

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
) -> Option<ClientStream> {
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
    // The ring is this client's and no stream exists without it, so both
    // refusals end the open rather than the daemon: a client that exited
    // between asking and being served cannot be granted memory, and neither
    // can one that asked while the machine had none.
    let shm = match SharedMemory::allocate(shm_size as usize) {
        Ok(shm) => shm,
        Err(e) => {
            eprintln!("soundd: no {shm_size}-byte ring for client {client_id} ({e:?})");
            return None;
        }
    };
    if shm.grant(client_pid).is_err() {
        eprintln!("soundd: client {client_id} (pid {client_pid}) is gone; no stream opened");
        return None;
    }
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
        _pad0: 0,
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

    let mut gain = GainRamp::new(Gain::SILENT);
    gain.set_target(Gain::UNITY, ramp_frames);

    Some(ClientStream {
        client_id,
        slot_reader,
        signal_write_fd: pipe_fds.write,
        signal_read_fd: Some(pipe_fds.read),
        gain,
        client_channels: req.channels,
        client_period_frames,
        resampler,
        delivered: false,
        pending_removal: false,
    })
}

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

/// Signal every client before the wait so priority inheritance can fill their
/// rings while soundd blocks, and reap the ones that died doing it.
///
/// §5.7/§7.3: a broken pipe here is the client's death, caught here rather than
/// left to the control connection — a client that dies mid-stream would
/// otherwise stay `is_streaming()` and keep the loop deferring buffers for a
/// producer that no longer exists. Death is exactly `Err(NotFound)`, the
/// kernel's broken-pipe error; a full pipe is `Err(WouldBlock)` and means the
/// client is merely behind on consuming signals, which must leave it untouched
/// — a §6.4-paused client stops reading its pipe indefinitely and is alive.
fn signal_clients(streams: &mut [ClientStream], ramp_frames: u32) {
    for stream in streams.iter_mut() {
        let died = matches!(
            syscall::write_nonblock(stream.signal_write_fd, &[1]),
            Err(syscall::SyscallError::NotFound)
        );
        if died && stream.signal_read_fd.is_none() && !stream.pending_removal {
            eprintln!("soundd: client {} died, ramping down", stream.client_id);
            stream.gain.set_target(Gain::SILENT, ramp_frames);
            stream.pending_removal = true;
        }
    }
}

/// Drain the command ring the control thread fills: connects, disconnects, and
/// volume changes. Shared by both sinks — a client's lifecycle is the same
/// whether its audio reaches hardware or a discard.
fn apply_commands(cmd_ring: &CommandRing, streams: &mut Vec<ClientStream>, ramp_frames: u32) {
    while let Some(cmd) = cmd_ring.pop() {
        match cmd {
            MixCommand::AddClient(client) => {
                eprintln!("soundd: client {} connected (id={})", streams.len(), client.client_id);
                let _ = syscall::write_nonblock(client.signal_write_fd, &[1]);
                streams.push(*client);
            }
            MixCommand::RemoveClient(id) => {
                if let Some(s) = streams.iter_mut().find(|s| s.client_id == id) {
                    s.gain.set_target(Gain::SILENT, ramp_frames);
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
}

/// Drop clients whose disconnect ramp has finished. Paused clients (§6.4) mix
/// silence and are never removed here; a disconnecting client leaves only after
/// its §5.5 ramp-out reaches idle, so its tail plays out first.
fn retain_active(streams: &mut Vec<ClientStream>) {
    streams.retain(|s| {
        if s.pending_removal && s.gain.is_idle() {
            eprintln!("soundd: client {} removed", s.client_id);
            syscall::close(s.signal_write_fd);
            if let Some(fd) = s.signal_read_fd {
                syscall::close(fd);
            }
            false
        } else {
            true
        }
    });
}

fn mix_thread(
    backend: &mut dyn Backend,
    cmd_ring: &CommandRing,
    cmd_pipe_read: Fd,
    num_buffers: usize,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_bytes: usize,
    device_period_frames: usize,
    ramp_frames: u32,
) {
    let device_period_samples = device_period_frames * device_channels as usize;
    let period_nanos = (device_period_frames as u64 * 1_000_000_000) / device_sample_rate as u64;
    // The device plays one period per `period_nanos`, so the wall-clock cost of
    // emptying the pipeline is bounded from below. Every buffer is in flight
    // the moment the mix loop finishes submitting, and the head one is only
    // part-played, so more than `(num_buffers - 1)` periods of audio are still
    // unplayed at that instant. See the drain count site.
    let min_drain_nanos = (num_buffers as u64 - 1) * period_nanos;
    // How much unplayed audio must still be on the wire before the mix loop may
    // defer a buffer for a client that is mid-refill (§5.10). Policy, not
    // physics, with the same standing as the kernel's `MAX_USER_STR`: of the
    // pipeline's 8 periods, soundd spends at most 3 waiting for a client and
    // always keeps 5 in reserve. It cannot be derived from worst-case wake
    // lateness — the recorded worst exceeds two whole pipelines, so no floor
    // inside the pipeline covers it. Move it only with a full re-baseline.
    assert!(num_buffers > 5, "soundd: pipeline too shallow to defer safely");
    let refill_floor_nanos = 5 * period_nanos;

    let mut streams: Vec<ClientStream> = Vec::new();
    // Boot starts SUSPENDED (§5.8): every buffer free, nothing submitted, the
    // PCM stream never started. There is no unconditional silence prime — the
    // first client's ordinary refill fills the whole pipeline through the
    // dithering mix path, and the kernel starts the stream on that submit.
    let mut free_mask: u32 = (1u32 << num_buffers) - 1;
    // Whether the device stream is running, i.e. soundd has submitted since
    // the last stop. Owned here: the kernel's own started flag is not
    // readable, and the two agree because every submit starts a stopped
    // stream and only the suspend block below stops it.
    //
    // Establish that agreement instead of assuming it. `false` is a claim
    // about kernel state, and it is only true if no soundd ran before this
    // one: the audio claim is released on descriptor close, so a soundd that
    // died inside the drain window — last completion drained, STOP not yet
    // issued — leaves the stream STARTED with an empty queue, and a successor
    // that merely believed it stopped would park forever with the host voice
    // open in permanent underrun, at exactly zero CPU. One STOP makes the
    // belief true. It costs nothing on an ordinary boot: the kernel's
    // `SoundController::stop` returns without a controlq round trip or a log
    // line when the stream is already stopped.
    backend.stop();
    let mut started = false;
    // Wall clock at the last instant the pipeline was known full. Re-stamped
    // after every refill; read only by the drain count site.
    let mut pipeline_filled_ns = syscall::clock_nanos();
    // Wall clock at which everything submitted will have finished playing. The
    // device plays one period per `period_nanos` and cannot play faster, so
    // this is the only honest measure of how much audio is still on the wire.
    // The free list is not: QEMU retires a whole pipeline in a few ms, so
    // "free" says nothing about what has been heard. Nothing is on the wire
    // at boot.
    let mut playout_until_ns = pipeline_filled_ns;

    // Gated on the audio device claim `main` already took, so a refusal is a
    // kernel bug. Mixing on without the RT band would show up only as glitches.
    syscall::set_rt_priority(true).expect("soundd holds the audio device claim");

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

    // Buffers the previous cycle deliberately left unfilled. Read by the drain
    // site, which must not mistake soundd's own restraint for a device stall.
    let mut deferred_last: u32 = 0;
    let mut stats = MixStats::default();
    let mut next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;

    // Exactly one emission of one of these markers is gate-asserted: the
    // `soundd: suspended` printed by the suspend block below, which
    // `check_suspend_structure` (tests/common/audio.rs) requires after the
    // last client removal on every audio run. That one must stay a single
    // format piece so it lands contiguously on the shared console.
    //
    // This boot emission is not asserted — the gate's capture opens at
    // ===TEST_START, long after soundd starts — and `soundd: resumed` is read
    // by no test at all. Renaming either of those two breaks nothing that
    // would tell you; they are diagnostics.
    eprintln!("soundd: suspended");

    loop {
        let was_streaming = !streams.is_empty();

        // Signal all clients BEFORE the io_uring wait, so priority inheritance
        // fills their ring slots while soundd is blocked in the poller below.
        signal_clients(&mut streams, ramp_frames);

        // The prediction this wait is armed against, when there is one.
        // Lateness is only defined relative to an instant soundd asked to be
        // woken at, and two waits name none: the idle path (§5.8) arms no timer
        // at all, and before the DLL locks there is no prediction to arm on.
        let mut armed_on: Option<f64> = None;

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
                    armed_on = Some(t_est);
                    // timeout 0 is the kernel's non-blocking sentinel
                    ((target - now) as u64).max(1)
                }
            }
        };

        poller.poll_add_fd(backend.handle(), IORING_POLL_IN, TOKEN_AUDIO);
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
        apply_commands(cmd_ring, &mut streams, ramp_frames);

        if !was_streaming && !streams.is_empty() {
            stats = MixStats::default();
            next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;
        }

        let n_records = backend.completions(&mut records);
        if n_records > 0 {
            // Measured against the prediction this wait was *armed* on, not
            // against whatever the DLL holds when the wait returns. They differ
            // on a window's first wake, armed while soundd was still idle and
            // asking for no wake time at all — reading the estimate directly
            // scores that sleep as a missed deadline. Nothing is hidden:
            // whenever soundd armed a timer the distance from that prediction
            // is the sample, however large.
            if let Some(t_est) = armed_on {
                let lateness = syscall::clock_nanos().saturating_sub(t_est as u64);
                stats.max_wake_lat_ns = stats.max_wake_lat_ns.max(lateness);
            }
            let mut wake_completions = 0u32;
            for rec in &records[..n_records] {
                let n = rec.mask.count_ones();
                assert!(n > 0, "soundd: completion record with empty mask");
                assert_eq!(free_mask & rec.mask, 0, "soundd: repeated completion for free buffer");
                free_mask |= rec.mask;
                // §2.4's zero-on-complete, before anything can decide to leave
                // this buffer unfilled: the engine returns to it in
                // `num_buffers` periods whatever soundd does.
                for idx in 0..num_buffers {
                    if rec.mask & (1 << idx) != 0 {
                        backend.released(idx);
                    }
                }
                wake_completions += n;
                dll.update(rec.timestamp_nanos as f64, n);
            }
            if !streams.is_empty() {
                stats.completions += wake_completions;
                stats.max_batch = stats.max_batch.max(wake_completions);
            }
        }

        // §5.9: every buffer free means the pipeline drained. What died with it
        // is the *clock*, not the audio — the device restarts its period grid
        // from whatever we submit next, so the DLL estimate must be dropped or
        // the next update reads the discontinuity as drift and drags the
        // period. The buffers themselves are refilled by the ordinary mix loop
        // below: submitting a full pipeline of silence instead would cost
        // `num_buffers` periods of audible dropout for a stall of any length.
        //
        // Counting a drain is narrower than detecting one. `drains` means
        // "soundd was late enough that the device ran out of audio", so the
        // three ways to see a full free list without being late must not raise
        // it: the idle path (§5.8) empties the pipeline by design and is the
        // only wake with `was_streaming` false; a device retiring faster than
        // it plays is rejected arithmetically by `min_drain_nanos`, which no
        // device playing at its own rate can beat; and a previous cycle's
        // deferral is soundd's own restraint, not a stall, so it suppresses the
        // DLL reset too.
        if free_mask.count_ones() as usize == num_buffers && deferred_last == 0 {
            let since_filled = syscall::clock_nanos().saturating_sub(pipeline_filled_ns);
            if was_streaming && since_filled >= min_drain_nanos {
                stats.drains += 1;
            }
            dll.reset();
        }

        let mut refilled = false;
        let mut deferred: u32 = 0;
        // With no clients there is nothing to mix: leaving the freed buffers
        // unsubmitted is what drains the pipeline (§5.8) instead of feeding
        // the device silence forever.
        while free_mask != 0 && !streams.is_empty() {
            let idx = free_mask.trailing_zeros() as usize;
            assert!(idx < num_buffers, "soundd: completion for nonexistent buffer {idx}");
            free_mask &= !(1 << idx);

            // §5.10's "wait until clients have filled", reached by deferring
            // the buffer rather than blocking on the client — which needs no
            // reverse notification, since the ring indices soundd already maps
            // say the same thing.
            //
            // A streaming client whose ring is empty was signalled microseconds
            // ago and is mid-callback, not absent. The ring is `num_buffers`
            // deep precisely so a mix cycle that outruns the client costs
            // margin rather than audio; filling this buffer with silence spends
            // that margin on the one thing it exists to prevent. Deferring is
            // safe for exactly as long as audio already on the wire has not run
            // out, so a client that stops producing altogether still costs
            // silence — at the floor rather than immediately.
            let now = syscall::clock_nanos();
            let mid_refill = streams
                .iter()
                .any(|s| s.is_streaming() && s.slot_reader.peek().is_none());
            if mid_refill && playout_until_ns.saturating_sub(now) >= refill_floor_nanos {
                deferred |= 1 << idx;
                stats.deferred += 1;
                continue;
            }

            mix_f32.fill(0.0);

            let mut any_data = false;
            let mut any_streaming = false;
            for stream in streams.iter_mut() {
                let covered = mix_client(
                    stream,
                    &mut mix_f32,
                    &mut decode_buf,
                    &mut convert_buf,
                    device_channels as usize,
                    device_period_frames,
                );
                if covered && !stream.delivered {
                    stream.delivered = true;
                    // §5.7 wants a client crash to break soundd's next write to
                    // the signal pipe, which it cannot while soundd holds a
                    // read end of its own. A delivered period proves the client
                    // holds one: `AudioStream::open` maps the ring and opens
                    // the pipe before returning, and the slot-filling thread is
                    // spawned after that. A client that fills without ever
                    // opening the pipe loses its stream on the next signal.
                    if let Some(fd) = stream.signal_read_fd.take() {
                        syscall::close(fd);
                    }
                }
                any_data |= covered;
                any_streaming |= stream.is_streaming();
            }

            let dma_buf = unsafe {
                core::slice::from_raw_parts_mut(backend.buffer(idx) as *mut i16, device_period_samples)
            };
            for i in 0..device_period_samples {
                dma_buf[i] = dither_and_quantize(mix_f32[i], &mut dither_rng);
            }

            if !started {
                started = true;
                // Before the submit, because that is where a stopped stream is
                // started: the marker has to precede whatever the backend logs
                // about starting.
                eprintln!("soundd: resumed");
            }
            backend.submit(idx, device_period_bytes);
            // Plays after whatever is already queued — unless that has all
            // played out, in which case the device restarts from now.
            playout_until_ns = playout_until_ns.max(now) + period_nanos;
            refilled = true;
            stats.submitted += 1;
            if any_streaming && !any_data { stats.underruns += 1; }
        }
        // Deferred buffers stay free and are reconsidered next cycle, by which
        // point the client has had another signal-to-mix window to produce.
        free_mask |= deferred;
        deferred_last = deferred;
        // Not re-stamped by a cycle that only deferred: no audio was added, so
        // the pipeline's remaining depth still dates from the previous fill.
        if refilled {
            pipeline_filled_ns = syscall::clock_nanos();
        }

        retain_active(&mut streams);

        // §5.8 DRAINING → SUSPENDED, on the completion that frees the last
        // buffer. The stop is immediate: grace between the drain and the PCM
        // STOP is zero, and that is policy like `refill_floor_nanos` above,
        // not physics. virtio STOP does not RELEASE — SET_PARAMS and PREPARE
        // stay valid and resume is one control verb inline with the first
        // submit — so there is no codec pop or renegotiation for grace to
        // amortize, and stopping at once is what puts the suspend markers
        // inside the audio gate's serial window on every run. The one event
        // that makes grace nonzero is a hardware backend that pops on stop,
        // advertised per-backend through `AudioInfo`; implement it then as a
        // clock comparison against a drain stamp, evaluated at the idle wakes
        // that still arrive while the buffers play out — never as an armed
        // timer, which would put a periodic wake back into the idle path this
        // whole state exists to empty.
        //
        // This block must not move into the full-drain site above:
        // that site is gated on `deferred_last == 0`, and a final streaming
        // cycle that deferred plus a whole-pipeline completion batch — QEMU's
        // routine cadence — would skip it, parking soundd forever with the
        // device started and nothing left to complete. The `started` guard
        // keeps a stray cmd wake (a SetVolume for a removed client) from
        // costing a controlq round trip.
        if started && streams.is_empty() && free_mask.count_ones() as usize == num_buffers {
            // The device's period grid dies with the stream; the next
            // completion after resume re-initializes the estimate.
            dll.reset();
            backend.stop();
            started = false;
            eprintln!("soundd: suspended");
        }

        // Flushing on the last disconnect keeps the tail between the final
        // periodic window and the client leaving in the record — for a stream
        // shorter than two windows that tail is most of it.
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

/// The default output on a machine with no audio hardware. It presents the same
/// virtual device every client negotiates against and drains each stream at that
/// real rate off a monotonic software clock, discarding the mix. Hardware
/// absence is a routing state, never an error: a client's write and backpressure
/// timing is identical to a real device, so nothing upstream can tell its audio
/// reaches nowhere.
///
/// The null sink *is* the mix loop clocked by a timer instead of a device. It
/// reuses every per-client mechanism — `mix_client`, the gain ramps, crash
/// detection, the command ring — and drops only what a device provides: there is
/// no DMA pipeline, so no DLL, no completion records, no dither, and no submit.
/// After mixing one period it throws the samples away.
///
/// Idle discipline matches §5.8: with no streams it holds no timer and takes no
/// wakes, blocking on the command pipe alone, so an audience of zero costs
/// exactly zero CPU. It does not request the RT band — it protects no audible
/// output, and could not anyway: `SYS_SET_RT_PRIORITY` is gated on the audio
/// claim there is no device to take.
fn null_sink_thread(
    cmd_ring: &CommandRing,
    cmd_pipe_read: Fd,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: usize,
    ramp_frames: u32,
) {
    let device_period_samples = device_period_frames * device_channels as usize;
    let period_nanos = (device_period_frames as u64 * 1_000_000_000) / device_sample_rate as u64;

    let mut streams: Vec<ClientStream> = Vec::new();
    let poller = Poller::new(64);
    let mut mix_f32 = vec![0.0f32; device_period_samples];
    // Sized for the highest client rate accepted at stream open, exactly as
    // mix_thread sizes its scratch, so `mix_client` never allocates.
    let max_client_frames = (device_period_frames * MAX_CLIENT_RATE as usize)
        .div_ceil(device_sample_rate as usize);
    let mut decode_buf = vec![0.0f32; max_client_frames * 2];
    let mut convert_buf = vec![0.0f32; max_client_frames * 2];

    const TOKEN_CMD: u64 = u64::MAX - 2;

    let mut stats = MixStats::default();
    let mut next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;
    // The virtual playout grid: the wall-clock instant the next period is due.
    // Meaningful only while streaming; re-anchored to now+one period when the
    // first client of a run connects.
    let mut next_period_ns = syscall::clock_nanos();

    eprintln!("soundd: null sink idle");

    loop {
        let was_streaming = !streams.is_empty();

        signal_clients(&mut streams, ramp_frames);

        // Idle discipline (§5.8): no streams → no timer, no wakes. A connect
        // arrives as a command-pipe byte, the only wake source the null sink
        // has. While streaming, wake at the next grid point.
        let timeout = if streams.is_empty() {
            u64::MAX
        } else {
            next_period_ns.saturating_sub(syscall::clock_nanos()).max(1)
        };

        poller.poll_add_fd(cmd_pipe_read, IORING_POLL_IN, TOKEN_CMD);
        let mut cmd_ready = false;
        poller.wait(1, timeout, |token| match token {
            TOKEN_CMD => cmd_ready = true,
            other => panic!("soundd: unexpected null-sink poll token {other}"),
        });

        if was_streaming {
            stats.wakes += 1;
        }

        if cmd_ready {
            let mut drain = [0u8; 64];
            while matches!(syscall::read_nonblock(cmd_pipe_read, &mut drain), Ok(n) if n == drain.len()) {}
        }
        apply_commands(cmd_ring, &mut streams, ramp_frames);

        // Start the grid when the first client of a run connects, and reset the
        // reporting window so no idle stretch dilutes it.
        if !was_streaming && !streams.is_empty() {
            let now = syscall::clock_nanos();
            next_period_ns = now + period_nanos;
            stats = MixStats::default();
            next_stats_ns = now + STATS_INTERVAL_NANOS;
        }

        // Drain every period the grid says is due, discarding the mix. This is
        // the whole difference from a real device: exactly one period consumed
        // per `period_nanos` of wall clock, so a client's ring drains — and its
        // writes backpressure — at the real audio rate. The batch is capped at
        // the ring depth: a client can be at most `slot_count` periods ahead, so
        // a wake that would drain more than that overslept long enough for the
        // grid to be a dead reference (a loaded CPU, a host suspend). It is
        // re-anchored to now rather than chasing the lost time, which nothing
        // heard it play.
        let mut batch = 0u32;
        while !streams.is_empty() && batch < NULL_SINK_BUFFERS as u32 {
            let now = syscall::clock_nanos();
            if now < next_period_ns {
                break;
            }
            let lateness = now.saturating_sub(next_period_ns);
            stats.max_wake_lat_ns = stats.max_wake_lat_ns.max(lateness);

            mix_f32.fill(0.0);
            let mut any_data = false;
            let mut any_streaming = false;
            for stream in streams.iter_mut() {
                let covered = mix_client(
                    stream,
                    &mut mix_f32,
                    &mut decode_buf,
                    &mut convert_buf,
                    device_channels as usize,
                    device_period_frames,
                );
                // §5.7: a delivered period proves the client holds its signal
                // pipe's read end, so soundd releases its own — the next write
                // then breaks if the client dies. Identical to the device path.
                if covered && !stream.delivered {
                    stream.delivered = true;
                    if let Some(fd) = stream.signal_read_fd.take() {
                        syscall::close(fd);
                    }
                }
                any_data |= covered;
                any_streaming |= stream.is_streaming();
            }
            // The mix is discarded here — no dither, no DMA buffer, no submit.
            stats.submitted += 1;
            stats.completions += 1;
            if any_streaming && !any_data {
                stats.underruns += 1;
            }
            next_period_ns += period_nanos;
            batch += 1;
        }
        if batch == NULL_SINK_BUFFERS as u32 {
            next_period_ns = syscall::clock_nanos() + period_nanos;
        }
        stats.max_batch = stats.max_batch.max(batch);

        retain_active(&mut streams);

        // Reporting: flush on the last disconnect so a short stream's tail is in
        // the record, and every STATS_INTERVAL_NANOS while streaming — the same
        // cadence and format mix_thread uses, so a discarded stream is not
        // silent about being discarded (#106's status tool reads one shape).
        let now_ns = syscall::clock_nanos();
        if was_streaming && streams.is_empty() {
            stats.report(0);
            stats = MixStats::default();
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
            eprintln!("soundd: null sink idle");
        } else if now_ns >= next_stats_ns {
            if !streams.is_empty() {
                stats.report(streams.len());
                stats = MixStats::default();
            }
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
        }
    }
}

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

/// Hand one command to the mix thread, waiting for room if the ring is full.
///
/// The mix thread drains the whole ring at the top of every cycle, so a full
/// ring means it has not run for a cycle and one device period is exactly how
/// long there is to wait. Throttling the control thread is the point: the
/// alternatives are dropping a command (a client stranded in the mix thread
/// forever) or asserting (soundd dead at a client's choosing). The retry is
/// unbounded because the mix thread is the process's main thread — if it has
/// stopped, soundd is already gone.
fn submit(cmd_ring: &CommandRing, cmd_pipe_write: Fd, cmd: MixCommand, period_nanos: u64) {
    let mut cmd = cmd;
    loop {
        let full = cmd_ring.try_push(cmd);
        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
        match full {
            Ok(()) => return,
            Err(returned) => {
                cmd = returned;
                syscall::nanosleep(period_nanos);
            }
        }
    }
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
    // One handle per client plus the listener; `MAX_CONTROL_CLIENTS` is derived
    // from this ring, so the set always fits in one batch.
    let poller = Poller::new(MAX_CONTROL_CLIENTS as u32 + 1);
    let period_nanos = (device_period_frames as u64 * 1_000_000_000) / device_sample_rate as u64;

    struct ControlClient {
        conn: Connection,
        msg: MsgBuf,
        // Set once MSG_STREAM_OPEN succeeds; accepted-but-silent connections
        // stay pending so they cannot stall the control plane.
        stream_idx: Option<usize>,
        /// Latest volume this client asked for, not yet handed to the mix
        /// thread. Volume is state, not an event: `set_target` overwrites the
        /// ramp outright, so applying N of them within one mix cycle leaves
        /// exactly what applying only the last would. Collapsing is therefore
        /// lossless, and it keeps a client's message rate off the command ring
        /// — the drain loop below reads everything a client has written before
        /// it yields.
        pending_volume: Option<Gain>,
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
                // Refused rather than left queued: a connection past the
                // poller's watchable set would never be read from. It is still
                // accepted first — leaving it in the listener queue keeps the
                // listener readable and spins this loop.
                Ok(accepted) if clients.len() >= MAX_CONTROL_CLIENTS => {
                    eprintln!("soundd: refusing connection, {MAX_CONTROL_CLIENTS} clients already connected");
                    let _ = accepted.conn.signal(MSG_STREAM_ERROR);
                }
                Ok(accepted) => {
                    clients.push(ControlClient {
                        conn: accepted.conn,
                        msg: MsgBuf::new(),
                        stream_idx: None,
                        pending_volume: None,
                    });
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
            let mut disconnected = false;
            'msgs: loop {
                let c = &mut clients[i];
                let (msg_type, plen, payload) = match c.msg.recv(&c.conn) {
                    Ok(Some(m)) => m,
                    Ok(None) => break 'msgs,
                    Err(()) => {
                        if let Some(idx) = c.stream_idx {
                            submit(cmd_ring, cmd_pipe_write, MixCommand::RemoveClient(idx), period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
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
                            disconnected = true;
                            break 'msgs;
                        }
                        eprintln!("soundd: opening stream: {}Hz {}ch fmt={}",
                            req.sample_rate, req.channels, req.format);
                        let idx = next_idx;
                        next_idx += 1;
                        let Some(client) = open_stream(
                            idx,
                            client_pids[i],
                            &req,
                            &clients[i].conn,
                            device_sample_rate,
                            device_channels,
                            device_period_frames,
                            slot_count,
                            ramp_frames,
                        ) else {
                            let _ = clients[i].conn.signal(MSG_STREAM_ERROR);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        };
                        submit(cmd_ring, cmd_pipe_write, MixCommand::AddClient(Box::new(client)), period_nanos);
                        clients[i].stream_idx = Some(idx);
                    }
                    (MSG_STREAM_SET_VOLUME, Some(idx)) if plen == core::mem::size_of::<StreamSetVolume>() => {
                        let raw = f32::from_le_bytes(payload[0..4].try_into().unwrap());
                        let Some(gain) = Gain::from_wire(raw) else {
                            eprintln!("soundd: volume is not a number, disconnecting client");
                            submit(cmd_ring, cmd_pipe_write, MixCommand::RemoveClient(idx), period_nanos);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        };
                        clients[i].pending_volume = Some(gain);
                    }
                    (MSG_STREAM_CLOSE, Some(idx)) => {
                        submit(cmd_ring, cmd_pipe_write, MixCommand::RemoveClient(idx), period_nanos);
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                    (other, _) => {
                        eprintln!("soundd: protocol violation (msg {other}), disconnecting client");
                        if let Some(idx) = clients[i].stream_idx {
                            submit(cmd_ring, cmd_pipe_write, MixCommand::RemoveClient(idx), period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                }
            }
            // One command for however many volume messages that drain carried.
            // A disconnecting client is skipped: its `RemoveClient` is already
            // queued, and a `SetVolume` behind it would aim the ramp at the
            // volume instead of at silence and strand the stream unremoved.
            if !disconnected {
                if let (Some(client_id), Some(target)) = (clients[i].stream_idx, clients[i].pending_volume.take()) {
                    submit(cmd_ring, cmd_pipe_write, MixCommand::SetVolume { client_id, target }, period_nanos);
                }
            }
        }
        for &i in dead.iter().rev() {
            clients.remove(i);
            client_pids.remove(i);
        }
    }
}

/// The virtual output soundd presents when the machine has no audio hardware.
/// These match the one configuration cpal's ToyOS backend advertises
/// (`src/host/toyos/mod.rs`): 44100 Hz stereo i16, 128 frames per period. A
/// stream negotiates against them exactly as it would a real device, so a
/// no-hardware machine is invisible to the client.
const NULL_SINK_RATE: u32 = 44_100;
const NULL_SINK_CHANNELS: u16 = 2;
const NULL_SINK_PERIOD_FRAMES: usize = 128;
/// Same DMA-pipeline depth as virtio-sound (`TX_INFLIGHT_MAX`): the client ring
/// is as deep, so a client may fill `NULL_SINK_BUFFERS - 1` periods ahead and
/// its backpressure is the device's. Power of two — ring indices wrap mod 2^32.
const NULL_SINK_BUFFERS: usize = 8;

fn main() {
    let listener = services::listen("soundd").expect("soundd already running");

    // A machine with no sound card is a routing state, not a bug: soundd
    // presents a virtual output and discards what is played to it, so a client
    // building an audio stream succeeds whether or not hardware is present.
    // Exiting was the old behavior — it released the name and left every audio
    // client's connect failing NotFound, which crashed uncontrolled programs
    // like `tone`. Hardware absence is a route, and the route when there is no
    // sink is the null sink.
    //
    // NotFound and nothing else means "no device". `services::listen` is not the
    // whole "already running" check: it releases the name and the audio claim
    // under different locks, so a soundd restarted the instant the previous one
    // exits can pass it and still lose the claim. That is a conflict, not an
    // absent sound card, and it has to stay loud.
    // The order is virtio first, and it is not a preference between two cards:
    // no machine in this project has both, and taking the kernel-driven one
    // first means a machine that has always had audio keeps exactly the path it
    // had. The T14 has only the second.
    match AudioDev::open() {
        Ok(dev) => return run_virtio(listener, dev),
        Err(syscall::SyscallError::NotFound) => {}
        Err(e) => panic!("soundd: cannot claim the audio device: {e}"),
    }
    match hda::Hda::claim() {
        Ok((hda, _path, channels)) => run_hda(listener, hda, channels),
        Err(hda::Refusal::NoDevice(syscall::SyscallError::NotFound)) => run_null_sink(listener),
        Err(why) => {
            eprintln!("soundd: the HDA controller cannot carry audio: {why}");
            run_null_sink(listener)
        }
    }
}

fn run_virtio(listener: toyos::Listener, audio_dev: AudioDev) {
    let info: AudioInfo = audio_dev.info().expect("soundd: failed to read audio info");

    let num_buffers = info.num_buffers as usize;
    let device_period_bytes = info.period_bytes as usize;
    let dma_page = SharedMemory::map(info.dma_token, 2 * 1024 * 1024)
        .expect("the DMA token the audio device just reported");
    let dma_base = dma_page.as_ptr();
    let buffers = (0..num_buffers)
        .map(|i| unsafe { dma_base.add(info.buf_offsets[i] as usize) })
        .collect();

    run_with_device(
        listener,
        &mut VirtioBackend { dev: audio_dev, buffers },
        num_buffers,
        info.sample_rate,
        info.channels as u16,
        device_period_bytes,
    );
}

fn run_hda(listener: toyos::Listener, hda: hda::Hda, channels: u8) {
    let info = hda.info();
    let num_buffers = info.periods as usize;
    let period_bytes = info.period_bytes as usize;
    let ring = SharedMemory::map(info.pcm_token, 2 * 1024 * 1024)
        .expect("the PCM token the HDA device just reported");
    // One region, `periods` buffers end to end: the buffer descriptor list the
    // kernel built points at exactly these offsets.
    let base = ring.as_ptr();
    let buffers = (0..num_buffers).map(|i| unsafe { base.add(i * period_bytes) }).collect();

    run_with_device(
        listener,
        &mut HdaBackend { hda, buffers, period_bytes },
        num_buffers,
        toyos_hda::config::RATE,
        channels as u16,
        period_bytes,
    );
}

fn run_with_device(
    listener: toyos::Listener,
    backend: &mut dyn Backend,
    num_buffers: usize,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_bytes: usize,
) {
    let device_period_frames = device_period_bytes / (device_channels as usize * 2);

    assert!(device_channels == 1 || device_channels == 2,
        "soundd: unsupported device channel count {device_channels}");
    // Ring indices wrap mod 2^32, so slot_count must divide it evenly.
    assert!(num_buffers.is_power_of_two(), "soundd: num_buffers must be a power of two");

    // Client ring depth matches the DMA pipeline depth: a wake gap can free
    // at most num_buffers periods, so a full client ring always covers it.
    let slot_count = num_buffers as u32;

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
        backend,
        &cmd_ring,
        cmd_pipe.read,
        num_buffers,
        device_sample_rate,
        device_channels,
        device_period_bytes,
        device_period_frames,
        ramp_frames,
    );
}

fn run_null_sink(listener: toyos::Listener) {
    let device_sample_rate = NULL_SINK_RATE;
    let device_channels = NULL_SINK_CHANNELS;
    let device_period_frames = NULL_SINK_PERIOD_FRAMES;
    let slot_count = NULL_SINK_BUFFERS as u32;

    // ~5ms connect/disconnect/volume ramp, same as the device path.
    let ramp_frames = device_sample_rate * 5 / 1000;

    eprintln!(
        "soundd: no audio device, presenting a null sink ({}Hz {}ch, {} frames/period, streams discarded)",
        device_sample_rate, device_channels, device_period_frames
    );

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

    null_sink_thread(
        &cmd_ring,
        cmd_pipe.read,
        device_sample_rate,
        device_channels,
        device_period_frames,
        ramp_frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
