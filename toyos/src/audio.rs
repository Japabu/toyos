//! soundd IPC protocol and slot-ring shared memory audio streaming.

use core::sync::atomic::Ordering;
use toyos_abi::audio::AudioSlotHeader;
use toyos_abi::syscall;
use crate::ipc::IpcError;
use crate::shm::SharedMemory;

pub const MSG_STREAM_OPEN: u32 = 1;
pub const MSG_STREAM_OPENED: u32 = 2;
pub const MSG_STREAM_SET_VOLUME: u32 = 3;
pub const MSG_STREAM_CLOSE: u32 = 4;
/// soundd rejected `MSG_STREAM_OPEN` (unsupported format/channels/rate).
pub const MSG_STREAM_ERROR: u32 = 5;

/// The only sample format currently implemented end-to-end.
pub const FORMAT_S16LE: u16 = 0;

crate::ipc_payload! {
    pub struct StreamOpenRequest {
        pub sample_rate: u32,
        pub channels: u16,
        pub format: u16,
    }

    pub struct StreamOpenResponse {
        pub shm_token: u32,
        /// The compiler reserved these bytes to align `signal_pipe_id`; naming
        /// them is what stops soundd's struct literal from sending whatever
        /// its stack held to every audio client.
        pub _pad0: u32,
        pub signal_pipe_id: u64,
        pub client_period_frames: u32,
        pub client_period_bytes: u32,
        pub device_sample_rate: u32,
        pub device_channels: u16,
        pub slot_count: u16,
    }

    pub struct StreamSetVolume {
        pub gain: f32,
    }
}

pub fn audio_submit(buf_idx: u32, len: u32) -> Result<(), syscall::SyscallError> {
    syscall::audio_submit(buf_idx, len)
}

pub struct AudioSlotWriter {
    shm: SharedMemory,
    period_bytes: u32,
    slot_count: u32,
}

/// Exclusive access to one ring slot. Holding the guard mutably borrows the
/// writer, so a second slot cannot be handed out until this one is committed
/// or dropped — the aliasing that made repeated `slot_data_mut` calls unsound
/// is unrepresentable.
pub struct SlotWriteGuard<'a> {
    writer: &'a mut AudioSlotWriter,
    idx: u32,
}

impl SlotWriteGuard<'_> {
    pub fn data(&mut self) -> &mut [u8] {
        let slot = self.idx % self.writer.slot_count;
        self.writer.slot_data_mut(slot)
    }

    /// Publish the filled slot to soundd.
    pub fn commit(self) {
        self.writer
            .header()
            .write_idx
            .store(self.idx.wrapping_add(1), Ordering::Release);
    }
}

impl AudioSlotWriter {
    pub fn new(shm: SharedMemory, period_bytes: u32, slot_count: u32) -> Self {
        Self { shm, period_bytes, slot_count }
    }

    fn header(&self) -> &AudioSlotHeader {
        unsafe { &*(self.shm.as_ptr() as *const AudioSlotHeader) }
    }

    fn slot_data_mut(&mut self, slot_idx: u32) -> &mut [u8] {
        let offset = AudioSlotHeader::SIZE + slot_idx as usize * self.period_bytes as usize;
        unsafe {
            core::slice::from_raw_parts_mut(self.shm.as_ptr().add(offset), self.period_bytes as usize)
        }
    }

    /// Acquire the next free slot for writing. Returns None if the ring is full.
    pub fn begin_fill(&mut self) -> Option<SlotWriteGuard<'_>> {
        // Only this side writes write_idx; read_idx needs Acquire so the slot
        // data reads soundd finished before releasing the slot are ordered.
        let w = self.header().write_idx.load(Ordering::Relaxed);
        let r = self.header().read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= self.slot_count {
            return None;
        }
        Some(SlotWriteGuard { writer: self, idx: w })
    }
}

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

    /// The oldest filled slot, or None if the ring is empty (underrun).
    ///
    /// The slot stays owned by soundd — the client may not refill it — until
    /// [`SlotReadGuard::advance`] publishes the consumption. Advancing before
    /// the data is copied out lets a concurrently-filling client overwrite the
    /// slot mid-read (torn audio).
    pub fn peek(&self) -> Option<SlotReadGuard<'_>> {
        let h = self.header();
        let w = h.write_idx.load(Ordering::Acquire);
        let r = h.read_idx.load(Ordering::Relaxed);
        if w == r {
            return None;
        }
        Some(SlotReadGuard { reader: self, idx: r })
    }
}

/// Access to the oldest filled slot. Advancing consumes the guard, so an
/// advance without a successful peek is unrepresentable — and the release
/// uses the index captured at peek time, never re-reading header state the
/// untrusted client can scribble on (a hostile peer rewinding `write_idx`
/// must only garble its own stream, not abort soundd).
pub struct SlotReadGuard<'a> {
    reader: &'a AudioSlotReader,
    idx: u32,
}

impl SlotReadGuard<'_> {
    pub fn data(&self) -> &[u8] {
        self.reader.slot_data(self.idx % self.reader.slot_count)
    }

    /// Release the slot back to the client.
    pub fn advance(self) {
        self.reader
            .header()
            .read_idx
            .store(self.idx.wrapping_add(1), Ordering::Release);
    }
}

#[derive(Debug)]
pub enum AudioError {
    NotFound,
    /// soundd rejected the requested format/channels/rate.
    Rejected,
    /// soundd closed the signal pipe (daemon exit or client removal).
    Disconnected,
    Ipc(IpcError),
    Protocol(u32),
}

pub struct AudioStream {
    control: crate::Connection,
    slot_writer: AudioSlotWriter,
    signal_fd: toyos_abi::Fd,
    period_frames: u32,
    device_sample_rate: u32,
    device_channels: u16,
}

impl AudioStream {
    const BOOT_RETRIES: u32 = 100;
    const BOOT_RETRY_INTERVAL_NS: u64 = 10_000_000;

    pub fn open(sample_rate: u32, channels: u16, format: u16) -> Result<Self, AudioError> {
        let control = Self::connect_soundd()?;

        let req = StreamOpenRequest { sample_rate, channels, format };
        control.send(MSG_STREAM_OPEN, &req).map_err(AudioError::Ipc)?;

        let header = control.recv_header().map_err(AudioError::Ipc)?;
        let resp: StreamOpenResponse = match header.msg_type {
            MSG_STREAM_OPENED => control.recv_payload(&header).map_err(AudioError::Ipc)?,
            MSG_STREAM_ERROR => return Err(AudioError::Rejected),
            other => return Err(AudioError::Protocol(other)),
        };

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
            device_sample_rate: resp.device_sample_rate,
            device_channels: resp.device_channels,
        })
    }

    /// Block until soundd signals, then fill all available ring slots via the
    /// callback. Each callback invocation receives one period-sized buffer.
    ///
    /// Returns `Err(Disconnected)` on signal-pipe EOF (soundd is gone or
    /// removed this client) — the caller must stop the stream, not retry.
    pub fn wait_and_fill(&mut self, mut callback: impl FnMut(&mut [u8])) -> Result<(), AudioError> {
        let mut buf = [0u8; 64];
        match syscall::read(self.signal_fd, &mut buf) {
            Ok(0) => return Err(AudioError::Disconnected),
            Ok(_) => {}
            Err(e) => return Err(AudioError::Ipc(IpcError::Syscall(e))),
        }
        while let Some(mut slot) = self.slot_writer.begin_fill() {
            callback(slot.data());
            slot.commit();
        }
        Ok(())
    }

    pub fn period_frames(&self) -> u32 {
        self.period_frames
    }

    pub fn device_sample_rate(&self) -> u32 {
        self.device_sample_rate
    }

    pub fn device_channels(&self) -> u16 {
        self.device_channels
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

impl Drop for AudioStream {
    fn drop(&mut self) {
        syscall::close(self.signal_fd);
    }
}
