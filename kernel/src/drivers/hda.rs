//! The Intel HDA stub: bring-up, one output stream, and the allow-list.
//!
//! `specs/hda-driver-plan.md` §4.1 is the design and §4.1.6 is the shape this
//! file implements. **The line is who touches a register.** The kernel resets
//! the controller, allocates the PCM ring and the buffer descriptor list,
//! programs every register whose value is an address or indexes one of those
//! structures, acknowledges the interrupt and derives the completion mask from
//! `SDnLPIB`. The driver — soundd — decides everything: which codecs answered,
//! which pin, which converter, the amplifiers, EAPD, the format. It reaches the
//! five registers those decisions land in through [`reg_write`], and the two it
//! has to poll through [`reg_read`], each checked against a positive list and
//! refused by name.
//!
//! Nothing here decides. The moment this file has to know which codec or which
//! pin, the line has moved and §4.1.5's last bullet has fired.
//!
//! Register offsets, bit positions and the descriptor layout come from the
//! Intel High Definition Audio specification.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use toyos_abi::audio::AudioCompletionRecord;
use toyos_abi::hda::HdaInfo;
use toyos_abi::syscall::{RegWidth, SyscallError};
use toyos_hda::stream;

use super::pci::PciDevice;
use crate::log;
use crate::mm::paging::CachePolicy;
use crate::mm::Mmio;
use crate::shared_memory;
use crate::sync::Lock;

const CLASS_MULTIMEDIA: u8 = 0x04;
const SUBCLASS_HDA: u8 = 0x03;

const HEADER_COMMAND: u64 = 0x04;
const HEADER_BAR0: u64 = 0x10;
const COMMAND_MEMORY_SPACE: u16 = 1 << 1;

const CAP_POWER_MANAGEMENT: u8 = 0x01;
const PM_CONTROL_STATUS: u64 = 0x04;
const PM_STATE_MASK: u16 = 0x3;
const PM_D3HOT_RECOVERY_NS: u64 = 10_000_000;

const GCAP: u64 = 0x00;
const VMIN: u64 = 0x02;
const VMAJ: u64 = 0x03;
const GCTL: u64 = 0x08;
const STATESTS: u64 = 0x0E;
const INTCTL: u64 = 0x20;
const IMMEDIATE_COMMAND: u64 = 0x60;
const IMMEDIATE_RESPONSE: u64 = 0x64;
const IMMEDIATE_STATUS: u64 = 0x68;

const GCTL_CRST: u32 = 1 << 0;
const INTCTL_GIE: u32 = 1 << 31;

/// The first stream descriptor's offset, and the stride between them. Input
/// descriptors come first, then output, then bidirectional — so which one is
/// the first output is a function of `GCAP.ISS` and never a constant.
const STREAM_BASE: u64 = 0x80;
const STREAM_STRIDE: u64 = 0x20;

const SD_CTL: u64 = 0x00;
/// The byte carrying the stream tag, which the codec's converter has to be
/// told the same number.
const SD_CTL_TAG: u64 = 0x02;
const SD_STS: u64 = 0x03;
const SD_LPIB: u64 = 0x04;
const SD_CBL: u64 = 0x08;
const SD_LVI: u64 = 0x0C;
const SD_FMT: u64 = 0x12;
const SD_BDPL: u64 = 0x18;
const SD_BDPU: u64 = 0x1C;

const SD_CTL_SRST: u8 = 1 << 0;
const SD_CTL_RUN: u8 = 1 << 1;
const SD_CTL_IOCE: u8 = 1 << 2;
const SD_CTL_FEIE: u8 = 1 << 3;
const SD_CTL_DEIE: u8 = 1 << 4;

const SD_STS_BCIS: u8 = 1 << 2;
const SD_STS_FIFOE: u8 = 1 << 3;
const SD_STS_DESE: u8 = 1 << 4;
const SD_STS_WRITE_CLEAR: u8 = SD_STS_BCIS | SD_STS_FIFOE | SD_STS_DESE;

/// The pipeline, in periods and bytes.
///
/// The same shape virtio-sound presents, and deliberately: soundd's mix loop,
/// its client ring depth and gate A's recorded counters are all sized against
/// eight periods of 512 bytes, so a second backend that chose differently would
/// be a second instrument as well as a second device.
const PERIODS: usize = 8;
const PERIOD_BYTES: usize = 512;

/// The stream tag this stream carries. One tag, one stream, and the number
/// reaches the codec through a verb soundd sends — so it has to be a number
/// both halves can name, which is what [`HdaInfo::stream_tag`] is for.
const STREAM_TAG: u8 = 1;

/// The smallest register window that can hold a stream descriptor. A BAR under
/// this is a function that is not the controller this driver understands, and
/// is refused by name rather than mapped and read past.
const MIN_BAR_BYTES: u64 = 0x1000;

/// How long a register bit is given to settle.
///
/// Policy, not physics: the specification's own numbers are microseconds, so an
/// expiry here is a device that has stopped rather than one that is slow. The
/// machine this ships on has no serial port, and a bring-up that spins forever
/// leaves its owner a black screen.
const SETTLE_NS: u64 = 100_000_000;

/// The delay the specification requires between releasing `CRST` and believing
/// `STATESTS`: 25 frames at 48 kHz, rounded up to a millisecond.
const CODEC_DETECT_NS: u64 = 1_000_000;

/// How many refused register accesses are named in the log before the driver is
/// told to stop asking.
///
/// Policy: a refusal is a driver bug worth reading, and an unbounded one is a
/// userland process choosing how much log the machine spends. Past this the
/// call still fails — only the line stops.
const MAX_NAMED_REFUSALS: usize = 16;

// --- what the interrupt handler may touch ---

/// The handler's whole view of the device.
///
/// Written once, before the vector is armed, and read with no lock afterwards —
/// the same contract `virtio_sound`'s `TX_ISR` has, and for the same reason: the
/// handler may interrupt a CPU holding [`CONTROLLER`].
struct StreamIsr {
    stream: UnsafeCell<Option<Mmio>>,
    /// The period the engine was last known to be playing. Written by the
    /// handler while the stream runs, and by [`reg_write`] on the edge that
    /// starts it — never both, because a stopped stream raises no interrupt.
    last: AtomicUsize,
    /// Periods completed and not yet handed to the driver. Accumulating rather
    /// than a ring: an interrupt carries nothing a later one does not, so there
    /// is no queue for a slow reader to overflow.
    mask: AtomicU32,
    /// `nanos_since_boot` at the newest interrupt folded into `mask`. The DLL
    /// measures a batch against its *last* grid point, which is why the newest
    /// is the one kept.
    timestamp: AtomicU64,
    /// FIFO and descriptor errors the device reported. Counted here and named
    /// once from the drain path: a handler that logs is a handler that produces
    /// work for the thing that failed.
    errors: AtomicU32,
    named_error: AtomicBool,
}

// SAFETY: `stream` is written once at init before the vector is armed and is
// read-only afterwards; every other field is atomic.
unsafe impl Sync for StreamIsr {}

static ISR: StreamIsr = StreamIsr {
    stream: UnsafeCell::new(None),
    last: AtomicUsize::new(0),
    mask: AtomicU32::new(0),
    timestamp: AtomicU64::new(0),
    errors: AtomicU32::new(0),
    named_error: AtomicBool::new(false),
};

fn isr_stream() -> Option<Mmio> {
    // SAFETY: sole writer is `init`, before the vector is armed.
    unsafe { *ISR.stream.get() }
}

/// Acknowledge one stream interrupt and record which periods it covered.
///
/// The order is the whole correctness of it: the status bit is cleared *before*
/// `SDnLPIB` is read, so a period that completes in between raises a fresh
/// interrupt rather than being acknowledged unseen.
pub fn isr_complete() {
    let timestamp = crate::clock::nanos_since_boot();
    let Some(stream) = isr_stream() else { return };

    let status = stream.read_u8(SD_STS);
    if status & SD_STS_WRITE_CLEAR == 0 {
        return;
    }
    stream.write_u8(SD_STS, status & SD_STS_WRITE_CLEAR);
    if status & (SD_STS_FIFOE | SD_STS_DESE) != 0 {
        ISR.errors.fetch_add(1, Ordering::Relaxed);
    }
    if status & SD_STS_BCIS == 0 {
        return;
    }

    let position = stream.read_u32(SD_LPIB);
    let last = ISR.last.load(Ordering::Relaxed);
    // A position the device cannot have is not a period to mark played.
    // `completed` refuses it rather than masking it to fit, and the driver
    // then sees no completion — which costs a wake, not a wrong buffer.
    let Some((mask, current)) = stream::completed(last, position, PERIOD_BYTES as u32, PERIODS)
    else {
        return;
    };
    ISR.last.store(current, Ordering::Relaxed);
    if mask == 0 {
        return;
    }
    ISR.timestamp.store(timestamp, Ordering::Relaxed);
    ISR.mask.fetch_or(mask, Ordering::Release);
    crate::irq_ring::isr_publish(crate::irq_ring::IrqSource::Audio, timestamp);
    crate::preempt::set_need_resched();
}

/// Are completions pending? Lock-free — fd readiness, io_uring poll and the
/// scheduler's park-time recheck all ask this.
pub fn has_pending() -> bool {
    ISR.mask.load(Ordering::Acquire) != 0
}

static IO_URING_WATCHERS: Lock<alloc::vec::Vec<crate::io_uring::RingId>> =
    Lock::new(alloc::vec::Vec::new());

pub fn add_io_uring_watcher(id: crate::io_uring::RingId) {
    let mut watchers = IO_URING_WATCHERS.lock();
    if !watchers.contains(&id) {
        watchers.push(id);
    }
}

pub fn remove_io_uring_watcher(id: crate::io_uring::RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

pub fn io_uring_watchers() -> alloc::vec::Vec<crate::io_uring::RingId> {
    IO_URING_WATCHERS.lock().clone()
}

/// Take the pending completions, or `None`.
///
/// The timestamp is read after the mask is taken, so it is never older than the
/// newest period in it. A handler landing in the window between the two makes
/// the reported batch look microseconds late and leaves its own bits for the
/// next read; nothing is lost and nothing is double-counted.
fn take_completions() -> Option<AudioCompletionRecord> {
    let mask = ISR.mask.swap(0, Ordering::AcqRel);
    if mask == 0 {
        return None;
    }
    Some(AudioCompletionRecord {
        mask,
        _pad: 0,
        timestamp_nanos: ISR.timestamp.load(Ordering::Acquire),
    })
}

/// Copy one completion record into `buf`, and name a device error the first
/// time one has been counted.
pub fn drain_completed(buf: &mut crate::user_ptr::UserBytesMut) -> usize {
    let errors = ISR.errors.load(Ordering::Relaxed);
    if errors != 0 && !ISR.named_error.swap(true, Ordering::Relaxed) {
        log!("hda: the stream reported a FIFO or descriptor error ({errors} so far)");
    }
    let Some(record) = take_completions() else { return 0 };
    let mut bytes = [0u8; AudioCompletionRecord::SIZE];
    bytes[0..4].copy_from_slice(&record.mask.to_le_bytes());
    bytes[8..16].copy_from_slice(&record.timestamp_nanos.to_le_bytes());
    buf.write_at(0, &bytes);
    AudioCompletionRecord::SIZE
}

// --- the controller ---

struct HdaController {
    regs: Mmio,
    stream: Mmio,
    /// Kept so the pages outlive the mappings that name them.
    _bdl: super::DmaPool,
    _pcm: super::DmaPool,
}

static CONTROLLER: Lock<Option<HdaController>> = Lock::new(None);
static INFO: Lock<Option<HdaInfo>> = Lock::new(None);
static REFUSALS: AtomicUsize = AtomicUsize::new(0);

pub fn info() -> Option<HdaInfo> {
    *INFO.lock()
}

// --- the allow-list ---

/// Why a register is on the write list.
///
/// Every entry carries the same property — **its value is not an address, and
/// it indexes nothing the kernel allocated.** The two registers that would fail
/// that test, `SDnCBL` and `SDnLVI`, are absent because the kernel writes them
/// itself from the descriptor list it built: an `SDnLVI` past the list's end is
/// a DMA engine reading buffer descriptors out of memory nobody initialised.
///
/// The polarity is the guarantee. A missing entry costs a driver that cannot
/// bring its stream up and says so; a refusal list missing an entry costs a
/// device pointed at kernel memory.
fn write_permit(stream_offset: u64, offset: u64, width: RegWidth) -> Result<&'static str, ()> {
    let sd = |field: u64| stream_offset + field;
    match (offset, width) {
        (IMMEDIATE_COMMAND, RegWidth::U32) => Ok("ICW: a codec verb, which names no memory"),
        (IMMEDIATE_STATUS, RegWidth::U16) => Ok("ICS: the immediate-command busy and valid bits"),
        (o, RegWidth::U8) if o == sd(SD_CTL) => Ok("SDnCTL: run and interrupt enables"),
        (o, RegWidth::U8) if o == sd(SD_CTL_TAG) => Ok("SDnCTL: the stream tag"),
        (o, RegWidth::U16) if o == sd(SD_FMT) => Ok("SDnFMT: the format word"),
        _ => Err(()),
    }
}

/// The two registers a driver has to poll to get an answer out of a codec.
///
/// Everything else it would read is either in [`HdaInfo`] or is the kernel's:
/// `SDnLPIB` is read in the handler and reaches the driver as a completion
/// mask, and `SDnSTS` is the acknowledgement, which from userland would be an
/// interrupt left asserted across a scheduling round trip.
fn read_permit(offset: u64, width: RegWidth) -> Result<&'static str, ()> {
    match (offset, width) {
        (IMMEDIATE_STATUS, RegWidth::U16) => Ok("ICS"),
        (IMMEDIATE_RESPONSE, RegWidth::U32) => Ok("IRR"),
        _ => Err(()),
    }
}

fn refuse(what: &str, offset: u64, width: RegWidth) -> SyscallError {
    if REFUSALS.fetch_add(1, Ordering::Relaxed) < MAX_NAMED_REFUSALS {
        log!(
            "hda: refused a {width:?} {what} of {offset:#x} — not on the allow-list \
             (hda-driver-plan.md §4.1.3)"
        );
    }
    SyscallError::PermissionDenied
}

pub fn reg_read(offset: u64, width: RegWidth) -> Result<u32, SyscallError> {
    if read_permit(offset, width).is_err() {
        return Err(refuse("read", offset, width));
    }
    let guard = CONTROLLER.lock();
    let controller = guard.as_ref().ok_or(SyscallError::NotFound)?;
    Ok(match width {
        RegWidth::U8 => controller.regs.read_u8(offset) as u32,
        RegWidth::U16 => controller.regs.read_u16(offset) as u32,
        RegWidth::U32 => controller.regs.read_u32(offset),
    })
}

pub fn reg_write(offset: u64, width: RegWidth, value: u32) -> Result<(), SyscallError> {
    let stream_offset = info().ok_or(SyscallError::NotFound)?.stream_offset as u64;
    if write_permit(stream_offset, offset, width).is_err() {
        return Err(refuse("write", offset, width));
    }
    if value > width.max_value() {
        return Err(SyscallError::InvalidArgument);
    }

    let guard = CONTROLLER.lock();
    let controller = guard.as_ref().ok_or(SyscallError::NotFound)?;

    if offset == stream_offset + SD_CTL {
        return start_stop(controller, value as u8);
    }
    match width {
        RegWidth::U8 => controller.regs.write_u8(offset, value as u8),
        RegWidth::U16 => controller.regs.write_u16(offset, value as u16),
        RegWidth::U32 => controller.regs.write_u32(offset, value),
    }
    Ok(())
}

/// `SDnCTL`'s first byte, which is where the driver starts and stops the engine.
///
/// Two things happen here that a plain store would not do, and both are
/// bookkeeping rather than policy:
///
/// **`SRST` is refused.** Entering stream reset clears the descriptor's address
/// and length registers, and the driver cannot write them back — so a stream
/// reset from userland is a `RUN` away from a DMA engine pointed at physical
/// address zero.
///
/// **The handler's position is re-anchored on the edge that starts the engine.**
/// `SDnLPIB` does not restart at zero when a stopped stream is started again, so
/// the period index the mask is derived from has to be read from the device at
/// the one instant nothing is advancing it.
fn start_stop(controller: &HdaController, value: u8) -> Result<(), SyscallError> {
    if value & SD_CTL_SRST != 0 {
        if REFUSALS.fetch_add(1, Ordering::Relaxed) < MAX_NAMED_REFUSALS {
            log!(
                "hda: refused SDnCTL {value:#04x} — stream reset clears the buffer descriptor \
                 address, which is the kernel's (hda-driver-plan.md §4.1.3)"
            );
        }
        return Err(SyscallError::PermissionDenied);
    }
    let running = controller.stream.read_u8(SD_CTL) & SD_CTL_RUN != 0;
    if value & SD_CTL_RUN != 0 && !running {
        let position = controller.stream.read_u32(SD_LPIB);
        ISR.last.store(position as usize / PERIOD_BYTES % PERIODS, Ordering::Relaxed);
        ISR.mask.store(0, Ordering::Relaxed);
    }
    controller.stream.write_u8(SD_CTL, value);
    Ok(())
}

// --- bring-up ---

/// Bring up the one HDA controller this machine has a codec behind.
///
/// **Every class-0403 function is taken out of reset and asked**, and exactly
/// one live link is bound: zero is a machine with no HDA audio and more than one
/// is a refusal naming every controller found. Choosing between two live links
/// would mean walking their codec graphs, which is the driver's work and the
/// thing this file must not do — and a first match is the defect `pci.rs`
/// records one layer down.
pub fn init(devices: &[PciDevice]) {
    let controllers: alloc::vec::Vec<&PciDevice> = devices
        .iter()
        .filter(|d| d.matches_class(CLASS_MULTIMEDIA, SUBCLASS_HDA, None))
        .collect();
    if controllers.is_empty() {
        return;
    }

    let mut live: alloc::vec::Vec<(&PciDevice, Mmio, u16, u16)> = alloc::vec::Vec::new();
    for pci in &controllers {
        match probe(pci) {
            Some((regs, gcap, statests)) => live.push((pci, regs, gcap, statests)),
            None => {}
        }
    }

    let (pci, regs, gcap, statests) = match live.len() {
        0 => {
            log!(
                "hda: {} class 0403 function(s) and no codec on any link — no HDA audio on this \
                 machine",
                controllers.len()
            );
            return;
        }
        1 => live.remove(0),
        n => {
            for (pci, _, _, statests) in &live {
                log!(
                    "hda: {:02x}:{:02x}.{} has a live link (statests={statests:#06x})",
                    pci.bus,
                    pci.dev,
                    pci.func
                );
            }
            log!(
                "hda: {n} controllers answer on this machine and choosing between them means \
                 walking their codec graphs, which is the driver's — refused by name, no HDA audio"
            );
            return;
        }
    };

    let output_streams = (gcap >> 12) & 0xF;
    let input_streams = (gcap >> 8) & 0xF;
    if output_streams == 0 {
        log!(
            "hda: {:02x}:{:02x}.{} reports no output stream descriptor (gcap={gcap:#06x}) — \
             refused",
            pci.bus,
            pci.dev,
            pci.func
        );
        return;
    }

    let stream_index = input_streams as u8;
    let stream_offset = STREAM_BASE + stream_index as u64 * STREAM_STRIDE;
    let stream = regs.subregion(stream_offset, STREAM_STRIDE);

    let bdl = super::DmaPool::alloc(PERIODS * 16);
    let pcm = super::DmaPool::alloc(PERIODS * PERIOD_BYTES);
    let bdl_slice = bdl.slice();
    let pcm_slice = pcm.slice();
    unsafe {
        bdl_slice.zero();
        pcm_slice.zero();
    }

    let entries = stream::build_bdl(pcm_slice.phys(), PERIOD_BYTES as u32, PERIODS)
        .expect("hda: the pipeline's own shape builds a descriptor list");
    for (i, entry) in entries.iter().enumerate() {
        unsafe {
            bdl_slice.write::<u64>(i * 16, entry.address);
            bdl_slice.write::<u32>(i * 16 + 8, entry.length);
            bdl_slice.write::<u32>(i * 16 + 12, u32::from(entry.interrupt_on_completion));
        }
    }

    if !reset_stream(stream) {
        log!(
            "hda: {:02x}:{:02x}.{} stream {stream_index} never left reset — refused",
            pci.bus,
            pci.dev,
            pci.func
        );
        return;
    }

    stream.write_u32(SD_BDPL, bdl_slice.phys() as u32);
    stream.write_u32(SD_BDPU, (bdl_slice.phys() >> 32) as u32);
    stream.write_u32(
        SD_CBL,
        stream::cyclic_length(PERIOD_BYTES as u32, PERIODS).expect("fits a u32"),
    );
    stream.write_u16(SD_LVI, stream::last_valid_index(PERIODS).expect("a ring of eight") as u16);
    stream.write_u8(SD_CTL_TAG, STREAM_TAG << 4);
    stream.write_u8(SD_CTL, SD_CTL_IOCE | SD_CTL_FEIE | SD_CTL_DEIE);
    stream.write_u8(SD_STS, SD_STS_WRITE_CLEAR);

    // SAFETY: no vector is armed yet, so nothing can observe a half-written
    // Option.
    unsafe {
        *ISR.stream.get() = Some(stream);
    }

    pci.enable_bus_master();
    if !arm_interrupt(pci) {
        return;
    }
    regs.write_u32(INTCTL, INTCTL_GIE | (1 << stream_index));

    let pcm_token = shared_memory::register(
        crate::DirectMap::from_phys(pcm_slice.phys()),
        crate::mm::PAGE_2M,
        CachePolicy::DeferToMtrr,
    );

    *CONTROLLER.lock() = Some(HdaController { regs, stream, _bdl: bdl, _pcm: pcm });
    *INFO.lock() = Some(HdaInfo {
        pcm_token: pcm_token.raw(),
        period_bytes: PERIOD_BYTES as u32,
        stream_offset: stream_offset as u32,
        statests,
        stream_tag: STREAM_TAG,
        periods: PERIODS as u8,
    });

    log!(
        "hda: {:02x}:{:02x}.{} bound, statests={statests:#06x}, output stream {stream_index} at \
         {stream_offset:#x}, tag {STREAM_TAG}, {PERIODS} periods of {PERIOD_BYTES} bytes",
        pci.bus,
        pci.dev,
        pci.func
    );

    #[cfg(feature = "hda-allowlist-selftest")]
    allowlist_selftest(stream_offset);
}

/// Every arm of [`write_permit`] and [`read_permit`], run against the bound
/// controller and reported by name.
///
/// **Nothing else can reach it.** The check is gated on holding the device
/// claim, soundd takes that claim for the life of the boot, and a `Claim` is
/// exclusive by construction — so no guest test can be the caller. The
/// alternative, a boot with no soundd, is a machine with no audio, which
/// answers nothing about a driver's refusals.
///
/// The permitted cases really write: `ICW` takes a null verb nothing has told
/// the controller to send, and `SDnFMT` takes back the word it already holds,
/// so what runs is the shipped path and not a rehearsal of the table.
#[cfg(feature = "hda-allowlist-selftest")]
fn allowlist_selftest(stream_offset: u64) {
    let sd = |field: u64| stream_offset + field;
    let cases: &[(&str, u64, RegWidth, u32)] = &[
        ("ICW", IMMEDIATE_COMMAND, RegWidth::U32, 0),
        ("SDnFMT", sd(SD_FMT), RegWidth::U16, 0),
        ("SDnCTL", sd(SD_CTL), RegWidth::U8, SD_CTL_IOCE as u32),
        ("SDnCTL-tag", sd(SD_CTL_TAG), RegWidth::U8, (STREAM_TAG as u32) << 4),
        // Every one below carries a value the kernel must own.
        ("SDnBDPL", sd(SD_BDPL), RegWidth::U32, 0),
        ("SDnBDPU", sd(SD_BDPU), RegWidth::U32, 0),
        ("SDnCBL", sd(SD_CBL), RegWidth::U32, 0),
        ("SDnLVI", sd(SD_LVI), RegWidth::U16, 0xFF),
        ("SDnSTS", sd(SD_STS), RegWidth::U8, 0),
        ("SDnCTL-srst", sd(SD_CTL), RegWidth::U8, SD_CTL_SRST as u32),
        // A 32-bit write of SDnCTL reaches SDnSTS, which is the interrupt
        // acknowledgement and the kernel's alone.
        ("SDnCTL-wide", sd(SD_CTL), RegWidth::U32, 0),
        ("INTCTL", INTCTL, RegWidth::U32, 0),
        ("GCTL", GCTL, RegWidth::U32, 0),
    ];
    for &(name, offset, width, value) in cases {
        let verdict = match reg_write(offset, width, value) {
            Ok(()) => "written",
            Err(_) => "refused",
        };
        log!("hda: selftest write {name} {verdict}");
    }
    for (name, offset, width) in [
        ("ICS", IMMEDIATE_STATUS, RegWidth::U16),
        ("IRR", IMMEDIATE_RESPONSE, RegWidth::U32),
        ("SDnLPIB", sd(SD_LPIB), RegWidth::U32),
        ("STATESTS", STATESTS, RegWidth::U16),
    ] {
        let verdict = match reg_read(offset, width) {
            Ok(_) => "read",
            Err(_) => "refused",
        };
        log!("hda: selftest read {name} {verdict}");
    }
}

/// Take one controller out of reset and ask whether anything is on its link.
///
/// `None` for every way a function can fail to be one: an I/O BAR, an
/// unassigned or undersized window, a register window that answers all ones, a
/// controller that will not reset, and a link with no codec on it. Each says
/// which, because on the machine this targets `STATESTS` reading zero is the
/// answer that ends the whole plan (§6.3) and it must not look like a driver
/// that gave up.
fn probe(pci: &PciDevice) -> Option<(Mmio, u16, u16)> {
    let low = pci.read_config_u32(HEADER_BAR0);
    if low & 1 != 0 {
        log!("hda: {:02x}:{:02x}.{} bar0 is an I/O BAR — refused", pci.bus, pci.dev, pci.func);
        return None;
    }
    let wide = (low >> 1) & 0x3 == 2;
    let high = if wide { pci.read_config_u32(HEADER_BAR0 + 4) } else { 0 };
    let base = ((high as u64) << 32) | (low as u64 & !0xF);
    if base == 0 {
        log!(
            "hda: {:02x}:{:02x}.{} bar0 is unassigned — firmware gave it no register window",
            pci.bus,
            pci.dev,
            pci.func
        );
        return None;
    }

    power_up(pci);
    let command = pci.read_config_u16(HEADER_COMMAND);
    pci.write_config_u16(HEADER_COMMAND, command | COMMAND_MEMORY_SPACE);

    let regs = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(
        base,
        MIN_BAR_BYTES * 4,
        CachePolicy::DeferToMtrr,
    );

    let gcap = regs.read_u16(GCAP);
    if gcap == u16::MAX {
        log!(
            "hda: {:02x}:{:02x}.{} register window at {base:#x} answers all ones",
            pci.bus,
            pci.dev,
            pci.func
        );
        return None;
    }

    if !reset_controller(regs) {
        log!(
            "hda: {:02x}:{:02x}.{} never left reset (gctl={:#010x}) — refused",
            pci.bus,
            pci.dev,
            pci.func,
            regs.read_u32(GCTL)
        );
        return None;
    }

    let statests = regs.read_u16(STATESTS);
    log!(
        "hda: {:02x}:{:02x}.{} {:04x}:{:04x} version {}.{} gcap={gcap:#06x} statests={statests:#06x}",
        pci.bus,
        pci.dev,
        pci.func,
        pci.vendor_id(),
        pci.device_id(),
        regs.read_u8(VMAJ),
        regs.read_u8(VMIN),
    );
    if statests == 0 {
        log!(
            "hda: {:02x}:{:02x}.{} statests=0x0000 — nothing on the legacy link",
            pci.bus,
            pci.dev,
            pci.func
        );
        return None;
    }
    Some((regs, gcap, statests))
}

/// Put the function in D0 if firmware left it lower. A function in D3hot
/// answers every register read with all ones, which is indistinguishable from a
/// controller that is not there.
fn power_up(pci: &PciDevice) {
    let Some(cap) = pci.capabilities().find(|c| c.id() == CAP_POWER_MANAGEMENT) else {
        return;
    };
    let pmcsr = cap.read_u16(PM_CONTROL_STATUS);
    if pmcsr & PM_STATE_MASK == 0 {
        return;
    }
    cap.write_u16(PM_CONTROL_STATUS, pmcsr & !PM_STATE_MASK);
    spin_ns(PM_D3HOT_RECOVERY_NS);
}

/// Hold the controller in reset and release it, waiting for both edges: the bit
/// reads back only once the controller has acted on it, so a write that is not
/// read back is a controller that is not there — and `STATESTS` read off one
/// would report a codec that does not exist.
fn reset_controller(regs: Mmio) -> bool {
    regs.write_u32(GCTL, 0);
    if !settles(|| regs.read_u32(GCTL) & GCTL_CRST == 0) {
        return false;
    }
    regs.write_u32(GCTL, GCTL_CRST);
    if !settles(|| regs.read_u32(GCTL) & GCTL_CRST != 0) {
        return false;
    }
    spin_ns(CODEC_DETECT_NS);
    true
}

/// The descriptor's own reset, which is what clears its address and length
/// registers before the kernel writes the ones it means.
fn reset_stream(stream: Mmio) -> bool {
    stream.write_u8(SD_CTL, SD_CTL_SRST);
    if !settles(|| stream.read_u8(SD_CTL) & SD_CTL_SRST != 0) {
        return false;
    }
    stream.write_u8(SD_CTL, 0);
    settles(|| stream.read_u8(SD_CTL) & SD_CTL_SRST == 0)
}

/// Arm the completion interrupt, or say why this machine has no HDA audio.
///
/// A refusal rather than a panic: a controller that cannot be told a period
/// completed is one whose every period stays in flight forever, and a machine
/// that boots and plays nothing is better than one that dies over a peripheral.
fn arm_interrupt(pci: &PciDevice) -> bool {
    let vector = crate::arch::idt::HDA_VECTOR;
    if pci.enable_msix(vector) || pci.enable_msi(vector) {
        return true;
    }
    log!(
        "hda: {:02x}:{:02x}.{} offers neither MSI-X nor MSI, and this driver has no other way to \
         be told a period completed — NOT INITIALISED",
        pci.bus,
        pci.dev,
        pci.func
    );
    false
}

fn settles(ready: impl Fn() -> bool) -> bool {
    let deadline = crate::clock::nanos_since_boot() + SETTLE_NS;
    while !ready() {
        if crate::clock::nanos_since_boot() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn spin_ns(duration: u64) {
    let deadline = crate::clock::nanos_since_boot() + duration;
    while crate::clock::nanos_since_boot() < deadline {
        core::hint::spin_loop();
    }
}
