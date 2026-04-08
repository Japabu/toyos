use toyos_abi::audio::AudioInfo;
use toyos_abi::io_uring;
use toyos_abi::ring::RingHeader;
use toyos_abi::syscall;
use toyos_abi::Fd;
use toyos::audio::{AudioOpenRequest, AudioOpenResponse, AudioSetVolume};
use toyos::audio::{MSG_AUDIO_OPEN, MSG_AUDIO_OPENED, MSG_AUDIO_SET_VOLUME};
use toyos::device;
use toyos::services;
use toyos::shm::SharedMemory;
use toyos::{Connection, Device};

const AUDIO_RING_SIZE: usize = 16384;
const DEFAULT_VOLUME: i32 = 256;

struct AudioStream {
    shm: SharedMemory,
    control: Connection,
    volume: i32,
}

impl AudioStream {
    fn ring(&self) -> &RingHeader {
        unsafe { &*(self.shm.as_ptr() as *const RingHeader) }
    }

    /// Read up to `max_samples` i16 samples from the ring, mix into `mix` at `offset`.
    /// Returns the number of samples actually read.
    fn mix_into(&self, mix: &mut [i32], offset: usize, max_samples: usize) -> usize {
        let ring = self.ring();
        let need = max_samples * 2;
        let avail = ring.available() as usize;
        let to_read = need.min(avail) & !1;
        if to_read == 0 {
            return 0;
        }
        let mut raw = [0u8; 8192];
        let n = ring.read(&mut raw[..to_read]);
        let samples = n / 2;
        let vol = self.volume;
        for i in 0..samples {
            let sample = i16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]) as i32;
            mix[offset + i] = mix[offset + i].saturating_add(sample * vol / 256);
        }
        samples
    }

    fn is_dead(&self) -> bool {
        let ring = self.ring();
        ring.is_writer_closed() && ring.available() == 0
    }
}

fn read_struct<T: Copy>(dev: &Device) -> T {
    let mut buf = [0u8; 256];
    let size = core::mem::size_of::<T>();
    assert!(size <= buf.len());
    let n = dev.read(&mut buf[..size]).expect("read device info failed");
    assert_eq!(n, size);
    unsafe { core::ptr::read(buf.as_ptr() as *const T) }
}

fn open_stream(control: Connection, client_pid: u32, _req: &AudioOpenRequest, next_id: &mut u32) -> AudioStream {
    let mut shm = SharedMemory::allocate(AUDIO_RING_SIZE);
    RingHeader::init(shm.as_mut_slice().as_mut_ptr(), AUDIO_RING_SIZE);
    shm.grant(client_pid);

    let id = *next_id;
    *next_id += 1;

    let _ = control.send(MSG_AUDIO_OPENED, &AudioOpenResponse {
        stream_id: id,
        shm_token: shm.token(),
        ring_size: AUDIO_RING_SIZE as u32,
    });

    AudioStream { shm, control, volume: DEFAULT_VOLUME }
}

fn main() {
    let listener = services::listen("soundd").expect("soundd already running");

    let audio_dev = device::open_audio().expect("soundd: no audio device");
    let info: AudioInfo = read_struct(&audio_dev);
    drop(audio_dev);

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

    let mut streams: Vec<AudioStream> = Vec::new();
    let mut next_stream_id: u32 = 1;
    let mut free_mask: u32 = (1u32 << num_buffers) - 1;

    // Mix accumulator: collects samples from client rings until a full DMA
    // period is ready. This decouples client audio tic rate from DMA period
    // size — partial tics accumulate across periods instead of creating
    // silence gaps.
    let mut accum = vec![0i32; period_samples];
    let mut accum_filled: usize = 0;

    loop {
        // 1. Reclaim completed DMA buffers
        free_mask |= toyos::audio::audio_poll();

        // 2. Drain client rings into accumulator
        if !streams.is_empty() && accum_filled < period_samples {
            let want = period_samples - accum_filled;
            let mut got = 0usize;
            for stream in &streams {
                let n = stream.mix_into(&mut accum, accum_filled, want);
                got = got.max(n);
            }
            accum_filled += got;
        }

        // 3. Submit DMA buffers when accumulator has a full period
        while free_mask != 0 {
            let idx = free_mask.trailing_zeros() as usize;
            if idx >= num_buffers { break; }

            if !streams.is_empty() && accum_filled < period_samples {
                break; // wait for more audio data
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

        // 4. Clean up dead streams
        streams.retain(|s| !s.is_dead());

        // 5. Check for connections and control messages
        let mut poll_fds: Vec<u64> = Vec::with_capacity(1 + streams.len());
        poll_fds.push(listener.fd().0 as u64);
        for stream in &streams {
            poll_fds.push(stream.control.fd().0 as u64);
        }

        // When idle (no streams), block for up to 100ms waiting for connections.
        // When active, poll with 1ms timeout — fast enough for ~23ms DMA periods
        // while avoiding busy-wait CPU burn.
        let timeout = if streams.is_empty() { 100_000_000 } else { 1_000_000 };
        let result = io_uring::poll_fds(&poll_fds, Some(timeout));

        if result.fd(0) {
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
            if result.fd(1 + i) {
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
                    _ => {}
                }
            }
        }
        if !dead_controls.is_empty() {
            streams.retain(|s| !dead_controls.contains(&s.control.fd()));
        }

        // Yield when all DMA buffers are in-flight
        if !streams.is_empty() && free_mask == 0 {
            syscall::nanosleep(1_000_000);
        }
    }
}
