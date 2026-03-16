use toyos_abi::audio::{self, AudioInfo, AudioOpenRequest, AudioOpenResponse, MSG_AUDIO_OPEN, MSG_AUDIO_OPENED};
use toyos_abi::device;
use toyos_abi::message;
use toyos_abi::pipe;
use toyos_abi::poll as toyos_poll;
use toyos_abi::ring::RingHeader;
use toyos_abi::services;
use toyos_abi::shm::SharedMemory;
use toyos_abi::syscall;
use toyos_abi::{Fd, Pid};

struct AudioStream {
    pipe_fd: Fd,
    ring: *const RingHeader,
}

impl AudioStream {
    /// Read available bytes from the pipe ring buffer into a local sample buffer.
    fn read_samples(&self, mix: &mut [i32]) {
        let ring = unsafe { &*self.ring };
        // Read in chunks — ring buffer gives us raw bytes, we need i16 samples
        let mut raw = [0u8; 8192];
        loop {
            let avail = ring.available() as usize;
            if avail < 2 {
                break;
            }
            let to_read = raw.len().min(avail);
            // Align to sample boundary (2 bytes per i16)
            let to_read = to_read & !1;
            if to_read == 0 {
                break;
            }
            let n = ring.read(&mut raw[..to_read]);
            if n == 0 {
                break;
            }
            // Mix i16 samples into i32 accumulator
            let samples = n / 2;
            let remaining_mix = mix.len().min(samples);
            for i in 0..remaining_mix {
                let sample = i16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]]);
                mix[i] = mix[i].saturating_add(sample as i32);
            }
            // If ring had more data than mix buffer, we consumed it but
            // only mixed what fits. In practice, soundd calls this once per
            // period so sizes should match.
            break;
        }
    }

    fn is_dead(&self) -> bool {
        let ring = unsafe { &*self.ring };
        ring.is_writer_closed() && ring.available() == 0
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        syscall::close(self.pipe_fd);
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

fn open_pipe_read(pipe_id: u64) -> Option<Fd> {
    pipe::open_by_id(pipe_id, true).ok()
}

fn map_pipe_ring(fd: Fd) -> *const RingHeader {
    syscall::pipe_map(fd).expect("pipe_map failed") as *const RingHeader
}

fn main() {
    services::register("soundd").expect("soundd already running");

    // Claim the audio device
    let audio_fd = device::open_audio().expect("soundd: no audio device");
    let info: AudioInfo = read_struct(audio_fd);
    syscall::close(audio_fd);

    // Map DMA data pages
    let num_buffers = info.num_buffers as usize;
    let mut dma_pages: Vec<SharedMemory> = Vec::with_capacity(num_buffers);
    for i in 0..num_buffers {
        dma_pages.push(SharedMemory::map(info.buf_tokens[i], 4096));
    }

    let period_frames = info.period_bytes as usize / 4; // stereo i16 = 4 bytes/frame
    let period_samples = period_frames * info.channels as usize;

    // Nanoseconds per period for poll timeout
    let period_ns = (period_frames as u64 * 1_000_000_000) / info.sample_rate as u64;

    eprintln!("soundd: ready, {} buffers, {}Hz, {} bytes/period",
        num_buffers, info.sample_rate, info.period_bytes);

    let mut streams: Vec<AudioStream> = Vec::new();
    let mut next_stream_id: u32 = 1;
    // Track which DMA buffers are free (bit N = buffer N is free)
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

            // Mix from all client streams into this DMA page
            let page = dma_pages[idx].as_mut_slice();
            let out = unsafe {
                core::slice::from_raw_parts_mut(
                    page.as_mut_ptr() as *mut i16,
                    period_samples,
                )
            };

            if streams.is_empty() {
                // Silence
                for s in out.iter_mut() {
                    *s = 0;
                }
            } else {
                // Mix: accumulate in i32, clamp to i16
                let mut mix = vec![0i32; period_samples];
                for stream in streams.iter() {
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

        // 4. Handle incoming messages (non-blocking drain)
        let timeout = if streams.is_empty() { 100_000_000 } else { period_ns };
        let result = toyos_poll::poll_timeout(&[], Some(timeout));
        if result.messages() {
            loop {
                let msg = message::recv();
                match msg.msg_type {
                    MSG_AUDIO_OPEN => {
                        let sender = msg.sender;
                        let req: AudioOpenRequest = msg.payload();
                        if let Some(fd) = open_pipe_read(req.pipe_id) {
                            let ring = map_pipe_ring(fd);
                            let id = next_stream_id;
                            next_stream_id += 1;
                            streams.push(AudioStream { pipe_fd: fd, ring });

                            message::send(Pid(sender), MSG_AUDIO_OPENED, &AudioOpenResponse { stream_id: id });
                        }
                    }
                    other => {
                        eprintln!("soundd: unknown message type {other} from pid {}", msg.sender);
                    }
                }
                // Drain remaining messages without blocking
                let check = toyos_poll::poll_timeout(&[], Some(0));
                if !check.messages() {
                    break;
                }
            }
        }
    }
}
