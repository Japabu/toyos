use core::ptr::{copy_nonoverlapping, read_volatile, write_volatile};

use super::pci::PciDevice;
use super::virtio::{BufDir, DescSlot, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::log;
use crate::mm::KernelSlice;
use crate::sync::Lock;
use crate::shared_memory;
use toyos_abi::audio::AudioInfo;

// VirtIO sound PCI identity
const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_SND_DEVICE: u16 = 0x1059; // 0x1040 + device_id 25

// Control request types
const VIRTIO_SND_R_PCM_INFO: u32 = 0x0100;
const VIRTIO_SND_R_PCM_SET_PARAMS: u32 = 0x0101;
const VIRTIO_SND_R_PCM_PREPARE: u32 = 0x0102;
#[allow(dead_code)]
const VIRTIO_SND_R_PCM_RELEASE: u32 = 0x0103;
const VIRTIO_SND_R_PCM_START: u32 = 0x0104;

// Status codes
const VIRTIO_SND_S_OK: u32 = 0x8000;

// PCM formats (VirtIO 1.2 spec §5.14.6.6)
const VIRTIO_SND_PCM_FMT_S16: u8 = 5;

// PCM rates (VirtIO 1.2 spec §5.14.6.7)
const VIRTIO_SND_PCM_RATE_44100: u8 = 6;
const VIRTIO_SND_PCM_RATE_48000: u8 = 7;

// DMA layout (byte offsets)
const OFF_CONTROLQ: usize  = 0x0000;
const OFF_EVENTQ: usize    = 0x1000;
const OFF_TXQ: usize       = 0x2000;
const OFF_CTRL_BUFS: usize = 0x3000;
const OFF_TX_META: usize   = 0x4000;
const OFF_TX_DATA: usize   = 0x5000; // 5 × 4KB
const DMA_SIZE: usize      = 0xA000;

const REQ_OFFSET: usize = 0x000;
const RESP_OFFSET: usize = 0x800;

static DMA: Lock<Option<DmaPool>> = Lock::new(None);

fn dma() -> KernelSlice {
    DMA.lock().as_ref().unwrap().slice()
}

// ---- VirtIO sound structs (per VirtIO 1.2 spec, section 5.14) ----

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

// ---- Sound Controller ----

/// Maximum number of TX buffers in flight at once.
/// Each uses 3 descriptors from the 16-slot virtqueue, so max 5 in flight.
const TX_INFLIGHT_MAX: usize = 5;

/// Stride between xfer headers within PAGE_TX_META (aligned to 16 bytes)
const XFER_STRIDE: u64 = 16;
/// Offset where status structs start within PAGE_TX_META
const STATUS_OFFSET: u64 = XFER_STRIDE * TX_INFLIGHT_MAX as u64;
/// Stride between status structs
const STATUS_STRIDE: u64 = core::mem::size_of::<VirtioSndPcmStatus>() as u64;

pub struct SoundController {
    device: VirtioDevice,
    controlq: Virtqueue,
    txq: Virtqueue,
    /// Physical addresses for virtqueue descriptors.
    req_phys: u64,
    resp_phys: u64,
    /// Virtual pointers for kernel read/write.
    req_ptr: *mut u8,
    resp_ptr: *mut u8,
    /// Physical base of PAGE_TX_META (for descriptor addresses).
    meta_phys: u64,
    /// Virtual base of PAGE_TX_META (for kernel write_volatile).
    meta_ptr: *mut u8,
    /// Physical addresses of the 5 TX data pages (for device DMA descriptors).
    tx_data_phys: [u64; TX_INFLIGHT_MAX],
    tx_inflight: usize,
    /// Bitmask of buffers currently in-flight (submitted but not yet completed)
    inflight_mask: u32,
    /// Maps first descriptor ID → buffer index (needed because desc IDs wrap around
    /// the 16-entry ring, so desc_id/3 doesn't map correctly after the first cycle)
    desc_to_buf: [u8; 16],
    started: bool,
    control_slot: Option<DescSlot>,
    /// Available TX descriptor slots (returned by poll_used, consumed by submit)
    tx_free_slots: alloc::vec::Vec<DescSlot>,
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
            0, // controlq index
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
        let rate = match sample_rate {
            44100 => VIRTIO_SND_PCM_RATE_44100,
            48000 => VIRTIO_SND_PCM_RATE_48000,
            _ => VIRTIO_SND_PCM_RATE_44100,
        };

        let cmd = VirtioSndPcmSetParams {
            hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_SET_PARAMS },
            stream_id: 0,
            buffer_bytes: 4096 * 4,
            period_bytes: 4096,
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

    /// Drain completed TX buffers. Returns bitmask of newly completed buffer indices.
    fn drain_tx(&mut self) -> u32 {
        let mut completed = 0u32;
        while self.tx_inflight > 0 {
            if let Some((slot, _)) = self.txq.poll_used() {
                let idx = self.desc_to_buf[slot.id() as usize] as usize;
                completed |= 1 << idx;
                self.inflight_mask &= !(1 << idx);
                self.tx_inflight -= 1;
                self.tx_free_slots.push(slot);
            } else {
                break;
            }
        }
        completed
    }

    /// Submit a TX data buffer to the VirtIO device.
    /// `idx`: buffer index (0..4), `len`: bytes of PCM data in the buffer.
    /// The data page must already be filled by soundd via shared memory.
    /// Returns true on success, false if the slot is in-flight.
    pub fn submit_buffer(&mut self, idx: usize, len: u32) -> bool {
        if idx >= TX_INFLIGHT_MAX { return false; }
        if self.inflight_mask & (1 << idx) != 0 { return false; }
        let Some(slot) = self.tx_free_slots.pop() else { return false };
        if !self.started {
            self.start();
        }

        let hdr_phys = self.meta_phys + idx as u64 * XFER_STRIDE;
        let data_phys = self.tx_data_phys[idx];
        let status_phys = self.meta_phys + STATUS_OFFSET + idx as u64 * STATUS_STRIDE;

        // Write xfer header via virtual pointer (kernel-owned page, not shared)
        let hdr_ptr = unsafe { self.meta_ptr.add(idx * XFER_STRIDE as usize) };
        let xfer = VirtioSndPcmXfer { stream_id: 0 };
        unsafe { write_volatile(hdr_ptr as *mut VirtioSndPcmXfer, xfer); }

        let hdr_size = core::mem::size_of::<VirtioSndPcmXfer>() as u32;
        let status_size = core::mem::size_of::<VirtioSndPcmStatus>() as u32;

        let first_desc = self.txq.submit(
            slot,
            &[
                (hdr_phys, hdr_size, BufDir::Readable),
                (data_phys, len, BufDir::Readable),
                (status_phys, status_size, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            2, // txq index
        );
        self.desc_to_buf[first_desc as usize] = idx as u8;
        self.inflight_mask |= 1 << idx;
        self.tx_inflight += 1;
        true
    }

    /// Poll for completed buffers. Returns bitmask of buffer indices that are now free.
    pub fn poll_completed(&mut self) -> u32 {
        self.drain_tx()
    }
}

/// Initialize the VirtIO sound device. Returns the controller and AudioInfo on success.
pub fn init(ecam: &crate::mm::Mmio) -> Option<(SoundController, AudioInfo)> {
    let pci_dev = PciDevice::find_by_id(ecam, VIRTIO_VENDOR, VIRTIO_SND_DEVICE)?;
    log!("virtio-sound: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    *DMA.lock() = Some(DmaPool::alloc(DMA_SIZE));
    let dma = dma();

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1);

    let cfg = device.device_config();
    let jacks = cfg.read_u32(0);
    let streams = cfg.read_u32(4);
    let chmaps = cfg.read_u32(8);
    log!("virtio-sound: {} jacks, {} streams, {} chmaps", jacks, streams, chmaps);

    assert!(streams > 0, "virtio-sound: no PCM streams available");

    let mut controlq = Virtqueue::new(dma.subslice(OFF_CONTROLQ, 0x1000));
    let mut eventq = Virtqueue::new(dma.subslice(OFF_EVENTQ, 0x1000));
    let mut txq = Virtqueue::new(dma.subslice(OFF_TXQ, 0x1000));

    device.setup_queue(0, &mut controlq);
    device.setup_queue(1, &mut eventq);
    device.setup_queue(2, &mut txq);
    device.enable_queue(0);
    device.enable_queue(1);
    device.enable_queue(2);
    device.activate();

    let ctrl_bufs = dma.subslice(OFF_CTRL_BUFS, 0x1000);
    let meta = dma.subslice(OFF_TX_META, 0x1000);
    let req_phys = ctrl_bufs.phys() + REQ_OFFSET as u64;
    let resp_phys = ctrl_bufs.phys() + RESP_OFFSET as u64;
    let req_ptr = ctrl_bufs.ptr_at(REQ_OFFSET);
    let resp_ptr = ctrl_bufs.ptr_at(RESP_OFFSET);
    let meta_phys = meta.phys();
    let meta_ptr = meta.base();

    let mut tx_data_phys = [0u64; TX_INFLIGHT_MAX];
    let dma_base_phys = dma.phys() & !(crate::mm::PAGE_2M - 1);
    let dma_token = shared_memory::register(crate::DirectMap::from_phys(dma_base_phys), crate::mm::PAGE_2M);
    let mut buf_offsets = [0u32; TX_INFLIGHT_MAX];
    for i in 0..TX_INFLIGHT_MAX {
        tx_data_phys[i] = dma.phys() + (OFF_TX_DATA + i * 0x1000) as u64;
        buf_offsets[i] = (OFF_TX_DATA + i * 0x1000) as u32;
    }

    let mut control_slots = controlq.initial_slots();
    let control_slot = control_slots.pop().expect("sound: no control slots");
    drop(control_slots);
    let tx_free_slots = txq.initial_slots();

    let mut ctrl = SoundController {
        device,
        controlq,
        txq,
        req_phys,
        resp_phys,
        req_ptr,
        resp_ptr,
        meta_phys,
        meta_ptr,
        tx_data_phys,
        tx_inflight: 0,
        inflight_mask: 0,
        desc_to_buf: [0; 16],
        started: false,
        control_slot: Some(control_slot),
        tx_free_slots,
    };

    // Query PCM stream info
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

    ctrl.configure(44100, 2);

    let info = AudioInfo {
        dma_token: dma_token.raw(),
        buf_offsets,
        num_buffers: TX_INFLIGHT_MAX as u8,
        sample_rate: 44100,
        channels: 2,
        period_bytes: 4096,
    };

    log!("virtio-sound: initialized ({} DMA buffers, playback starts on first submit)", TX_INFLIGHT_MAX);
    Some((ctrl, info))
}
