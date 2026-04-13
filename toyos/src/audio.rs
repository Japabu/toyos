//! soundd IPC protocol and double-buffer shared memory audio streaming.

use core::sync::atomic::Ordering;
use toyos_abi::audio::AudioSlotHeader;
use toyos_abi::syscall;
use crate::ipc::IpcError;
use crate::shm::SharedMemory;

// ---------------------------------------------------------------------------
// IPC message types
// ---------------------------------------------------------------------------

pub const MSG_STREAM_OPEN: u32 = 1;
pub const MSG_STREAM_OPENED: u32 = 2;
pub const MSG_STREAM_SET_VOLUME: u32 = 3;
pub const MSG_STREAM_CLOSE: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StreamOpenRequest {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StreamOpenResponse {
    pub shm_token: u32,
    pub signal_pipe_id: u64,
    pub client_period_frames: u32,
    pub client_period_bytes: u32,
    pub device_sample_rate: u32,
    pub device_channels: u16,
    pub slot_count: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct StreamSetVolume {
    pub gain: f32,
}

pub fn audio_submit(buf_idx: u32, len: u32) {
    syscall::audio_submit(buf_idx, len);
}

// ---------------------------------------------------------------------------
// AudioSlotWriter — client side of slot-ring protocol
// ---------------------------------------------------------------------------

pub struct AudioSlotWriter {
    shm: SharedMemory,
    period_bytes: u32,
    slot_count: u32,
}

impl AudioSlotWriter {
    pub fn new(shm: SharedMemory, period_bytes: u32, slot_count: u32) -> Self {
        Self { shm, period_bytes, slot_count }
    }

    fn header(&self) -> &AudioSlotHeader {
        unsafe { &*(self.shm.as_ptr() as *const AudioSlotHeader) }
    }

    fn slot_data_mut(&self, slot_idx: u32) -> &mut [u8] {
        let offset = AudioSlotHeader::SIZE + slot_idx as usize * self.period_bytes as usize;
        unsafe {
            core::slice::from_raw_parts_mut(self.shm.as_ptr().add(offset), self.period_bytes as usize)
        }
    }

    /// Try to acquire a slot for writing. Returns None if the ring is full.
    pub fn try_fill(&self) -> Option<(u32, &mut [u8])> {
        let w = self.header().write_idx.load(Ordering::Acquire);
        let r = self.header().read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= self.slot_count {
            return None;
        }
        let slot_idx = w % self.slot_count;
        Some((w, self.slot_data_mut(slot_idx)))
    }

    /// Commit a filled slot, advancing write_idx.
    pub fn commit(&self, w: u32) {
        self.header().write_idx.store(w.wrapping_add(1), Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// AudioSlotReader — soundd side of slot-ring protocol
// ---------------------------------------------------------------------------

pub struct AudioSlotReader {
    shm: SharedMemory,
    period_bytes: u32,
    slot_count: u32,
}

impl AudioSlotReader {
    pub fn new(shm: SharedMemory, period_bytes: u32, slot_count: u32) -> Self {
        Self { shm, period_bytes, slot_count }
    }

    fn header(&self) -> &AudioSlotHeader {
        unsafe { &*(self.shm.as_ptr() as *const AudioSlotHeader) }
    }

    fn slot_data(&self, slot_idx: u32) -> &[u8] {
        let offset = AudioSlotHeader::SIZE + slot_idx as usize * self.period_bytes as usize;
        unsafe {
            core::slice::from_raw_parts(self.shm.as_ptr().add(offset), self.period_bytes as usize)
        }
    }

    /// Consume one filled slot. Returns None if the ring is empty (underrun).
    pub fn try_consume(&self) -> Option<&[u8]> {
        let h = self.header();
        let w = h.write_idx.load(Ordering::Acquire);
        let r = h.read_idx.load(Ordering::Acquire);
        if w == r {
            return None;
        }
        let slot_idx = r % self.slot_count;
        let slice = self.slot_data(slot_idx);
        h.read_idx.store(r.wrapping_add(1), Ordering::Release);
        Some(slice)
    }

    pub fn period_bytes(&self) -> u32 {
        self.period_bytes
    }
}

// ---------------------------------------------------------------------------
// AudioStream — client handle for soundd
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AudioError {
    NotFound,
    Ipc(IpcError),
    Protocol(u32),
}

pub struct AudioStream {
    control: crate::Connection,
    slot_writer: AudioSlotWriter,
    signal_fd: toyos_abi::Fd,
    period_frames: u32,
    period_bytes: u32,
}

impl AudioStream {
    const BOOT_RETRIES: u32 = 100;
    const BOOT_RETRY_INTERVAL_NS: u64 = 10_000_000;

    pub fn open(sample_rate: u32, channels: u16, format: u16) -> Result<Self, AudioError> {
        let control = Self::connect_soundd()?;

        let req = StreamOpenRequest { sample_rate, channels, format };
        control.send(MSG_STREAM_OPEN, &req).map_err(AudioError::Ipc)?;

        let (msg_type, resp): (u32, StreamOpenResponse) =
            control.recv().map_err(AudioError::Ipc)?;
        if msg_type != MSG_STREAM_OPENED {
            return Err(AudioError::Protocol(msg_type));
        }

        let slot_count = resp.slot_count as u32;
        let shm_size = AudioSlotHeader::SIZE + slot_count as usize * resp.client_period_bytes as usize;
        let shm = SharedMemory::map(resp.shm_token, shm_size);
        let slot_writer = AudioSlotWriter::new(shm, resp.client_period_bytes, slot_count);

        let signal_fd = syscall::pipe_open(resp.signal_pipe_id, 0)
            .map_err(|e| AudioError::Ipc(IpcError::Syscall(e)))?;

        Ok(Self {
            control,
            slot_writer,
            signal_fd,
            period_frames: resp.client_period_frames,
            period_bytes: resp.client_period_bytes,
        })
    }

    /// Block until soundd signals, then fill all available ring slots via the callback.
    /// Each callback invocation receives one period-sized buffer to fill.
    pub fn wait_and_fill(&self, mut callback: impl FnMut(&mut [u8])) {
        let mut buf = [0u8; 64];
        let _ = syscall::read(self.signal_fd, &mut buf);
        while let Some((w, slot)) = self.slot_writer.try_fill() {
            callback(slot);
            self.slot_writer.commit(w);
        }
    }

    pub fn period_frames(&self) -> u32 {
        self.period_frames
    }

    pub fn period_bytes(&self) -> u32 {
        self.period_bytes
    }

    pub fn set_volume(&self, gain: f32) -> Result<(), AudioError> {
        self.control.send(MSG_STREAM_SET_VOLUME, &StreamSetVolume { gain })
            .map_err(AudioError::Ipc)
    }

    pub fn close(&self) {
        let _ = self.control.signal(MSG_STREAM_CLOSE);
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
