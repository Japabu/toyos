use core::cell::UnsafeCell;
use core::ptr::{copy_nonoverlapping, read_volatile, write_volatile};
use core::sync::atomic::{AtomicU8, Ordering};

use super::pci::{PciDevice, MSIX_ENTRY};
use super::virtio::{BufDir, DescSlot, UsedRingConsumer, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::mm::paging::CachePolicy;
use crate::log;
use crate::shared_memory;
use toyos_abi::audio::AudioInfo;

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_SND_DEVICE: u16 = 0x1059; // 0x1040 + device_id 25

const VIRTIO_SND_R_PCM_INFO: u32 = 0x0100;
const VIRTIO_SND_R_PCM_SET_PARAMS: u32 = 0x0101;
const VIRTIO_SND_R_PCM_PREPARE: u32 = 0x0102;
const VIRTIO_SND_R_PCM_START: u32 = 0x0104;
const VIRTIO_SND_R_PCM_STOP: u32 = 0x0105;

// Event codes (VirtIO 1.2 spec §5.14.6.1)
const VIRTIO_SND_EVT_JACK_CONNECTED: u32 = 0x1000;
const VIRTIO_SND_EVT_JACK_DISCONNECTED: u32 = 0x1001;
const VIRTIO_SND_EVT_PCM_PERIOD_ELAPSED: u32 = 0x1100;
const VIRTIO_SND_EVT_PCM_XRUN: u32 = 0x1101;

const VIRTIO_SND_S_OK: u32 = 0x8000;

// PCM formats (VirtIO 1.2 spec §5.14.6.6)
const VIRTIO_SND_PCM_FMT_S16: u8 = 5;

// PCM rates (VirtIO 1.2 spec §5.14.6.7)
const VIRTIO_SND_PCM_RATE_44100: u8 = 6;
const VIRTIO_SND_PCM_RATE_48000: u8 = 7;

/// The rates this driver can encode, best first. 44100 leads because it is
/// what the mixer, the resampler and the gate's recorded counters are sized
/// against; 48000 is the one every other device offers.
const SUPPORTED_RATES: [(u32, u8); 2] = [
    (44100, VIRTIO_SND_PCM_RATE_44100),
    (48000, VIRTIO_SND_PCM_RATE_48000),
];

fn rate_code(hz: u32) -> Option<u8> {
    let mut i = 0;
    while i < SUPPORTED_RATES.len() {
        if SUPPORTED_RATES[i].0 == hz {
            return Some(SUPPORTED_RATES[i].1);
        }
        i += 1;
    }
    None
}

/// Pick a rate and channel count the device actually advertises.
///
/// The caps were queried, logged, and then ignored: `configure(44100, 2)` ran
/// unconditionally. `None` means the device offers nothing this driver
/// implements — audio is optional, so the machine boots without it rather than
/// dying over a peripheral, but the log has to name the missing capability or
/// the next person is decoding a bitmap by hand on a laptop with no serial.
fn choose_params(info: &VirtioSndPcmInfo) -> Option<(u32, u8)> {
    if info.formats & (1 << VIRTIO_SND_PCM_FMT_S16) == 0 {
        log!("virtio-sound: no usable format — device offers {:#x}, driver needs S16 (bit {})",
            info.formats, VIRTIO_SND_PCM_FMT_S16);
        return None;
    }

    let mut rate = None;
    let mut i = 0;
    while i < SUPPORTED_RATES.len() {
        let (hz, code) = SUPPORTED_RATES[i];
        if info.rates & (1 << code) != 0 {
            rate = Some(hz);
            break;
        }
        i += 1;
    }
    let Some(rate) = rate else {
        log!("virtio-sound: no usable rate — device offers {:#x}, driver needs 44100 (bit {}) or 48000 (bit {})",
            info.rates, VIRTIO_SND_PCM_RATE_44100, VIRTIO_SND_PCM_RATE_48000);
        return None;
    };

    // Stereo if the device takes it; soundd converts either way, so the only
    // unusable case is a device whose minimum is more channels than we mix.
    if info.channels_min > 2 {
        log!("virtio-sound: no usable channel count — device needs at least {}, driver mixes at most 2",
            info.channels_min);
        return None;
    }
    let channels = if info.channels_max >= 2 { 2 } else { info.channels_max };
    if channels == 0 {
        log!("virtio-sound: device advertises a maximum of zero channels");
        return None;
    }

    Some((rate, channels))
}

// Virtqueue indices, fixed by the virtio sound device specification.
const CONTROL_QUEUE: u16 = 0;
const EVENT_QUEUE: u16 = 1;
const TX_QUEUE: u16 = 2;

// Kernel-only DMA page layout (byte offsets). Virtqueue rings, control
// buffers and xfer/status metadata must never be reachable from userspace —
// a process that can rewrite descriptors can point the device at arbitrary
// physical memory.
const OFF_CONTROLQ: usize   = 0x0000;
const OFF_EVENTQ: usize     = 0x1000;
const OFF_TXQ: usize        = 0x2000;
const OFF_CTRL_BUFS: usize  = 0x3000;
const OFF_TX_META: usize    = 0x4000;
const OFF_EVENT_BUFS: usize = 0x5000;
const KERNEL_DMA_SIZE: usize = 0x6000;

// Shared DMA page: PCM data only, granted to whichever process claims
// DeviceType::Audio. AudioInfo.buf_offsets are relative to this page.
const SHARED_DMA_SIZE: usize = TX_INFLIGHT_MAX * PERIOD_BYTES;

const REQ_OFFSET: usize = 0x000;
const RESP_OFFSET: usize = 0x800;

const PERIOD_BYTES: usize = 512;

// VirtIO sound structs (per VirtIO 1.2 spec, section 5.14)

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndHdr {
    code: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndQueryInfo {
    hdr: VirtioSndHdr,
    start_id: u32,
    count: u32,
    size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndPcmInfo {
    hdr: u32, // info header: hda_fn_nid
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
struct VirtioSndPcmSetParams {
    hdr: VirtioSndHdr,
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
struct VirtioSndPcmHdr {
    hdr: VirtioSndHdr,
    stream_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndPcmXfer {
    stream_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndPcmStatus {
    status: u32,
    latency_bytes: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtioSndEvent {
    hdr: VirtioSndHdr,
    data: u32,
}

const TXQ_SIZE: usize = 32;

/// State the MSI-X ISR needs to drain txq completions without locks: the
/// used-ring consumer plus the desc-id → buffer-index map. `consumer` is
/// touched only by the ISR after init (the MSI-X vector targets a single
/// CPU and the handler runs with IF=0, so it is never re-entered);
/// `desc_to_buf` is additionally written by the submit path under the
/// AUDIO lock.
struct TxIsr {
    consumer: UnsafeCell<Option<UsedRingConsumer>>,
    desc_to_buf: [AtomicU8; TXQ_SIZE],
}

// SAFETY: `consumer` is single-threaded by construction (written once at
// init before the MSI-X vector can fire, then ISR-only); `desc_to_buf` is
// atomic.
unsafe impl Sync for TxIsr {}

static TX_ISR: TxIsr = TxIsr {
    consumer: UnsafeCell::new(None),
    desc_to_buf: [const { AtomicU8::new(0) }; TXQ_SIZE],
};

/// Drain the txq used ring from the MSI-X ISR. Returns the bitmask of
/// completed buffer indices. Lock-free: descriptor recycling and
/// inflight-mask clearing need the AUDIO lock, so they happen later on the
/// syscall side (`SoundController::recycle`), driven by the records the
/// ISR pushes.
pub fn isr_drain_tx() -> u32 {
    // SAFETY: sole accessor after init — see TxIsr.
    let consumer = unsafe { &mut *TX_ISR.consumer.get() };
    // A config-change interrupt can arrive on this vector before init has
    // installed the consumer — nothing to drain yet.
    let Some(consumer) = consumer.as_mut() else { return 0 };
    let mut mask = 0u32;
    while let Some(desc_id) = consumer.poll() {
        assert!((desc_id as usize) < TXQ_SIZE, "virtio-sound: bogus used desc id {desc_id}");
        // Relaxed suffices: the submit path stores the mapping before the
        // avail-ring publish (Release fence in Virtqueue::submit), and the
        // device writes the used entry only after consuming the avail entry;
        // UsedRingConsumer::poll's Acquire fence completes the chain.
        let buf = TX_ISR.desc_to_buf[desc_id as usize].load(Ordering::Relaxed) as u32;
        mask |= 1 << buf;
    }
    mask
}

pub(crate) const TX_INFLIGHT_MAX: usize = 8;

/// Stride between xfer headers within the TX meta region (aligned to 16 bytes)
const XFER_STRIDE: u64 = 16;
/// Offset where status structs start within the TX meta region
const STATUS_OFFSET: u64 = XFER_STRIDE * TX_INFLIGHT_MAX as u64;
/// Stride between status structs
const STATUS_STRIDE: u64 = core::mem::size_of::<VirtioSndPcmStatus>() as u64;

/// Number of event buffers kept posted on the eventq.
const EVENT_BUFS: usize = 8;
/// Stride between event buffers (event struct is 8 bytes, padded to 16).
const EVENT_BUF_STRIDE: usize = 16;

pub struct SoundController {
    device: VirtioDevice,
    controlq: Virtqueue,
    eventq: Virtqueue,
    txq: Virtqueue,
    /// Physical addresses for virtqueue descriptors.
    req_phys: u64,
    resp_phys: u64,
    /// Virtual pointers for kernel read/write.
    req_ptr: *mut u8,
    resp_ptr: *mut u8,
    /// Physical base of the TX meta region (for descriptor addresses).
    meta_phys: u64,
    /// Virtual base of the TX meta region (for kernel write_volatile).
    meta_ptr: *mut u8,
    /// Physical addresses of the 8 PCM data buffers in the shared page.
    tx_data_phys: [u64; TX_INFLIGHT_MAX],
    /// Bitmask of buffers currently in-flight (submitted, completion record
    /// not yet returned to userspace).
    inflight_mask: u32,
    /// Descriptor chain head currently carrying each buffer — recycled from
    /// completion-record masks (the ISR owns the used ring, so DescSlots
    /// never come back through poll_used).
    buf_desc: [u8; TX_INFLIGHT_MAX],
    started: bool,
    control_slot: Option<DescSlot>,
    /// Available TX descriptor slots (returned by recycle, consumed by submit)
    tx_free_slots: alloc::vec::Vec<DescSlot>,
    /// Event buffer region (kernel page) for eventq reposting.
    event_buf_phys: u64,
    event_buf_ptr: *mut u8,
    /// DMA backing — kernel-only page (rings + metadata) and the page shared
    /// with soundd (PCM data only).
    _dma_kernel: DmaPool,
    _dma_shared: DmaPool,
}

unsafe impl Send for SoundController {}

impl SoundController {
    fn ctrl_command<T: Copy>(&mut self, req: &T, resp_size: u32) -> u32 {
        let bytes = unsafe {
            core::slice::from_raw_parts(req as *const T as *const u8, core::mem::size_of::<T>())
        };
        unsafe {
            copy_nonoverlapping(bytes.as_ptr(), self.req_ptr, bytes.len());
        }

        let slot = self.control_slot.take().expect("sound: no control slot");
        self.control_slot = Some(self.controlq.submit_and_wait(
            slot,
            &[
                (self.req_phys, bytes.len() as u32, BufDir::Readable),
                (self.resp_phys, resp_size, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            CONTROL_QUEUE,
        ));

        unsafe { read_volatile(self.resp_ptr as *const u32) }
    }

    fn simple_ctrl(&mut self, code: u32, stream_id: u32) -> u32 {
        let cmd = VirtioSndPcmHdr {
            hdr: VirtioSndHdr { code },
            stream_id,
        };
        self.ctrl_command(&cmd, core::mem::size_of::<VirtioSndHdr>() as u32)
    }

    pub fn configure(&mut self, sample_rate: u32, channels: u8) {
        // `expect`, not a fallback: `choose_params` has already checked this
        // rate against the device's own bitmap, so an unencodable one here
        // means the two disagree — a driver bug, not a device we cannot drive.
        // The old `_ => RATE_44100` arm turned that into telling a device to
        // play at a rate it never offered.
        let rate = rate_code(sample_rate)
            .expect("virtio-sound: configure() given a rate the driver cannot encode");

        let cmd = VirtioSndPcmSetParams {
            hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_SET_PARAMS },
            stream_id: 0,
            buffer_bytes: (PERIOD_BYTES * TX_INFLIGHT_MAX) as u32,
            period_bytes: PERIOD_BYTES as u32,
            features: 0,
            channels,
            format: VIRTIO_SND_PCM_FMT_S16,
            rate,
            _padding: 0,
        };
        let status = self.ctrl_command(&cmd, core::mem::size_of::<VirtioSndHdr>() as u32);
        assert!(status == VIRTIO_SND_S_OK, "virtio-sound: SET_PARAMS failed: {:#x}", status);

        let status = self.simple_ctrl(VIRTIO_SND_R_PCM_PREPARE, 0);
        assert!(status == VIRTIO_SND_S_OK, "virtio-sound: PREPARE failed: {:#x}", status);

        log!("virtio-sound: configured stream 0: {}Hz {}ch s16le", sample_rate, channels);
    }

    pub fn start(&mut self) {
        if self.started { return; }
        let status = self.simple_ctrl(VIRTIO_SND_R_PCM_START, 0);
        assert!(status == VIRTIO_SND_S_OK, "virtio-sound: START failed: {:#x}", status);
        self.started = true;
        log!("virtio-sound: stream 0 started");
    }

    pub fn stop(&mut self) {
        if !self.started { return; }
        let status = self.simple_ctrl(VIRTIO_SND_R_PCM_STOP, 0);
        assert!(status == VIRTIO_SND_S_OK, "virtio-sound: STOP failed: {:#x}", status);
        self.started = false;
        log!("virtio-sound: stream 0 stopped");
    }

    /// Submit a TX data buffer to the VirtIO device.
    /// `idx`: buffer index (0..TX_INFLIGHT_MAX), `len`: bytes of PCM data.
    /// The data buffer must already be filled by soundd via shared memory.
    /// Returns false on bad arguments, an in-flight buffer, or a full queue.
    pub fn submit_buffer(&mut self, idx: usize, len: u32) -> bool {
        if idx >= TX_INFLIGHT_MAX { return false; }
        if len == 0 || len as usize > PERIOD_BYTES { return false; }
        if self.inflight_mask & (1 << idx) != 0 { return false; }
        let Some(slot) = self.tx_free_slots.pop() else { return false };
        if !self.started {
            self.start();
        }

        let first_desc = slot.id();
        // The ISR maps used-ring desc ids to buffer indices; the mapping
        // must be published before the avail-ring store makes the chain
        // visible to the device (Release fence inside Virtqueue::submit).
        TX_ISR.desc_to_buf[first_desc as usize].store(idx as u8, Ordering::Relaxed);

        let hdr_phys = self.meta_phys + idx as u64 * XFER_STRIDE;
        let data_phys = self.tx_data_phys[idx];
        let status_phys = self.meta_phys + STATUS_OFFSET + idx as u64 * STATUS_STRIDE;

        // Write xfer header via virtual pointer (kernel-owned page, not shared)
        let hdr_ptr = unsafe { self.meta_ptr.add(idx * XFER_STRIDE as usize) };
        let xfer = VirtioSndPcmXfer { stream_id: 0 };
        unsafe { write_volatile(hdr_ptr as *mut VirtioSndPcmXfer, xfer); }

        let hdr_size = core::mem::size_of::<VirtioSndPcmXfer>() as u32;
        let status_size = core::mem::size_of::<VirtioSndPcmStatus>() as u32;

        let submitted = self.txq.submit(
            slot,
            &[
                (hdr_phys, hdr_size, BufDir::Readable),
                (data_phys, len, BufDir::Readable),
                (status_phys, status_size, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            TX_QUEUE,
        );
        assert!(submitted == first_desc, "virtio-sound: submit used unexpected descriptor");
        self.buf_desc[idx] = first_desc as u8;
        self.inflight_mask |= 1 << idx;
        true
    }

    /// Return the descriptors of completed buffers to the free pool. `mask`
    /// comes from the completion records being handed to userspace — this
    /// is the AUDIO-lock half of completion handling that the lock-free ISR
    /// cannot do.
    pub fn recycle(&mut self, mask: u32) {
        let mut m = mask;
        while m != 0 {
            let idx = m.trailing_zeros() as usize;
            m &= m - 1;
            assert!(
                self.inflight_mask & (1 << idx) != 0,
                "virtio-sound: completion for idle buffer {idx}"
            );
            self.inflight_mask &= !(1 << idx);
            self.tx_free_slots.push(DescSlot::reclaim(self.buf_desc[idx] as u16));
        }
    }

    /// Service the eventq: log device-side exceptions (XRUN etc.) and
    /// repost the buffers. Called from the completion-drain path.
    pub fn poll_events(&mut self) {
        while let Some((slot, _len)) = self.eventq.poll_used() {
            let id = slot.id() as usize;
            assert!(id < EVENT_BUFS, "virtio-sound: bogus event desc id {id}");
            let event = unsafe {
                read_volatile(self.event_buf_ptr.add(id * EVENT_BUF_STRIDE) as *const VirtioSndEvent)
            };
            let name = match event.hdr.code {
                VIRTIO_SND_EVT_JACK_CONNECTED => " (jack connected)",
                VIRTIO_SND_EVT_JACK_DISCONNECTED => " (jack disconnected)",
                VIRTIO_SND_EVT_PCM_PERIOD_ELAPSED => " (period elapsed)",
                VIRTIO_SND_EVT_PCM_XRUN => " (PCM XRUN)",
                _ => "",
            };
            log!("virtio-sound: device event {:#x}{} data={}", event.hdr.code, name, event.data);
            self.eventq.submit(
                slot,
                &[(self.event_buf_phys + (id * EVENT_BUF_STRIDE) as u64,
                   core::mem::size_of::<VirtioSndEvent>() as u32,
                   BufDir::Writable)],
                self.device.notify_mmio(),
                self.device.notify_off_multiplier(),
                EVENT_QUEUE,
            );
        }
    }
}

const VIRTIO_SOUND_VECTOR: u8 = 0x23;

/// Arm this device's txq completion interrupt, or say why the machine has no
/// audio.
///
/// A refusal rather than a panic, for `virtio_net::arm_interrupt`'s reason and
/// one of its own: `TX_ISR` is the only consumer of the txq used ring, so a
/// device that cannot deliver its completions is one whose every period stays
/// in flight forever. `None` here leaves `audio::register` uncalled, so soundd
/// finds no device and exits — a machine that boots and plays nothing rather
/// than a machine that dies.
fn arm_interrupt(pci_dev: &PciDevice, device: &VirtioDevice) -> bool {
    if !pci_dev.enable_msix(VIRTIO_SOUND_VECTOR) {
        log!("virtio-sound: NOT INITIALISED at PCI {:02x}:{:02x}.{} — its MSI-X could not be \
             armed and this driver has no other way to be told a period completed",
            pci_dev.bus, pci_dev.dev, pci_dev.func);
        return false;
    }
    if let Err(refused) = device.bind_msix(TX_QUEUE) {
        log!("virtio-sound: NOT INITIALISED at PCI {:02x}:{:02x}.{} — the device refused a \
             vector for {}", pci_dev.bus, pci_dev.dev, pci_dev.func, refused);
        return false;
    }
    log!("virtio-sound: MSI-X vector {:#x} on table entry {}", VIRTIO_SOUND_VECTOR, MSIX_ENTRY);
    true
}

/// Initialize the VirtIO sound device. Returns the controller and AudioInfo on success.
pub fn init(devices: &[PciDevice]) -> Option<(SoundController, AudioInfo)> {
    let pci_dev = *devices.iter().find(|d| d.is_id(VIRTIO_VENDOR, VIRTIO_SND_DEVICE))?;
    log!("virtio-sound: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    let dma_kernel = DmaPool::alloc(KERNEL_DMA_SIZE);
    let dma_shared = DmaPool::alloc(SHARED_DMA_SIZE);
    let dma = dma_kernel.slice();
    let shared = dma_shared.slice();

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1);

    let cfg = device.device_config();
    let jacks = cfg.read_u32(0);
    let streams = cfg.read_u32(4);
    let chmaps = cfg.read_u32(8);
    log!("virtio-sound: {} jacks, {} streams, {} chmaps", jacks, streams, chmaps);

    assert!(streams > 0, "virtio-sound: no PCM streams available");

    let mut controlq = Virtqueue::new(dma.subslice(OFF_CONTROLQ, 0x1000), 16);
    let mut eventq = Virtqueue::new(dma.subslice(OFF_EVENTQ, 0x1000), 16);
    let mut txq = Virtqueue::new(dma.subslice(OFF_TXQ, 0x1000), TXQ_SIZE as u16);

    // The ISR is the only txq used-ring consumer. Install it before
    // arm_interrupt so no interrupt on this vector can observe a half-written
    // Option (config-change interrupts share the vector).
    // SAFETY: MSI-X is not enabled yet — nothing races this store.
    unsafe { *TX_ISR.consumer.get() = Some(txq.split_used_consumer()); }

    device.setup_queue(CONTROL_QUEUE, &mut controlq);
    device.setup_queue(EVENT_QUEUE, &mut eventq);
    device.setup_queue(TX_QUEUE, &mut txq);
    if !arm_interrupt(&pci_dev, &device) {
        return None;
    }
    device.enable_queue(CONTROL_QUEUE);
    device.enable_queue(EVENT_QUEUE);
    device.enable_queue(TX_QUEUE);
    device.activate();

    let ctrl_bufs = dma.subslice(OFF_CTRL_BUFS, 0x1000);
    let meta = dma.subslice(OFF_TX_META, 0x1000);
    let event_bufs = dma.subslice(OFF_EVENT_BUFS, 0x1000);
    let req_phys = ctrl_bufs.phys() + REQ_OFFSET as u64;
    let resp_phys = ctrl_bufs.phys() + RESP_OFFSET as u64;
    let req_ptr = ctrl_bufs.ptr_at(REQ_OFFSET);
    let resp_ptr = ctrl_bufs.ptr_at(RESP_OFFSET);
    let meta_phys = meta.phys();
    let meta_ptr = meta.base();

    // DmaPool allocations are whole 2MB pages, so the shared slice base IS
    // the page soundd maps; buf_offsets are relative to it.
    let dma_token = shared_memory::register(crate::DirectMap::from_phys(shared.phys()), crate::mm::PAGE_2M, CachePolicy::DeferToMtrr);
    let mut tx_data_phys = [0u64; TX_INFLIGHT_MAX];
    let mut buf_offsets = [0u32; TX_INFLIGHT_MAX];
    for i in 0..TX_INFLIGHT_MAX {
        tx_data_phys[i] = shared.phys() + (i * PERIOD_BYTES) as u64;
        buf_offsets[i] = (i * PERIOD_BYTES) as u32;
    }

    let mut control_slots = controlq.initial_slots();
    let control_slot = control_slots.pop().expect("sound: no control slots");
    drop(control_slots);
    let tx_free_slots = txq.initial_slots_strided(3);

    let mut ctrl = SoundController {
        device,
        controlq,
        eventq,
        txq,
        req_phys,
        resp_phys,
        req_ptr,
        resp_ptr,
        meta_phys,
        meta_ptr,
        tx_data_phys,
        inflight_mask: 0,
        buf_desc: [0; TX_INFLIGHT_MAX],
        started: false,
        control_slot: Some(control_slot),
        tx_free_slots,
        event_buf_phys: event_bufs.phys(),
        event_buf_ptr: event_bufs.base(),
        _dma_kernel: dma_kernel,
        _dma_shared: dma_shared,
    };

    // Post event buffers so the device can report XRUN/jack events. Slot ids
    // double as buffer indices — single-descriptor chains, reposted 1:1.
    let mut event_slots = ctrl.eventq.initial_slots();
    event_slots.truncate(EVENT_BUFS);
    for slot in event_slots {
        let id = slot.id() as usize;
        ctrl.eventq.submit(
            slot,
            &[(ctrl.event_buf_phys + (id * EVENT_BUF_STRIDE) as u64,
               core::mem::size_of::<VirtioSndEvent>() as u32,
               BufDir::Writable)],
            ctrl.device.notify_mmio(),
            ctrl.device.notify_off_multiplier(),
            EVENT_QUEUE,
        );
    }

    let query = VirtioSndQueryInfo {
        hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_INFO },
        start_id: 0,
        count: 1,
        size: core::mem::size_of::<VirtioSndPcmInfo>() as u32,
    };
    let resp_size = core::mem::size_of::<VirtioSndHdr>() as u32
        + core::mem::size_of::<VirtioSndPcmInfo>() as u32;
    let status = ctrl.ctrl_command(&query, resp_size);
    assert!(status == VIRTIO_SND_S_OK, "virtio-sound: PCM_INFO failed: {:#x}", status);

    let pcm_info = unsafe {
        core::ptr::read_unaligned(ctrl.resp_ptr.add(core::mem::size_of::<VirtioSndHdr>()) as *const VirtioSndPcmInfo)
    };
    log!("virtio-sound: stream 0: dir={} ch={}-{} fmts={:#x} rates={:#x}",
        pcm_info.direction, pcm_info.channels_min, pcm_info.channels_max,
        pcm_info.formats, pcm_info.rates);

    let (sample_rate, channels) = choose_params(&pcm_info)?;
    ctrl.configure(sample_rate, channels);

    let info = AudioInfo {
        dma_token: dma_token.raw(),
        buf_offsets,
        num_buffers: TX_INFLIGHT_MAX as u8,
        _pad0: [0; 3],
        sample_rate,
        channels,
        _pad1: [0; 3],
        period_bytes: PERIOD_BYTES as u32,
    };

    log!("virtio-sound: initialized ({} DMA buffers, playback starts on first submit)", TX_INFLIGHT_MAX);
    Some((ctrl, info))
}
