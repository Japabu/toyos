//! soundd IPC protocol and audio hardware wrappers.

use core::sync::atomic::{AtomicU32, Ordering};
use toyos_abi::syscall;
use crate::ipc::IpcError;
use crate::shm::SharedMemory;

pub const MSG_AUDIO_OPEN: u32 = 1;
pub const MSG_AUDIO_OPENED: u32 = 2;
pub const MSG_AUDIO_SET_VOLUME: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioOpenResponse {
    pub stream_id: u32,
    pub shm_token: u32,
    pub ring_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AudioSetVolume {
    pub stream_id: u32,
    pub volume: u32,
}

pub fn audio_submit(buf_idx: u32, len: u32) {
    syscall::audio_submit(buf_idx, len);
}

pub fn audio_poll() -> u32 {
    syscall::audio_poll()
}

// ---------------------------------------------------------------------------
// AudioRing — SPSC ring buffer over shared memory for audio streaming
// ---------------------------------------------------------------------------

#[repr(C, align(64))]
struct Header {
    write_cursor: AtomicU32,
    read_cursor: AtomicU32,
    capacity: u32,
    flags: AtomicU32,
}

const WRITER_CLOSED: u32 = 1;

fn header(shm: &SharedMemory) -> &Header {
    unsafe { &*(shm.as_ptr() as *const Header) }
}

fn data_ptr(shm: &SharedMemory) -> *mut u8 {
    unsafe { shm.as_ptr().add(core::mem::size_of::<Header>()) }
}

fn available(shm: &SharedMemory) -> u32 {
    let h = header(shm);
    let w = h.write_cursor.load(Ordering::Acquire);
    let r = h.read_cursor.load(Ordering::Acquire);
    w.wrapping_sub(r)
}

/// Reader half of an audio ring buffer. Used by soundd to consume audio
/// samples written by clients.
pub struct AudioRingReader {
    shm: SharedMemory,
}

impl AudioRingReader {
    pub fn new(shm: SharedMemory) -> Self {
        Self { shm }
    }

    pub fn available(&self) -> u32 {
        available(&self.shm)
    }

    pub fn read(&self, buf: &mut [u8]) -> usize {
        let avail = self.available() as usize;
        if avail == 0 {
            return 0;
        }
        let h = header(&self.shm);
        let count = buf.len().min(avail);
        let cap = h.capacity as usize;
        let r = h.read_cursor.load(Ordering::Relaxed) as usize;
        let offset = r % cap;
        let data = data_ptr(&self.shm);

        let first = count.min(cap - offset);
        unsafe {
            core::ptr::copy_nonoverlapping(data.add(offset), buf.as_mut_ptr(), first);
            if first < count {
                core::ptr::copy_nonoverlapping(data, buf.as_mut_ptr().add(first), count - first);
            }
        }

        h.read_cursor.store((r + count) as u32, Ordering::Release);
        count
    }

    pub fn is_writer_closed(&self) -> bool {
        header(&self.shm).flags.load(Ordering::Acquire) & WRITER_CLOSED != 0
    }
}

/// Writer half of an audio ring buffer. Used by audio clients to produce
/// samples consumed by soundd.
pub struct AudioRingWriter {
    shm: SharedMemory,
}

impl AudioRingWriter {
    /// Initialize shared memory as an audio ring and take ownership as writer.
    pub fn new(mut shm: SharedMemory) -> Self {
        let capacity = shm.len() - core::mem::size_of::<Header>();
        let ptr = shm.as_mut_slice().as_mut_ptr() as *mut Header;
        unsafe {
            (*ptr).write_cursor = AtomicU32::new(0);
            (*ptr).read_cursor = AtomicU32::new(0);
            (*ptr).capacity = capacity as u32;
            (*ptr).flags = AtomicU32::new(0);
        }
        Self { shm }
    }

    pub fn write(&self, buf: &[u8]) -> usize {
        let h = header(&self.shm);
        let free = (h.capacity - available(&self.shm)) as usize;
        if free == 0 {
            return 0;
        }
        let count = buf.len().min(free);
        let cap = h.capacity as usize;
        let w = h.write_cursor.load(Ordering::Relaxed) as usize;
        let offset = w % cap;
        let data = data_ptr(&self.shm);

        let first = count.min(cap - offset);
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), data.add(offset), first);
            if first < count {
                core::ptr::copy_nonoverlapping(buf.as_ptr().add(first), data, count - first);
            }
        }

        h.write_cursor.store((w + count) as u32, Ordering::Release);
        count
    }

    pub fn close(&self) {
        header(&self.shm).flags.fetch_or(WRITER_CLOSED, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// AudioStream — ergonomic client for soundd
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AudioError {
    NotFound,
    Ipc(IpcError),
    Protocol(u32),
}

pub struct AudioStream {
    control: crate::Connection,
    ring: AudioRingWriter,
    stream_id: u32,
}

impl AudioStream {
    const BOOT_RETRIES: u32 = 100;
    const BOOT_RETRY_INTERVAL_NS: u64 = 10_000_000;

    /// Connect to soundd and open a stream. Retries connection during boot.
    pub fn open(sample_rate: u32, channels: u16, format: u16) -> Result<Self, AudioError> {
        let control = Self::connect_soundd()?;

        let req = AudioOpenRequest { sample_rate, channels, format };
        control.send(MSG_AUDIO_OPEN, &req).map_err(AudioError::Ipc)?;

        let (msg_type, resp): (u32, AudioOpenResponse) =
            control.recv().map_err(AudioError::Ipc)?;
        if msg_type != MSG_AUDIO_OPENED {
            return Err(AudioError::Protocol(msg_type));
        }

        let shm = SharedMemory::map(resp.shm_token, resp.ring_size as usize);
        let ring = AudioRingWriter::new(shm);

        Ok(Self { control, ring, stream_id: resp.stream_id })
    }

    pub fn ring(&self) -> &AudioRingWriter {
        &self.ring
    }

    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub fn set_volume(&self, volume: u32) -> Result<(), AudioError> {
        self.control.send(MSG_AUDIO_SET_VOLUME, &AudioSetVolume {
            stream_id: self.stream_id,
            volume,
        }).map_err(AudioError::Ipc)
    }

    fn connect_soundd() -> Result<crate::Connection, AudioError> {
        for _ in 0..Self::BOOT_RETRIES {
            if let Ok(conn) = crate::services::connect("soundd") {
                return Ok(conn);
            }
            syscall::nanosleep(Self::BOOT_RETRY_INTERVAL_NS);
        }
        Err(AudioError::NotFound)
    }
}
