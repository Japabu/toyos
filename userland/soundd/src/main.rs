use toyos_abi::audio::{AudioInfo, AudioSlotHeader};
use toyos_abi::Fd;
use toyos::audio::{
    AudioSlotReader, StreamOpenRequest, StreamOpenResponse, StreamSetVolume,
    MSG_STREAM_OPEN, MSG_STREAM_OPENED, MSG_STREAM_SET_VOLUME, MSG_STREAM_CLOSE,
};
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::services;
use toyos::shm::SharedMemory;
use toyos::{AudioDev, Connection};
use toyos_abi::syscall;

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rubato::{SincFixedOut, SincInterpolationParameters, SincInterpolationType, VecResampler, WindowFunction};

// ---------------------------------------------------------------------------
// Per-client state
// ---------------------------------------------------------------------------

struct ClientResampler {
    resampler: SincFixedOut<f32>,
    input_buf: Vec<Vec<f32>>,
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

    fn set_target(&mut self, target: f32, ramp_samples: u32) {
        self.target = target;
        self.step = (target - self.current) / ramp_samples as f32;
        self.remaining = ramp_samples;
    }

    fn next(&mut self) -> f32 {
        if self.remaining > 0 {
            self.current += self.step;
            self.remaining -= 1;
            if self.remaining == 0 { self.current = self.target; }
        }
        self.current
    }

    fn is_idle(&self) -> bool { self.remaining == 0 }
}

struct ClientStream {
    client_id: usize,
    slot_reader: AudioSlotReader,
    signal_write_fd: Fd,
    signal_read_fd: Fd,
    gain: GainRamp,
    client_channels: u16,
    client_format: u16,
    client_period_frames: u32,
    resampler: Option<ClientResampler>,
    pending_removal: bool,
    consecutive_underruns: u32,
}

// ---------------------------------------------------------------------------
// Lock-free SPSC command queue (control thread → mix thread)
// ---------------------------------------------------------------------------

const CMD_RING_SIZE: u32 = 16;

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

    fn push(&self, cmd: MixCommand) -> bool {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= CMD_RING_SIZE { return false; }
        let idx = (w % CMD_RING_SIZE) as usize;
        unsafe { (*self.slots.get())[idx] = Some(cmd); }
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
        true
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

fn dither_and_quantize(sample: f32, rng: &mut Xorshift32) -> i16 {
    let dither = rng.next() + rng.next(); // triangular PDF in [-1.0, 1.0]
    (sample * 32767.0 + dither).clamp(-32768.0, 32767.0) as i16
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

    fn update(&mut self, t_actual: f64) {
        match self.t_estimated {
            None => {
                self.t_estimated = Some(t_actual + self.period);
            }
            Some(t_est) => {
                let error = t_actual - t_est;
                let new_t = t_est + self.period + self.bw * error;
                self.period += self.bw * self.bw * error;
                // Clamp period to [50%, 200%] of nominal to prevent collapse
                let min = self.nominal_period * 0.5;
                let max = self.nominal_period * 2.0;
                self.period = self.period.clamp(min, max);
                self.t_estimated = Some(new_t);
            }
        }
    }

    fn next_wake_nanos(&self) -> Option<f64> {
        self.t_estimated
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
    ramp_samples: u32,
) -> ClientStream {
    let client_period_frames = if req.sample_rate != device_sample_rate {
        ((device_period_frames as u64 * req.sample_rate as u64 + device_sample_rate as u64 - 1)
            / device_sample_rate as u64) as u32
    } else {
        device_period_frames
    };

    let sample_size: u32 = match req.format {
        0 => 2, // S16LE
        _ => 2,
    };
    let client_frame_size = req.channels as u32 * sample_size;
    let client_period_bytes = client_period_frames * client_frame_size;

    const SLOT_COUNT: u32 = 4;
    let shm_size = AudioSlotHeader::SIZE as u32 + SLOT_COUNT * client_period_bytes;
    let shm = SharedMemory::allocate(shm_size as usize);
    shm.grant(client_pid);
    let shm_token = shm.token();

    // Initialize the slot ring header
    unsafe {
        let hdr = &*(shm.as_ptr() as *const AudioSlotHeader);
        hdr.write_idx.store(0, core::sync::atomic::Ordering::Relaxed);
        hdr.read_idx.store(0, core::sync::atomic::Ordering::Relaxed);
    }

    // Create signal pipe
    let pipe_fds = syscall::pipe();
    let signal_pipe_id = syscall::pipe_id(pipe_fds.read).expect("pipe_id failed");

    let slot_reader = AudioSlotReader::new(shm, client_period_bytes, SLOT_COUNT);

    let _ = control.send(MSG_STREAM_OPENED, &StreamOpenResponse {
        shm_token,
        signal_pipe_id,
        client_period_frames,
        client_period_bytes,
        device_sample_rate,
        device_channels,
        slot_count: SLOT_COUNT as u16,
    });

    // Set up resampler if needed
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
        let input_buf = vec![Vec::new(); device_channels as usize];
        Some(ClientResampler { resampler, input_buf })
    } else {
        None
    };

    let mut gain = GainRamp::new(0.0);
    gain.set_target(1.0, ramp_samples);

    ClientStream {
        client_id,
        slot_reader,
        signal_write_fd: pipe_fds.write,
        signal_read_fd: pipe_fds.read,
        gain,
        client_channels: req.channels,
        client_format: req.format,
        client_period_frames,
        resampler,
        pending_removal: false,
        consecutive_underruns: 0,
    }
}

// ---------------------------------------------------------------------------
// Mix thread
// ---------------------------------------------------------------------------

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
    ramp_samples: u32,
) {
    let device_period_samples = device_period_frames * device_channels as usize;
    let period_nanos = (device_period_frames as u64 * 1_000_000_000) / device_sample_rate as u64;

    let mut streams: Vec<ClientStream> = Vec::new();
    let mut free_mask: u32 = 0;

    // Prime the DMA pipeline with silence
    for i in 0..num_buffers {
        let buf = unsafe { core::slice::from_raw_parts_mut(dma_ptrs[i] as *mut u8, device_period_bytes) };
        buf.fill(0);
        toyos::audio::audio_submit(i as u32, device_period_bytes as u32);
    }

    syscall::set_rt_priority(true);

    let poller = Poller::new(64);
    let mut mix_f32 = vec![0.0f32; device_period_samples];
    let mut decode_buf = vec![0.0f32; 4096];
    let mut convert_buf = vec![0.0f32; 4096];
    let mut dither_rng = Xorshift32((syscall::clock_nanos() as u32) | 1);
    let mut dll = Dll::new(period_nanos as f64);

    const TOKEN_AUDIO: u64 = u64::MAX - 1;
    const TOKEN_CMD: u64 = u64::MAX - 2;

    let mut stat_wakes: u32 = 0;
    let mut stat_completions: u32 = 0;
    let mut stat_submitted: u32 = 0;
    let mut stat_underruns: u32 = 0;

    loop {
        // Signal all clients BEFORE the io_uring wait. Priority inheritance
        // boosts them to RT; they fill their ring slots while soundd is blocked
        // in the poller wait below.
        for stream in streams.iter() {
            let _ = syscall::write_nonblock(stream.signal_write_fd, &[1]);
        }

        let timeout = if streams.is_empty() {
            u64::MAX
        } else {
            match dll.next_wake_nanos() {
                None => period_nanos,
                Some(t) => {
                    let now = syscall::clock_nanos() as f64;
                    let delta = (t - now).max(0.0) as u64;
                    if delta == 0 { period_nanos } else { delta }
                }
            }
        };

        poller.poll_add(&audio_dev, IORING_POLL_IN, TOKEN_AUDIO);
        poller.poll_add_fd(cmd_pipe_read, IORING_POLL_IN, TOKEN_CMD);

        let mut ready_tokens: Vec<u64> = Vec::new();
        poller.wait(1, timeout, |token| ready_tokens.push(token));

        if !streams.is_empty() {
            stat_wakes += 1;
        }

        // Drain command queue
        if ready_tokens.contains(&TOKEN_CMD) {
            let mut drain = [0u8; 64];
            let _ = syscall::read_nonblock(cmd_pipe_read, &mut drain);
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
                        s.gain.set_target(0.0, ramp_samples);
                        s.pending_removal = true;
                    }
                }
                MixCommand::SetVolume { client_id, target } => {
                    if let Some(s) = streams.iter_mut().find(|s| s.client_id == client_id) {
                        s.gain.set_target(target, ramp_samples);
                    }
                }
            }
        }

        // Handle DMA completions
        match audio_dev.read_completions() {
            Ok((mask, ts)) => {
                if !streams.is_empty() { stat_completions += mask.count_ones(); }
                free_mask |= mask;
                if ts != 0 { dll.update(ts as f64); }
            }
            Err(toyos_abi::syscall::SyscallError::WouldBlock) => {}
            Err(e) => panic!("soundd: read_completions failed: {e:?}"),
        }

        // Mix and submit for each free DMA buffer
        while free_mask != 0 {
            let idx = free_mask.trailing_zeros() as usize;
            if idx >= num_buffers { break; }
            free_mask &= !(1 << idx);

            for s in mix_f32.iter_mut() { *s = 0.0; }

            let mut any_data = false;
            for stream in streams.iter_mut() {
                let Some(client_data) = stream.slot_reader.try_consume() else {
                    stream.consecutive_underruns += 1;
                    // Dead client detection: ~1 second of continuous underruns
                    if stream.consecutive_underruns > 344 && !stream.pending_removal {
                        eprintln!("soundd: client {} timed out (no data)", stream.client_id);
                        stream.pending_removal = true;
                    }
                    continue;
                };
                stream.consecutive_underruns = 0;
                any_data = true;

                let client_frames = stream.client_period_frames as usize;
                let client_channels = stream.client_channels as usize;
                let client_samples = client_frames * client_channels;

                // 1. Decode to f32
                if decode_buf.len() < client_samples {
                    decode_buf.resize(client_samples, 0.0);
                }
                let decoded = &mut decode_buf[..client_samples];
                match stream.client_format {
                    0 => decode_i16_to_f32(client_data, decoded),
                    _ => decode_i16_to_f32(client_data, decoded),
                }

                // 2. Channel convert if needed
                let (working_buf, working_channels, working_frames) =
                    if client_channels != device_channels as usize {
                        let out_samples = client_frames * device_channels as usize;
                        if convert_buf.len() < out_samples {
                            convert_buf.resize(out_samples, 0.0);
                        }
                        let converted = &mut convert_buf[..out_samples];
                        if client_channels == 1 && device_channels == 2 {
                            channel_convert_mono_to_stereo(decoded, converted);
                        } else if client_channels == 2 && device_channels == 1 {
                            channel_convert_stereo_to_mono(decoded, converted);
                        } else {
                            panic!("soundd: unsupported channel conversion {}→{}", client_channels, device_channels);
                        }
                        (&convert_buf[..out_samples], device_channels as usize, client_frames)
                    } else {
                        (&decode_buf[..client_samples], client_channels, client_frames)
                    };

                // 3. Resample if needed
                let final_samples: &[f32] = if let Some(ref mut rs) = stream.resampler {
                    for ch in 0..working_channels {
                        rs.input_buf[ch].clear();
                        for frame in 0..working_frames {
                            rs.input_buf[ch].push(working_buf[frame * working_channels + ch]);
                        }
                    }

                    let resampled = rs.resampler.process(&rs.input_buf, None)
                        .expect("resampler failed");

                    let out_frames = resampled[0].len();
                    let out_samples = out_frames * working_channels;
                    if convert_buf.len() < out_samples {
                        convert_buf.resize(out_samples, 0.0);
                    }
                    for frame in 0..out_frames {
                        for ch in 0..working_channels {
                            convert_buf[frame * working_channels + ch] = resampled[ch][frame];
                        }
                    }
                    &convert_buf[..out_samples]
                } else {
                    working_buf
                };

                // 4. Apply per-sample gain ramp and accumulate
                let mix_len = mix_f32.len().min(final_samples.len());
                for i in 0..mix_len {
                    mix_f32[i] += final_samples[i] * stream.gain.next();
                }
            }

            // TPDF dither + quantize f32 → i16 into DMA buffer
            let dma_buf = unsafe {
                core::slice::from_raw_parts_mut(dma_ptrs[idx] as *mut i16, device_period_samples)
            };
            for i in 0..device_period_samples {
                dma_buf[i] = dither_and_quantize(mix_f32[i], &mut dither_rng);
            }

            toyos::audio::audio_submit(idx as u32, device_period_bytes as u32);
            stat_submitted += 1;
            if !any_data && !streams.is_empty() { stat_underruns += 1; }
        }

        // Remove clients that finished ramp-down or have no data to fade
        streams.retain(|s| {
            if s.pending_removal && (s.gain.is_idle() || s.consecutive_underruns > 0) {
                eprintln!("soundd: client {} removed", s.client_id);
                syscall::close(s.signal_write_fd);
                syscall::close(s.signal_read_fd);
                false
            } else {
                true
            }
        });

        if stat_wakes > 0 && stat_wakes % 344 == 0 {
            eprintln!("soundd: wakes={} completions={} submitted={} underruns={} clients={}",
                stat_wakes, stat_completions, stat_submitted, stat_underruns, streams.len());
        }
    }
}

// ---------------------------------------------------------------------------
// Control thread
// ---------------------------------------------------------------------------

fn control_thread(
    listener: toyos::Listener,
    cmd_ring: &CommandRing,
    cmd_pipe_write: Fd,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: u32,
    ramp_samples: u32,
) {
    let poller = Poller::new(32);

    struct ControlClient {
        conn: Connection,
        idx: usize,
    }

    let mut clients: Vec<ControlClient> = Vec::new();
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
                    let Ok(header) = accepted.conn.recv_header() else { continue };
                    match header.msg_type {
                        MSG_STREAM_OPEN => {
                            let req: StreamOpenRequest = accepted.conn.recv_payload(&header).unwrap();
                            eprintln!("soundd: opening stream: {}Hz {}ch fmt={}",
                                req.sample_rate, req.channels, req.format);
                            let idx = next_idx;
                            next_idx += 1;
                            let client = open_stream(
                                idx,
                                accepted.client_pid,
                                &req,
                                &accepted.conn,
                                device_sample_rate,
                                device_channels,
                                device_period_frames,
                                ramp_samples,
                            );
                            cmd_ring.push(MixCommand::AddClient(Box::new(client)));
                            let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                            clients.push(ControlClient { conn: accepted.conn, idx });
                        }
                        other => eprintln!("soundd: unexpected message type {other}"),
                    }
                }
                Err(e) => eprintln!("soundd: accept failed: {e:?}"),
            }
        }

        let mut dead: Vec<usize> = Vec::new();
        for i in 0..clients.len() {
            if ready.contains(&(i as u64)) {
                let Ok(header) = clients[i].conn.recv_header() else {
                    cmd_ring.push(MixCommand::RemoveClient(clients[i].idx));
                    let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                    dead.push(i);
                    continue;
                };
                match header.msg_type {
                    MSG_STREAM_SET_VOLUME => {
                        let cmd: StreamSetVolume = clients[i].conn.recv_payload(&header).unwrap();
                        cmd_ring.push(MixCommand::SetVolume {
                            client_id: clients[i].idx,
                            target: cmd.gain.clamp(0.0, 1.0),
                        });
                        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                    }
                    MSG_STREAM_CLOSE => {
                        cmd_ring.push(MixCommand::RemoveClient(clients[i].idx));
                        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
                        dead.push(i);
                    }
                    _ => {}
                }
            }
        }
        for &i in dead.iter().rev() {
            clients.remove(i);
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

    let dma_page = SharedMemory::map(info.dma_token, 2 * 1024 * 1024);
    let dma_base = dma_page.as_ptr();
    let mut dma_ptrs: Vec<*mut u8> = Vec::with_capacity(num_buffers);
    for i in 0..num_buffers {
        dma_ptrs.push(unsafe { dma_base.add(info.buf_offsets[i] as usize) });
    }

    audio_dev.start().expect("soundd: failed to start audio");

    // ~5ms ramp at device sample rate
    let ramp_samples = device_sample_rate * 5 / 1000;

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
                ramp_samples,
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
        ramp_samples,
    );
}
