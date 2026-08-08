//! soundd as the driver of a virtio-sound device.
//!
//! `specs/hda-driver-plan.md` §4.1: the kernel negotiated the device's features,
//! built its virtqueues and owns their descriptors, and answers one register
//! write per queue doorbell. Everything that is a *decision* is here — which
//! stream, at what rate and format, when a period goes out and when the stream
//! runs — and every one of them is a message this process writes into a buffer
//! of its own and publishes by index.
//!
//! There is no descriptor here and no physical address. The chains were built
//! once, at bind, out of offsets into the region below; what this file writes
//! into an avail ring is which of them to run.
//!
//! Structure layouts and command codes are VirtIO 1.2 §5.14.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use toyos::shm::SharedMemory;
use toyos::VirtioSoundDev;
use toyos_abi::audio::AudioCompletionRecord;
use toyos_abi::syscall::SyscallError;
use toyos_abi::virtio_sound as abi;

const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;

const EVT_JACK_CONNECTED: u32 = 0x1000;
const EVT_JACK_DISCONNECTED: u32 = 0x1001;
const EVT_PCM_PERIOD_ELAPSED: u32 = 0x1100;
const EVT_PCM_XRUN: u32 = 0x1101;

const S_OK: u32 = 0x8000;

const FMT_S16: u8 = 5;
const RATE_44100: u8 = 6;
const RATE_48000: u8 = 7;

/// The one stream this driver opens. A device with several is a decision this
/// file has not been asked to make, and taking the first of them is what §2.3
/// forbids one layer down — but virtio-sound numbers its streams and reports
/// only how many, so there is nothing here to choose *by*.
const STREAM_ID: u32 = 0;

/// The rates this driver can encode, best first. 44100 leads because it is what
/// the mixer, the resampler and the gate's recorded counters are sized against;
/// 48000 is the one every other device offers.
const SUPPORTED_RATES: [(u32, u8); 2] = [(44100, RATE_44100), (48000, RATE_48000)];

/// How many times a control command's completion is polled before the device is
/// called dead.
///
/// **A count and not a deadline, and the difference is the whole of it.** A wall
/// clock keeps running while this guest is not: under host load a vCPU can lose
/// tens of milliseconds without executing an instruction, so a duration here
/// would measure the host's scheduler and report a healthy device as a stopped
/// one — at a suspend boundary, in the audio path. A count advances only when
/// this driver actually looked.
///
/// Policy, not physics: the specification has no number. What it is set against
/// is that a device answering at all answers in one round trip, so anything this
/// side of enormous separates "slow" from "gone".
const CTRL_POLLS: u32 = 100_000;
/// Spins between two looks at the used ring.
const SPINS_PER_POLL: u32 = 256;

#[repr(C)]
#[derive(Clone, Copy)]
struct Hdr {
    code: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct QueryInfo {
    hdr: Hdr,
    start_id: u32,
    count: u32,
    size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcmInfo {
    /// The info header the device leads every entry with: its HDA function node.
    hdr: u32,
    features: u32,
    formats: u64,
    rates: u64,
    direction: u8,
    channels_min: u8,
    channels_max: u8,
    _padding: [u8; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcmSetParams {
    hdr: Hdr,
    stream_id: u32,
    buffer_bytes: u32,
    period_bytes: u32,
    features: u32,
    channels: u8,
    format: u8,
    rate: u8,
    _padding: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcmHdr {
    hdr: Hdr,
    stream_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcmXfer {
    stream_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Event {
    code: u32,
    data: u32,
}

/// The control buffers the kernel described have to hold every message this file
/// sends or reads back, and the descriptor lengths are fixed at bind — so this
/// is the one place the two halves could disagree about a size.
const _: () = {
    assert!(core::mem::size_of::<QueryInfo>() <= abi::CTRL_BUF_BYTES);
    assert!(core::mem::size_of::<PcmSetParams>() <= abi::CTRL_BUF_BYTES);
    assert!(core::mem::size_of::<PcmHdr>() <= abi::CTRL_BUF_BYTES);
    assert!(
        core::mem::size_of::<Hdr>() + core::mem::size_of::<PcmInfo>() <= abi::CTRL_BUF_BYTES
    );
};

// Avail: flags(u16) idx(u16) ring[size](u16). Used: flags(u16) idx(u16)
// ring[size](id:u32 len:u32).
const AVAIL_IDX: usize = 2;
const AVAIL_RING: usize = 4;
const USED_IDX: usize = 2;
const USED_RING: usize = 4;
const USED_ELEM: usize = 8;

/// The driver's half of a virtqueue: an index to publish, and a doorbell to ring
/// after publishing it.
///
/// No descriptor table, which is the whole point — the chains are the kernel's
/// and this names one by its head.
struct Avail {
    ring: *mut u8,
    size: u16,
    doorbell: u32,
    queue: u16,
}

impl Avail {
    /// Make `head`'s chain available to the device and ring the doorbell.
    fn publish(&self, dev: &VirtioSoundDev, head: u16) {
        unsafe {
            let idx_ptr = self.ring.add(AVAIL_IDX) as *mut u16;
            let idx = read_volatile(idx_ptr);
            write_volatile(
                self.ring.add(AVAIL_RING + (idx % self.size) as usize * 2) as *mut u16,
                head,
            );
            // The chain has to be in the ring before the index covers it.
            fence(Ordering::Release);
            write_volatile(idx_ptr, idx.wrapping_add(1));
            fence(Ordering::Release);
        }
        dev.notify(self.doorbell, self.queue).unwrap_or_else(|e| {
            panic!("soundd: virtio-sound refused queue {}'s doorbell: {e}", self.queue)
        });
    }
}

/// A used ring this process consumes.
///
/// The TX queue has no such thing here and that is the design: its consumer is
/// the interrupt handler, which timestamps the completion, and a ring this
/// process could rewrite would be a mask the kernel derived from userland.
struct Used {
    ring: *mut u8,
    size: u16,
    last: u16,
}

impl Used {
    fn poll(&mut self) -> Option<u16> {
        unsafe {
            let used_idx = read_volatile(self.ring.add(USED_IDX) as *const u16);
            if used_idx == self.last {
                return None;
            }
            // The device wrote the element before bumping its index.
            fence(Ordering::Acquire);
            let slot = (self.last % self.size) as usize;
            let id = read_volatile(self.ring.add(USED_RING + slot * USED_ELEM) as *const u32);
            self.last = self.last.wrapping_add(1);
            Some(id as u16)
        }
    }
}

/// Why this machine's virtio-sound device cannot carry audio. Each is a line
/// soundd prints before falling back to the null sink — "no sound" without which
/// of these it was is a report nobody can act on.
pub enum Refusal {
    NoDevice(SyscallError),
    /// The kernel's answers stopped making sense, which is a bug here or there
    /// and never a property of the machine.
    Kernel(SyscallError),
    /// The device did not answer a control command inside its deadline.
    Silent(&'static str),
    /// It answered, and said no.
    Rejected(&'static str, u32),
    /// It offers nothing this driver implements. Already named on its own line,
    /// with the bitmap it was read from.
    NoFormat,
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDevice(e) => write!(f, "no virtio-sound device the kernel bound ({e})"),
            Self::Kernel(e) => write!(f, "the kernel refused a call this driver has to make ({e})"),
            Self::Silent(what) => write!(f, "the device never answered {what}"),
            Self::Rejected(what, status) => write!(f, "the device refused {what} ({status:#x})"),
            Self::NoFormat => write!(f, "the device offers no format or rate this driver mixes"),
        }
    }
}

pub struct Virtio {
    dev: VirtioSoundDev,
    /// The region every ring and buffer below points into. Held so the mapping
    /// outlives the pointers taken out of it.
    _shm: SharedMemory,
    base: *mut u8,
    control: Avail,
    control_done: Used,
    events: Avail,
    events_done: Used,
    tx: Avail,
    running: bool,
}

impl Virtio {
    /// Claim the device, ask what its stream can do, and configure it.
    pub fn claim() -> Result<(Self, u32, u8), Refusal> {
        let dev = VirtioSoundDev::open().map_err(Refusal::NoDevice)?;
        let info = dev.info().map_err(Refusal::Kernel)?;
        let shm = SharedMemory::map(info.dma_token, 2 * 1024 * 1024)
            .map_err(Refusal::Kernel)?;
        let base = shm.as_ptr();

        let avail = |offset: usize, size: u16, doorbell: u32, queue: u16| Avail {
            ring: unsafe { base.add(offset) },
            size,
            doorbell,
            queue,
        };
        let used = |offset: usize, size: u16| Used {
            ring: unsafe { base.add(offset) },
            size,
            last: 0,
        };
        let mut virtio = Virtio {
            dev,
            _shm: shm,
            base,
            control: avail(
                abi::OFF_CTRL_AVAIL,
                abi::CONTROL_QUEUE_SIZE,
                info.notify_control,
                abi::CONTROL_QUEUE,
            ),
            control_done: used(abi::OFF_CTRL_USED, abi::CONTROL_QUEUE_SIZE),
            events: avail(
                abi::OFF_EVENT_AVAIL,
                abi::EVENT_QUEUE_SIZE,
                info.notify_event,
                abi::EVENT_QUEUE,
            ),
            events_done: used(abi::OFF_EVENT_USED, abi::EVENT_QUEUE_SIZE),
            tx: avail(abi::OFF_TX_AVAIL, abi::TX_QUEUE_SIZE, info.notify_tx, abi::TX_QUEUE),
            running: false,
        };

        for i in 0..abi::EVENT_BUFS {
            virtio.events.publish(&virtio.dev, i as u16);
        }

        let pcm = virtio.pcm_info()?;
        eprintln!(
            "virtio-sound: stream {STREAM_ID}: dir={} ch={}-{} fmts={:#x} rates={:#x}",
            pcm.direction, pcm.channels_min, pcm.channels_max, pcm.formats, pcm.rates
        );
        let (rate, channels) = choose_params(&pcm).ok_or(Refusal::NoFormat)?;
        virtio.configure(rate, channels)?;
        Ok((virtio, rate, channels))
    }

    pub fn dev(&self) -> &VirtioSoundDev {
        &self.dev
    }

    pub fn buffer(&self, idx: usize) -> *mut u8 {
        unsafe { self.base.add(abi::OFF_PCM + idx * abi::PERIOD_BYTES) }
    }

    pub fn completions(&self, out: &mut [AudioCompletionRecord]) -> usize {
        match self.dev.read_completions(out) {
            Ok(n) => n,
            Err(SyscallError::WouldBlock) => 0,
            Err(e) => panic!("soundd: read_completions failed: {e:?}"),
        }
    }

    /// Put period `idx` on the wire.
    ///
    /// The chain is already built and its PCM descriptor already names this
    /// buffer, so what a period costs is a store of the stream id, a store into
    /// the avail ring, and one doorbell — the same one syscall the deleted
    /// `SYS_AUDIO_SUBMIT` cost.
    pub fn submit(&mut self, idx: usize, bytes: usize) {
        assert_eq!(
            bytes,
            abi::PERIOD_BYTES,
            "soundd: the TX chain's PCM descriptor is a whole period and the kernel built it"
        );
        self.start();
        unsafe {
            write_volatile(
                self.base.add(abi::OFF_TX_XFER + idx * abi::XFER_STRIDE) as *mut PcmXfer,
                PcmXfer { stream_id: STREAM_ID },
            );
        }
        self.tx.publish(&self.dev, abi::tx_chain_head(idx));
    }

    fn start(&mut self) {
        if self.running {
            return;
        }
        self.simple_ctrl(R_PCM_START, "START")
            .unwrap_or_else(|e| panic!("soundd: virtio-sound could not start its stream: {e}"));
        self.running = true;
        eprintln!("virtio-sound: stream {STREAM_ID} started");
    }

    pub fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.simple_ctrl(R_PCM_STOP, "STOP")
            .unwrap_or_else(|e| panic!("soundd: virtio-sound could not stop its stream: {e}"));
        self.running = false;
        eprintln!("virtio-sound: stream {STREAM_ID} stopped");
    }

    /// Report what the device says went wrong, and repost the buffer it said it
    /// in. The device's own view of an underrun, which soundd's counters cannot
    /// see: they measure what this process failed to submit.
    pub fn poll_events(&mut self) {
        while let Some(id) = self.events_done.poll() {
            let idx = id as usize;
            if idx >= abi::EVENT_BUFS {
                eprintln!("soundd: virtio-sound returned event buffer {idx}, which it never had");
                continue;
            }
            let event = unsafe {
                read_volatile(
                    self.base.add(abi::OFF_EVENT_BUFS + idx * abi::EVENT_BUF_STRIDE)
                        as *const Event,
                )
            };
            let name = match event.code {
                EVT_JACK_CONNECTED => " (jack connected)",
                EVT_JACK_DISCONNECTED => " (jack disconnected)",
                EVT_PCM_PERIOD_ELAPSED => " (period elapsed)",
                EVT_PCM_XRUN => " (PCM XRUN)",
                _ => "",
            };
            eprintln!(
                "virtio-sound: device event {:#x}{name} data={}",
                event.code, event.data
            );
            self.events.publish(&self.dev, id);
        }
    }

    fn pcm_info(&mut self) -> Result<PcmInfo, Refusal> {
        let query = QueryInfo {
            hdr: Hdr { code: R_PCM_INFO },
            start_id: STREAM_ID,
            count: 1,
            size: core::mem::size_of::<PcmInfo>() as u32,
        };
        self.ctrl(&query, "PCM_INFO")?;
        Ok(unsafe {
            core::ptr::read_unaligned(
                self.base.add(abi::OFF_CTRL_RESP + core::mem::size_of::<Hdr>()) as *const PcmInfo,
            )
        })
    }

    fn configure(&mut self, rate: u32, channels: u8) -> Result<(), Refusal> {
        // `expect`, not a fallback: `choose_params` has already checked this rate
        // against the device's own bitmap, so an unencodable one here means the
        // two disagree — a driver bug, not a device we cannot drive.
        let code = rate_code(rate).expect("soundd: a rate chosen from the device's own bitmap");
        let params = PcmSetParams {
            hdr: Hdr { code: R_PCM_SET_PARAMS },
            stream_id: STREAM_ID,
            buffer_bytes: (abi::PERIOD_BYTES * abi::PERIODS) as u32,
            period_bytes: abi::PERIOD_BYTES as u32,
            features: 0,
            channels,
            format: FMT_S16,
            rate: code,
            _padding: 0,
        };
        self.ctrl(&params, "SET_PARAMS")?;
        self.simple_ctrl(R_PCM_PREPARE, "PREPARE")?;
        eprintln!("virtio-sound: configured stream {STREAM_ID}: {rate}Hz {channels}ch s16le");
        Ok(())
    }

    fn simple_ctrl(&mut self, code: u32, what: &'static str) -> Result<(), Refusal> {
        self.ctrl(&PcmHdr { hdr: Hdr { code }, stream_id: STREAM_ID }, what)
    }

    /// One control command: copy the request into the buffer the kernel's chain
    /// describes, publish that chain, wait for it back, and read the status.
    fn ctrl<T: Copy>(&mut self, req: &T, what: &'static str) -> Result<(), Refusal> {
        unsafe {
            core::ptr::copy_nonoverlapping(
                req as *const T as *const u8,
                self.base.add(abi::OFF_CTRL_REQ),
                core::mem::size_of::<T>(),
            );
        }
        self.control.publish(&self.dev, 0);

        let mut answered = false;
        for _ in 0..CTRL_POLLS {
            if self.control_done.poll().is_some() {
                answered = true;
                break;
            }
            for _ in 0..SPINS_PER_POLL {
                core::hint::spin_loop();
            }
        }
        if !answered {
            return Err(Refusal::Silent(what));
        }

        let status = unsafe { read_volatile(self.base.add(abi::OFF_CTRL_RESP) as *const u32) };
        if status != S_OK {
            return Err(Refusal::Rejected(what, status));
        }
        Ok(())
    }
}

fn rate_code(hz: u32) -> Option<u8> {
    SUPPORTED_RATES.iter().find(|(rate, _)| *rate == hz).map(|(_, code)| *code)
}

/// Pick a rate and channel count the device actually advertises.
///
/// `None` means it offers nothing this driver implements — audio is optional, so
/// soundd presents the null sink rather than dying over a peripheral, but the
/// log has to name the missing capability or the next person is decoding a
/// bitmap by hand on a laptop with no serial.
fn choose_params(info: &PcmInfo) -> Option<(u32, u8)> {
    if info.formats & (1 << FMT_S16) == 0 {
        eprintln!(
            "virtio-sound: no usable format — device offers {:#x}, driver needs S16 (bit {FMT_S16})",
            info.formats
        );
        return None;
    }

    let Some(&(rate, _)) = SUPPORTED_RATES.iter().find(|(_, code)| info.rates & (1 << code) != 0)
    else {
        eprintln!(
            "virtio-sound: no usable rate — device offers {:#x}, driver needs 44100 (bit \
             {RATE_44100}) or 48000 (bit {RATE_48000})",
            info.rates
        );
        return None;
    };

    // Stereo if the device takes it; soundd converts either way, so the only
    // unusable case is a device whose minimum is more channels than we mix.
    if info.channels_min > 2 {
        eprintln!(
            "virtio-sound: no usable channel count — device needs at least {}, driver mixes at \
             most 2",
            info.channels_min
        );
        return None;
    }
    let channels = if info.channels_max >= 2 { 2 } else { info.channels_max };
    if channels == 0 {
        eprintln!("virtio-sound: device advertises a maximum of zero channels");
        return None;
    }
    Some((rate, channels))
}
