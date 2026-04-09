use toyos_abi::audio::AudioInfo;
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos_abi::Fd;
use toyos::audio::{AudioOpenRequest, AudioOpenResponse, AudioRingReader, AudioSetVolume};
use toyos::audio::{MSG_AUDIO_DATA_READY, MSG_AUDIO_OPEN, MSG_AUDIO_OPENED, MSG_AUDIO_SET_VOLUME};
use toyos::services;
use toyos::shm::SharedMemory;
use toyos::{AudioDev, Connection};

const AUDIO_RING_SIZE: usize = 16384;
const DEFAULT_VOLUME: i32 = 256;

struct ClientStream {
    ring: AudioRingReader,
    control: Connection,
    volume: i32,
}

impl ClientStream {
    /// Read up to `max_samples` i16 samples from the ring, mix into `mix` at `offset`.
    /// Returns the number of samples actually read.
    fn mix_into(&self, mix: &mut [i32], offset: usize, max_samples: usize) -> usize {
        let need = max_samples * 2;
        let avail = self.ring.available() as usize;
        let to_read = need.min(avail) & !1;
        if to_read == 0 {
            return 0;
        }
        let mut raw = [0u8; 8192];
        let n = self.ring.read(&mut raw[..to_read]);
        let samples = n / 2;
        let vol = self.volume;
        for i in 0..samples {
            let sample = i16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]) as i32;
            mix[offset + i] = mix[offset + i].saturating_add(sample * vol / 256);
        }
        samples
    }

    fn is_dead(&self) -> bool {
        self.ring.is_writer_closed() && self.ring.available() == 0
    }
}

fn open_stream(control: Connection, client_pid: u32, _req: &AudioOpenRequest, next_id: &mut u32) -> ClientStream {
    let shm = SharedMemory::allocate(AUDIO_RING_SIZE);
    shm.grant(client_pid);

    let id = *next_id;
    *next_id += 1;

    let _ = control.send(MSG_AUDIO_OPENED, &AudioOpenResponse {
        stream_id: id,
        shm_token: shm.token(),
        ring_size: AUDIO_RING_SIZE as u32,
    });

    let ring = AudioRingReader::new(shm);
    ClientStream { ring, control, volume: DEFAULT_VOLUME }
}

fn main() {
    let listener = services::listen("soundd").expect("soundd already running");

    let audio_dev = AudioDev::open().expect("soundd: no audio device");
    let info: AudioInfo = audio_dev.info().expect("soundd: failed to read audio info");

    let num_buffers = info.num_buffers as usize;
    let dma_page = SharedMemory::map(info.dma_token, 2 * 1024 * 1024);
    let dma_base = dma_page.as_ptr();
    let mut dma_ptrs: Vec<*mut u8> = Vec::with_capacity(num_buffers);
    for i in 0..num_buffers {
        dma_ptrs.push(unsafe { dma_base.add(info.buf_offsets[i] as usize) });
    }

    let period_frames = info.period_bytes as usize / 4;
    let period_samples = period_frames * info.channels as usize;

    eprintln!("soundd: ready, {} buffers, {}Hz, {} bytes/period",
        num_buffers, info.sample_rate, info.period_bytes);

    let mut streams: Vec<ClientStream> = Vec::new();
    let mut next_stream_id: u32 = 1;
    let mut free_mask: u32 = (1u32 << num_buffers) - 1;

    // Mix accumulator: collects samples from client rings until a full DMA
    // period is ready. This decouples client audio tic rate from DMA period
    // size — partial tics accumulate across periods instead of creating
    // silence gaps.
    let mut accum = vec![0i32; period_samples];
    let mut accum_filled: usize = 0;

    let poller = Poller::new(64);

    const TOKEN_LISTENER: u64 = u64::MAX;
    const TOKEN_AUDIO: u64 = u64::MAX - 1;

    loop {
        // 1. Register poll interests
        poller.poll_add(&audio_dev, IORING_POLL_IN, TOKEN_AUDIO);
        poller.poll_add(&listener, IORING_POLL_IN, TOKEN_LISTENER);
        for (i, stream) in streams.iter().enumerate() {
            poller.poll_add(&stream.control, IORING_POLL_IN, i as u64);
        }

        // 2. Wait for events — DMA completions arrive as interrupts via
        // TOKEN_AUDIO. Client ring data arrives via shared memory with no
        // kernel notification, so we use a short timeout to poll rings.
        let timeout = if streams.is_empty() { 100_000_000 } else { u64::MAX };
        let mut ready_tokens: Vec<u64> = Vec::new();
        poller.wait(1, timeout, |token| ready_tokens.push(token));

        // 3. Handle DMA completions via fd read
        if ready_tokens.contains(&TOKEN_AUDIO) {
            if let Ok(mask) = audio_dev.read_completions() {
                free_mask |= mask;
            }
        }

        // 4. Drain client rings into accumulator
        if !streams.is_empty() && accum_filled < period_samples {
            let want = period_samples - accum_filled;
            let mut got = 0usize;
            for stream in &streams {
                let n = stream.mix_into(&mut accum, accum_filled, want);
                got = got.max(n);
            }
            accum_filled += got;
        }

        // 5. Submit DMA buffers when accumulator has a full period
        while free_mask != 0 {
            let idx = free_mask.trailing_zeros() as usize;
            if idx >= num_buffers { break; }

            if !streams.is_empty() && accum_filled < period_samples {
                break;
            }

            free_mask &= !(1 << idx);

            let out = unsafe {
                core::slice::from_raw_parts_mut(dma_ptrs[idx] as *mut i16, period_samples)
            };

            if streams.is_empty() {
                for s in out.iter_mut() { *s = 0; }
            } else {
                for (i, &s) in accum.iter().enumerate() {
                    out[i] = s.clamp(-32768, 32767) as i16;
                }
                for s in accum.iter_mut() { *s = 0; }
                accum_filled = 0;
            }

            toyos::audio::audio_submit(idx as u32, info.period_bytes);
        }

        // 6. Clean up dead streams
        streams.retain(|s| !s.is_dead());

        // 7. Handle connections and control messages
        if ready_tokens.contains(&TOKEN_LISTENER) {
            let conn = services::accept(&listener).expect("accept failed");
            let Ok(header) = conn.conn.recv_header() else {
                continue;
            };
            match header.msg_type {
                MSG_AUDIO_OPEN => {
                    let req: AudioOpenRequest = conn.conn.recv_payload(&header).unwrap();
                    let stream = open_stream(conn.conn, conn.client_pid, &req, &mut next_stream_id);
                    streams.push(stream);
                }
                other => eprintln!("soundd: unexpected message type {other}"),
            }
        }

        let mut dead_controls: Vec<Fd> = Vec::new();
        for i in 0..streams.len() {
            if ready_tokens.contains(&(i as u64)) {
                let fd = streams[i].control.fd();
                let Ok(header) = streams[i].control.recv_header() else {
                    dead_controls.push(fd);
                    continue;
                };
                match header.msg_type {
                    MSG_AUDIO_SET_VOLUME => {
                        let cmd: AudioSetVolume = streams[i].control.recv_payload(&header).unwrap();
                        streams[i].volume = (cmd.volume as i32).min(512);
                    }
                    MSG_AUDIO_DATA_READY => {}
                    _ => {}
                }
            }
        }
        if !dead_controls.is_empty() {
            streams.retain(|s| !dead_controls.contains(&s.control.fd()));
        }
    }
}
