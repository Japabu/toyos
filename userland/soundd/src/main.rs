use toyos_abi::audio::{self, AudioInfo, AudioOpenRequest, AudioOpenResponse, MSG_AUDIO_OPEN, MSG_AUDIO_OPENED};
use toyos_abi::device;
use toyos_abi::ipc;
use toyos_abi::pipe;
use toyos_abi::poll as toyos_poll;
use toyos_abi::ring::RingHeader;
use toyos_abi::services;
use toyos_abi::shm::SharedMemory;
use toyos_abi::syscall;
use toyos_abi::Fd;

struct AudioStream {
    data_fd: Fd,
    ring: *const RingHeader,
    control: Fd,
}

impl AudioStream {
    fn read_samples(&self, mix: &mut [i32]) {
        let ring = unsafe { &*self.ring };
        let mut raw = [0u8; 8192];
        let avail = ring.available() as usize;
        if avail < 2 {
            return;
        }
        let to_read = raw.len().min(avail) & !1;
        if to_read == 0 {
            return;
        }
        let n = ring.read(&mut raw[..to_read]);
        let samples = n / 2;
        let count = mix.len().min(samples);
        for i in 0..count {
            let sample = i16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]);
            mix[i] = mix[i].saturating_add(sample as i32);
        }
    }

    fn is_dead(&self) -> bool {
        let ring = unsafe { &*self.ring };
        ring.is_writer_closed() && ring.available() == 0
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        syscall::close(self.data_fd);
        syscall::close(self.control);
    }
}

fn read_struct<T: Copy>(fd: Fd) -> T {
    let mut buf = [0u8; 256];
    let size = core::mem::size_of::<T>();
    assert!(size <= buf.len());
    let n = syscall::read(fd, &mut buf[..size]).expect("read device info failed");
    assert_eq!(n, size);
    unsafe { core::ptr::read(buf.as_ptr() as *const T) }
}

fn open_audio_stream(control: Fd, req: &AudioOpenRequest, next_id: &mut u32) -> Option<AudioStream> {
    let data_fd = pipe::open_by_id(req.pipe_id, true).ok()?;
    let ring = syscall::pipe_map(data_fd).expect("pipe_map failed") as *const RingHeader;
    let id = *next_id;
    *next_id += 1;
    let _ = ipc::send(control, MSG_AUDIO_OPENED, &AudioOpenResponse { stream_id: id });
    Some(AudioStream { data_fd, ring, control })
}

fn main() {
    let listener = services::listen("soundd").expect("soundd already running");

    let audio_fd = device::open_audio().expect("soundd: no audio device");
    let info: AudioInfo = read_struct(audio_fd);
    syscall::close(audio_fd);

    let num_buffers = info.num_buffers as usize;
    let mut dma_pages: Vec<SharedMemory> = Vec::with_capacity(num_buffers);
    for i in 0..num_buffers {
        dma_pages.push(SharedMemory::map(info.buf_tokens[i], 4096));
    }

    let period_frames = info.period_bytes as usize / 4; // stereo i16 = 4 bytes/frame
    let period_samples = period_frames * info.channels as usize;
    let period_ns = (period_frames as u64 * 1_000_000_000) / info.sample_rate as u64;

    eprintln!("soundd: ready, {} buffers, {}Hz, {} bytes/period",
        num_buffers, info.sample_rate, info.period_bytes);

    let mut streams: Vec<AudioStream> = Vec::new();
    let mut next_stream_id: u32 = 1;
    let mut free_mask: u32 = (1u32 << num_buffers) - 1;

    loop {
        // 1. Reclaim completed DMA buffers
        free_mask |= audio::audio_poll();

        // 2. Fill and submit all free DMA buffers
        while free_mask != 0 {
            let idx = free_mask.trailing_zeros() as usize;
            if idx >= num_buffers {
                break;
            }
            free_mask &= !(1 << idx);

            let page = dma_pages[idx].as_mut_slice();
            let out = unsafe {
                core::slice::from_raw_parts_mut(page.as_mut_ptr() as *mut i16, period_samples)
            };

            if streams.is_empty() {
                for s in out.iter_mut() { *s = 0; }
            } else {
                let mut mix = vec![0i32; period_samples];
                for stream in &streams {
                    stream.read_samples(&mut mix);
                }
                for (i, &s) in mix.iter().enumerate() {
                    out[i] = s.clamp(-32768, 32767) as i16;
                }
            }

            audio::audio_submit(idx as u32, info.period_bytes);
        }

        // 3. Clean up dead streams
        streams.retain(|s| !s.is_dead());

        // 4. Poll: listener + control sockets from active streams
        let timeout = if streams.is_empty() { 100_000_000 } else { period_ns };
        let mut poll_fds: Vec<u64> = Vec::with_capacity(1 + streams.len());
        poll_fds.push(listener.0 as u64);
        for stream in &streams {
            poll_fds.push(stream.control.0 as u64);
        }
        let result = toyos_poll::poll_timeout(&poll_fds, Some(timeout));

        // 5. Accept new connections
        if result.fd(0) {
            let conn = services::accept(listener).expect("accept failed");
            let client = conn.fd;
            let header = ipc::recv_header(client);
            match header.msg_type {
                MSG_AUDIO_OPEN => {
                    let req: AudioOpenRequest = ipc::recv_payload(client, &header);
                    if let Some(stream) = open_audio_stream(client, &req, &mut next_stream_id) {
                        streams.push(stream);
                    }
                }
                other => eprintln!("soundd: unexpected message type {other} from new client"),
            }
        }

        // 6. Handle control messages from existing clients
        for i in 0..streams.len() {
            if result.fd(1 + i) {
                let fd = streams[i].control;
                let header = ipc::recv_header(fd);
                match header.msg_type {
                    other => eprintln!("soundd: unknown control message type {other}"),
                }
            }
        }
    }
}
