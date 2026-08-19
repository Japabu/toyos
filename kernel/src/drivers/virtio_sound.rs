//! The virtio-sound stub: bring-up, the virtqueues, and the allow-list.
//!
//! **The line is who writes an address**, and this device is the second to take
//! that shape after `hda.rs`. A split virtqueue puts every address a virtio
//! driver ever programs in one place: the descriptor table. So the three tables
//! here live in a page no process maps,
//! their chains are built once at bind out of offsets into a region the kernel
//! allocated, and what the driver gets is the avail rings that select a chain by
//! index, the used rings that say one came back, and one register write to ring
//! the doorbell.
//!
//! Nothing here decides. Which stream, at what rate, in what format, when a
//! period is published and when the stream runs are soundd's, and every one of
//! them is a message the driver writes into a buffer of its own.
//!
//! Structure layouts and command codes come from the VirtIO 1.2 specification
//! §5.14; the ones the kernel needs are the transfer header's size and nothing
//! else, because the kernel never reads a response.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use toyos_abi::audio::AudioCompletionRecord;
use toyos_abi::syscall::{RegWidth, SyscallError};
use toyos_abi::virtio_sound as abi;

use super::pci::{PciDevice, MSIX_ENTRY};
use super::virtio::{BufDir, UsedRingConsumer, VirtioDevice, Virtqueue, VirtqueueRegions,
                    VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::log;
use crate::mm::paging::CachePolicy;
use crate::mm::{KernelSlice, Mmio};
use crate::object::shm::Region;
use crate::sync::Lock;

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_SND_DEVICE: u16 = 0x1059; // 0x1040 + device_id 25

/// The transfer header that leads every TX chain: one `le32` stream id, written
/// by the driver into a buffer of its own. The kernel needs its *size* to build
/// the chain and never its contents — QEMU derives the PCM byte count from the
/// chain's total readable length minus this.
const XFER_HEADER_BYTES: u32 = 4;
/// The per-period status the device writes back: status and latency, two `le32`.
const STATUS_BYTES: u32 = 8;
/// One event: a code and its data.
const EVENT_BYTES: u32 = 8;

/// The kernel-only DMA page: the three descriptor tables, and the TX used ring
/// that only the interrupt handler consumes.
const OFF_CTRL_DESC: usize = 0x0000;
const OFF_EVENT_DESC: usize = 0x0400;
const OFF_TX_DESC: usize = 0x0800;
const OFF_TX_USED: usize = 0x0C00;
const KERNEL_DMA_BYTES: usize = 0x1000;

const _: () = {
    use super::virtio::DESC_BYTES;
    assert!(abi::CONTROL_QUEUE_SIZE as usize * DESC_BYTES <= OFF_EVENT_DESC - OFF_CTRL_DESC);
    assert!(abi::EVENT_QUEUE_SIZE as usize * DESC_BYTES <= OFF_TX_DESC - OFF_EVENT_DESC);
    assert!(abi::TX_QUEUE_SIZE as usize * DESC_BYTES <= OFF_TX_USED - OFF_TX_DESC);
    assert!(abi::used_bytes(abi::TX_QUEUE_SIZE) <= KERNEL_DMA_BYTES - OFF_TX_USED);
};

/// How many refused register accesses are named before the driver is told to
/// stop asking. Policy, and the same one the HDA stub carries: a refusal is a
/// driver bug worth reading, and an unbounded one is a userland process choosing
/// how much log the machine spends.
const MAX_NAMED_REFUSALS: usize = 16;

// --- the interrupt handler ---

/// The handler's whole view of the device.
///
/// Written once, before the vector is armed, and read with no lock afterwards:
/// the handler may interrupt a CPU holding [`CONTROLLER`].
struct TxIsr {
    consumer: UnsafeCell<Option<UsedRingConsumer>>,
    /// Used-ring entries naming a descriptor that heads no chain. Counted here
    /// and named once from the drain path — the avail ring is the driver's, so
    /// this is a userland bug and a handler that logs is a handler that produces
    /// work for the thing that failed.
    stray: AtomicU32,
    named_stray: AtomicBool,
}

// SAFETY: `consumer` is written once at init before the vector can fire and is
// read only by the handler afterwards; every other field is atomic.
unsafe impl Sync for TxIsr {}

static TX_ISR: TxIsr = TxIsr {
    consumer: UnsafeCell::new(None),
    stray: AtomicU32::new(0),
    named_stray: AtomicBool::new(false),
};

/// Drain the TX used ring and return the periods it completed.
///
/// A used entry names the head descriptor the driver published, and the driver
/// publishes an index — so a head that is not a chain's is untrusted input, not
/// a device fault, and is counted rather than asserted on.
fn drain_tx() -> u32 {
    // SAFETY: sole accessor after init — see `TxIsr`.
    let consumer = unsafe { &mut *TX_ISR.consumer.get() };
    // A configuration-change interrupt shares this vector and can arrive before
    // init has installed the consumer.
    let Some(consumer) = consumer.as_mut() else { return 0 };
    let mut mask = 0u32;
    while let Some(head) = consumer.poll() {
        let idx = head as usize / abi::TX_CHAIN as usize;
        if idx >= abi::PERIODS || head % abi::TX_CHAIN != 0 {
            TX_ISR.stray.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        mask |= 1 << idx;
    }
    mask
}

/// Rust half of the MSI-X handler, called from the IDT entry.
pub fn isr_complete() {
    // Timestamp first — this is the hardware-completion time the DLL feeds on.
    let timestamp = crate::clock::nanos_since_boot();
    let mask = drain_tx();
    if mask == 0 {
        return;
    }
    isr_push_completion(mask, timestamp);
    crate::irq_ring::isr_publish(crate::irq_ring::IrqSource::Audio, timestamp);
    // Force a scheduler entry on IRQ return so drain_irqs converts the record
    // into wakes now, not at the next 10ms quantum tick.
    crate::preempt::set_need_resched();
}

// --- the completion record ring ---

const RECORD_RING_CAP: u32 = 16;

/// SPSC ring of completion records. Producer is the MSI-X handler (single CPU,
/// IF=0 — never concurrent with itself); consumer is whoever holds
/// [`CONTROLLER`] in [`drain_completed`]. Indices are free-running u32s.
///
/// One record per interrupt, and not one accumulating mask: soundd's DLL
/// measures a batch against its own grid point, so folding two interrupts into
/// one record would hand it a lateness it never saw. The HDA stub accumulates
/// because its position read makes a second interrupt carry nothing a later one
/// does not; a used ring is not that.
///
/// Occupancy is bounded by the period count while the driver behaves: pending
/// records carry pairwise-disjoint masks, because a period's bit re-enters one
/// only after its chain has been republished, and the driver republishes only
/// what a record it has read told it was free. **But the driver is the one
/// publishing**, so that bound is userland's to keep and this used to be an
/// assertion a process could ask the kernel to fail. What a full ring costs
/// instead is [`SPILL`] — exactly what the HDA stub costs always.
struct RecordRing {
    slots: [UnsafeCell<AudioCompletionRecord>; RECORD_RING_CAP as usize],
    head: AtomicU32,
    tail: AtomicU32,
}

const _: () = assert!(
    RECORD_RING_CAP as usize >= abi::PERIODS,
    "record ring must hold one record per period"
);

// SAFETY: slot access is arbitrated by the head/tail protocol above.
unsafe impl Sync for RecordRing {}

static RECORDS: RecordRing = RecordRing {
    slots: [const {
        UnsafeCell::new(AudioCompletionRecord { mask: 0, _pad: 0, timestamp_nanos: 0 })
    }; RECORD_RING_CAP as usize],
    head: AtomicU32::new(0),
    tail: AtomicU32::new(0),
};

/// Where a completion goes when the ring is full: one mask and the newest
/// timestamp, emitted after everything already queued.
///
/// It cannot be lost and it cannot grow — a mask is eight bits and OR is
/// idempotent — so a driver that stops reading its completions costs itself
/// timestamp granularity and nothing else costs anything.
struct Spill {
    mask: AtomicU32,
    timestamp: AtomicU64,
}

static SPILL: Spill = Spill { mask: AtomicU32::new(0), timestamp: AtomicU64::new(0) };

fn isr_push_completion(mask: u32, timestamp_nanos: u64) {
    let ring = &RECORDS;
    let head = ring.head.load(Ordering::Relaxed); // sole writer of head
    let tail = ring.tail.load(Ordering::Acquire);
    if head.wrapping_sub(tail) >= RECORD_RING_CAP {
        SPILL.timestamp.store(timestamp_nanos, Ordering::Relaxed);
        SPILL.mask.fetch_or(mask, Ordering::Release);
        return;
    }
    let slot = (head % RECORD_RING_CAP) as usize;
    // SAFETY: slot is outside [tail, head) — not visible to the consumer.
    unsafe {
        *ring.slots[slot].get() = AudioCompletionRecord { mask, _pad: 0, timestamp_nanos };
    }
    // Release: publish the record contents before the consumer can observe the
    // new head.
    ring.head.store(head.wrapping_add(1), Ordering::Release);
}

/// Pop the oldest pending record. Called under [`CONTROLLER`], which serializes
/// consumers — the tail store needs no CAS.
///
/// The spill comes last because it is the newest: it exists only for interrupts
/// that arrived after everything the ring holds.
fn pop_completion() -> Option<AudioCompletionRecord> {
    let ring = &RECORDS;
    let tail = ring.tail.load(Ordering::Relaxed); // sole writer of tail
    // Acquire pairs with the producer's Release head store: the record contents
    // are visible before head covers them.
    let head = ring.head.load(Ordering::Acquire);
    if head == tail {
        let mask = SPILL.mask.swap(0, Ordering::AcqRel);
        if mask == 0 {
            return None;
        }
        return Some(AudioCompletionRecord {
            mask,
            _pad: 0,
            timestamp_nanos: SPILL.timestamp.load(Ordering::Relaxed),
        });
    }
    let rec = unsafe { *ring.slots[(tail % RECORD_RING_CAP) as usize].get() };
    ring.tail.store(tail.wrapping_add(1), Ordering::Release);
    Some(rec)
}

/// Readiness: are completion records pending? Lock-free — fd readiness,
/// io_uring poll and the scheduler's park-time recheck all ask this.
pub fn has_pending() -> bool {
    RECORDS.head.load(Ordering::Acquire) != RECORDS.tail.load(Ordering::Acquire)
        || SPILL.mask.load(Ordering::Acquire) != 0
}

/// Copy up to `buf.len() / 16` pending records into `buf`, oldest first, and
/// name a stray completion the first time one has been counted.
pub fn drain_completed(buf: &mut crate::user_ptr::UserBytesMut) -> usize {
    let stray = TX_ISR.stray.load(Ordering::Relaxed);
    if stray != 0 && !TX_ISR.named_stray.swap(true, Ordering::Relaxed) {
        log!(
            "virtio-sound: the device completed a chain this driver never built ({stray} so far) \
             — its avail ring names a descriptor that heads none"
        );
    }
    let max = buf.len() / AudioCompletionRecord::SIZE;
    let _guard = CONTROLLER.lock();
    let mut written = 0;
    for _ in 0..max {
        let Some(rec) = pop_completion() else { break };
        // Field-wise serialization — never expose struct padding.
        let mut record = [0u8; AudioCompletionRecord::SIZE];
        record[0..4].copy_from_slice(&rec.mask.to_le_bytes());
        record[8..16].copy_from_slice(&rec.timestamp_nanos.to_le_bytes());
        buf.write_at(written, &record);
        written += AudioCompletionRecord::SIZE;
    }
    written
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

// --- the controller ---

/// What the bring-up leaves behind: the notification region the driver's three
/// doorbells are in, and the pages every descriptor points into.
///
/// The virtqueues are not here because after [`build_chains`] there is nothing
/// left to do with one — their descriptors are written, their addresses are in
/// the device, and their used rings are consumed by the handler and by the
/// driver.
struct Bound {
    notify: Mmio,
    _dma_kernel: DmaPool,
    _dma_shared: DmaPool,
}

static CONTROLLER: Lock<Option<Bound>> = Lock::new(None);
static INFO: Lock<Option<(abi::VirtioSoundInfo, Region)>> = Lock::new(None);
static REFUSALS: AtomicU32 = AtomicU32::new(0);

pub fn info() -> Option<(abi::VirtioSoundInfo, Region)> {
    INFO.lock().clone()
}

// --- the allow-list ---

/// The driver's whole write surface: three doorbells, one per queue.
///
/// Every entry carries the same property the HDA stub's do — **its value is not
/// an address, and it indexes nothing the kernel allocated.** A doorbell takes a
/// queue index, and which queue is already decided by which of the three offsets
/// was named, so the value cannot reach anything the offset did not.
///
/// The polarity is the guarantee. A missing entry costs a driver that cannot
/// notify a queue and says so; a refusal list missing an entry costs a device
/// pointed at kernel memory.
fn write_permit(info: &abi::VirtioSoundInfo, offset: u64, width: RegWidth) -> bool {
    let Ok(offset) = u32::try_from(offset) else { return false };
    width == RegWidth::U16
        && [info.notify_control, info.notify_event, info.notify_tx].contains(&offset)
}

fn refuse(what: &str, offset: u64, width: RegWidth) -> SyscallError {
    if REFUSALS.fetch_add(1, Ordering::Relaxed) < MAX_NAMED_REFUSALS as u32 {
        log!("virtio-sound: refused a {width:?} {what} of {offset:#x} — not on the allow-list");
    }
    SyscallError::PermissionDenied
}

/// **This driver reads no register at all**, so the read list is empty and every
/// call is a refusal. The device's answers reach it through memory: the used
/// rings it polls and the completion records the handler pushes.
pub fn reg_read(offset: u64, width: RegWidth) -> Result<u32, SyscallError> {
    Err(refuse("read", offset, width))
}

pub fn reg_write(offset: u64, width: RegWidth, value: u32) -> Result<(), SyscallError> {
    let (info, _) = info().ok_or(SyscallError::NotFound)?;
    if !write_permit(&info, offset, width) {
        return Err(refuse("write", offset, width));
    }
    if value > width.max_value() {
        return Err(SyscallError::InvalidArgument);
    }
    let guard = CONTROLLER.lock();
    let controller = guard.as_ref().ok_or(SyscallError::NotFound)?;
    controller.notify.write_u16(offset, value as u16);
    Ok(())
}

// --- bring-up ---

/// Bring up the machine's virtio-sound device, or leave it unclaimed and say
/// why.
///
/// A refusal rather than a panic throughout: audio is optional, so a machine
/// that boots and plays nothing is better than one that dies over a peripheral.
/// [`info`] then answers `None`, the claim is `Absent`, and soundd falls back to
/// the null sink.
pub fn init(devices: &[PciDevice]) {
    let Some(pci) = devices.iter().find(|d| d.is_id(VIRTIO_VENDOR, VIRTIO_SND_DEVICE)) else {
        return;
    };
    log!("virtio-sound: found at PCI {:02x}:{:02x}.{}", pci.bus, pci.dev, pci.func);

    let dma_kernel = DmaPool::alloc(KERNEL_DMA_BYTES);
    let dma_shared = DmaPool::alloc(abi::SHARED_BYTES);
    let kernel_mem = dma_kernel.slice();
    let shared = dma_shared.slice();
    unsafe { shared.zero() };

    let device = VirtioDevice::init(pci, VIRTIO_F_VERSION_1);

    let cfg = device.device_config();
    let (jacks, streams, chmaps) = (cfg.read_u32(0), cfg.read_u32(4), cfg.read_u32(8));
    log!("virtio-sound: {jacks} jacks, {streams} streams, {chmaps} chmaps");
    if streams == 0 {
        log!("virtio-sound: NOT INITIALISED — the device offers no PCM stream to play into");
        return;
    }

    let mut controlq = queue(
        kernel_mem.subslice(OFF_CTRL_DESC, OFF_EVENT_DESC - OFF_CTRL_DESC),
        shared.subslice(abi::OFF_CTRL_AVAIL, abi::avail_bytes(abi::CONTROL_QUEUE_SIZE)),
        shared.subslice(abi::OFF_CTRL_USED, abi::used_bytes(abi::CONTROL_QUEUE_SIZE)),
        abi::CONTROL_QUEUE_SIZE,
    );
    let mut eventq = queue(
        kernel_mem.subslice(OFF_EVENT_DESC, OFF_TX_DESC - OFF_EVENT_DESC),
        shared.subslice(abi::OFF_EVENT_AVAIL, abi::avail_bytes(abi::EVENT_QUEUE_SIZE)),
        shared.subslice(abi::OFF_EVENT_USED, abi::used_bytes(abi::EVENT_QUEUE_SIZE)),
        abi::EVENT_QUEUE_SIZE,
    );
    // The one used ring the driver never sees: the handler is its only consumer,
    // and a mask derived from a ring userland can rewrite would be a completion
    // for a period that never played.
    let mut txq = queue(
        kernel_mem.subslice(OFF_TX_DESC, OFF_TX_USED - OFF_TX_DESC),
        shared.subslice(abi::OFF_TX_AVAIL, abi::avail_bytes(abi::TX_QUEUE_SIZE)),
        kernel_mem.subslice(OFF_TX_USED, abi::used_bytes(abi::TX_QUEUE_SIZE)),
        abi::TX_QUEUE_SIZE,
    );

    build_chains(&controlq, &eventq, &txq, shared.phys());

    // Install the used-ring consumer before the vector can fire, so no interrupt
    // observes a half-written Option — configuration-change interrupts share it.
    // SAFETY: MSI-X is not enabled yet.
    unsafe { *TX_ISR.consumer.get() = Some(txq.split_used_consumer()) };

    device.setup_queue(abi::CONTROL_QUEUE, &mut controlq);
    device.setup_queue(abi::EVENT_QUEUE, &mut eventq);
    device.setup_queue(abi::TX_QUEUE, &mut txq);
    if !arm_interrupt(pci, &device) {
        return;
    }
    device.enable_queue(abi::CONTROL_QUEUE);
    device.enable_queue(abi::EVENT_QUEUE);
    device.enable_queue(abi::TX_QUEUE);
    device.activate();

    // `DmaPool` allocations are whole 2 MiB pages, so the slice base is the page
    // the driver maps and the ABI's offsets are relative to it.
    let dma_region = Region {
        phys: crate::DirectMap::from_phys(shared.phys()),
        size: crate::mm::PAGE_2M,
        cache: CachePolicy::DeferToMtrr,
        pages: None,
    };
    let multiplier = device.notify_off_multiplier();
    let info = abi::VirtioSoundInfo {
        dma: toyos_abi::HANDLE_INVALID,
        notify_control: controlq.notify_bytes(multiplier) as u32,
        notify_event: eventq.notify_bytes(multiplier) as u32,
        notify_tx: txq.notify_bytes(multiplier) as u32,
        jacks,
        streams,
        chmaps,
    };

    *CONTROLLER.lock() = Some(Bound {
        notify: device.notify_mmio(),
        _dma_kernel: dma_kernel,
        _dma_shared: dma_shared,
    });
    *INFO.lock() = Some((info, dma_region));

    log!(
        "virtio-sound: bound, {} periods of {} bytes, doorbells at {:#x}/{:#x}/{:#x}",
        abi::PERIODS,
        abi::PERIOD_BYTES,
        info.notify_control,
        info.notify_event,
        info.notify_tx
    );
}

fn queue(desc: KernelSlice, avail: KernelSlice, used: KernelSlice, size: u16) -> Virtqueue {
    Virtqueue::from_regions(&VirtqueueRegions::from_separate(desc, avail, used, size), size)
}

/// Every chain the driver will ever publish, built once out of offsets into the
/// shared region.
///
/// This is where the boundary is: after this runs there is no descriptor left to
/// write, so the driver's whole vocabulary is an index into an avail ring and a
/// doorbell. One control chain serves every command because the device reads the
/// header first and takes only what that command defines.
fn build_chains(controlq: &Virtqueue, eventq: &Virtqueue, txq: &Virtqueue, base: u64) {
    let at = |offset: usize| base + offset as u64;

    controlq.write_chain(
        0,
        &[
            (at(abi::OFF_CTRL_REQ), abi::CTRL_BUF_BYTES as u32, BufDir::Readable),
            (at(abi::OFF_CTRL_RESP), abi::CTRL_BUF_BYTES as u32, BufDir::Writable),
        ],
    );

    // One descriptor per buffer, so a buffer index and a descriptor index are
    // the same number and the driver reposts by index.
    for i in 0..abi::EVENT_BUFS {
        eventq.write_chain(
            i as u16,
            &[(at(abi::OFF_EVENT_BUFS + i * abi::EVENT_BUF_STRIDE), EVENT_BYTES, BufDir::Writable)],
        );
    }

    for i in 0..abi::PERIODS {
        txq.write_chain(
            abi::tx_chain_head(i),
            &[
                (at(abi::OFF_TX_XFER + i * abi::XFER_STRIDE), XFER_HEADER_BYTES, BufDir::Readable),
                (at(abi::OFF_PCM + i * abi::PERIOD_BYTES), abi::PERIOD_BYTES as u32, BufDir::Readable),
                (at(abi::OFF_TX_STATUS + i * abi::STATUS_STRIDE), STATUS_BYTES, BufDir::Writable),
            ],
        );
    }
}

/// Arm this device's TX completion interrupt, or say why the machine has no
/// audio.
///
/// A refusal rather than a panic, and one of the reasons is its own: the handler
/// is the only consumer of the TX used ring, so a device that cannot deliver its
/// completions is one whose every period stays in flight forever.
fn arm_interrupt(pci: &PciDevice, device: &VirtioDevice) -> bool {
    let vector = crate::arch::idt::VIRTIO_SOUND_VECTOR;
    if !pci.enable_msix(vector) {
        log!(
            "virtio-sound: NOT INITIALISED at PCI {:02x}:{:02x}.{} — its MSI-X could not be \
             armed and this driver has no other way to be told a period completed",
            pci.bus,
            pci.dev,
            pci.func
        );
        return false;
    }
    if let Err(refused) = device.bind_msix(abi::TX_QUEUE) {
        log!(
            "virtio-sound: NOT INITIALISED at PCI {:02x}:{:02x}.{} — the device refused a vector \
             for {refused}",
            pci.bus,
            pci.dev,
            pci.func
        );
        return false;
    }
    log!("virtio-sound: MSI-X vector {vector:#x} on table entry {MSIX_ENTRY}");
    true
}
