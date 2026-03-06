use core::ptr::{copy_nonoverlapping, read_volatile, write_volatile};

use super::pci::PciDevice;
use super::virtio::{BufDir, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::log;
use crate::sync::Lock;

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
const VIRTIO_SND_R_PCM_STOP: u32 = 0x0105;

// Status codes
const VIRTIO_SND_S_OK: u32 = 0x8000;

// PCM formats (VirtIO 1.2 spec §5.14.6.6)
const VIRTIO_SND_PCM_FMT_S16: u8 = 5;

// PCM rates (VirtIO 1.2 spec §5.14.6.7)
const VIRTIO_SND_PCM_RATE_44100: u8 = 6;
const VIRTIO_SND_PCM_RATE_48000: u8 = 7;

// DMA page assignments
const PAGE_CONTROLQ: usize = 0;
const PAGE_EVENTQ: usize = 1;
const PAGE_TXQ: usize = 2;
const PAGE_CTRL_BUFS: usize = 3;
const PAGE_TX_BUFS: usize = 4; // 5 pages: 4..8
const PAGE_TX_STATUS: usize = 9;

const REQ_OFFSET: usize = 0x000;
const RESP_OFFSET: usize = 0x800;

static DMA: Lock<DmaPool<10>> = Lock::new(DmaPool::new());

fn dma_addr(page: usize) -> u64 {
    DMA.lock().page_addr(page)
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

pub struct SoundController {
    device: VirtioDevice,
    controlq: Virtqueue,
    txq: Virtqueue,
    req_buf: u64,
    resp_buf: u64,
    tx_buf_addrs: [u64; TX_INFLIGHT_MAX],
    tx_status_addrs: [u64; TX_INFLIGHT_MAX],
    tx_buf_idx: usize,
    tx_inflight: usize,
    started: bool,
}

unsafe impl Send for SoundController {}

impl SoundController {
    fn ctrl_command<T: Copy>(&mut self, req: &T, resp_size: u32) -> u32 {
        let bytes = unsafe {
            core::slice::from_raw_parts(req as *const T as *const u8, core::mem::size_of::<T>())
        };
        unsafe {
            copy_nonoverlapping(bytes.as_ptr(), self.req_buf as *mut u8, bytes.len());
        }

        self.controlq.submit_and_wait(
            &[
                (self.req_buf, bytes.len() as u32, BufDir::Readable),
                (self.resp_buf, resp_size, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0, // controlq index
        );

        unsafe { read_volatile(self.resp_buf as *const u32) }
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

        // Set parameters for stream 0
        let cmd = VirtioSndPcmSetParams {
            hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_SET_PARAMS },
            stream_id: 0,
            buffer_bytes: 4096 * 4, // 16KB total buffer
            period_bytes: 4096,      // 4KB per period (~23ms at 44100Hz stereo s16)
            features: 0,
            channels,
            format: VIRTIO_SND_PCM_FMT_S16,
            rate,
            _padding: 0,
        };
        let status = self.ctrl_command(&cmd, core::mem::size_of::<VirtioSndHdr>() as u32);
        assert!(status == VIRTIO_SND_S_OK, "virtio-sound: SET_PARAMS failed: {:#x}", status);

        // Prepare stream
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
    }

    /// Drain any completed TX buffers from the used ring.
    fn drain_tx(&mut self) -> usize {
        let mut drained = 0;
        while self.tx_inflight > 0 {
            if self.txq.poll_used().is_some() {
                self.tx_inflight -= 1;
                drained += 1;
            } else {
                break;
            }
        }
        drained
    }

    /// Write PCM samples to the TX queue (non-blocking). Data must be s16le.
    /// Max ~4000 bytes per call (DMA page minus header overhead).
    /// If the TX queue is full, the buffer is silently dropped.
    pub fn write_samples(&mut self, data: &[u8]) {
        if data.is_empty() { return; }
        if !self.started {
            self.start();
        }

        // Drain any completed buffers
        self.drain_tx();

        // If all slots are in use, drop this buffer (audio tolerates gaps)
        if self.tx_inflight >= TX_INFLIGHT_MAX {
            return;
        }

        let idx = self.tx_buf_idx;
        self.tx_buf_idx = (self.tx_buf_idx + 1) % TX_INFLIGHT_MAX;

        let buf_addr = self.tx_buf_addrs[idx];
        let status_addr = self.tx_status_addrs[idx];

        // Write xfer header at start of buffer
        let xfer = VirtioSndPcmXfer { stream_id: 0 };
        let hdr_size = core::mem::size_of::<VirtioSndPcmXfer>();
        let status_size = core::mem::size_of::<VirtioSndPcmStatus>() as u32;

        // Max payload is page size minus header
        let max_payload = 4096 - hdr_size;
        let payload_len = data.len().min(max_payload);

        unsafe {
            write_volatile(buf_addr as *mut VirtioSndPcmXfer, xfer);
            copy_nonoverlapping(
                data.as_ptr(),
                (buf_addr as *mut u8).add(hdr_size),
                payload_len,
            );
        }

        // 3 descriptors: xfer header (readable), PCM data (readable), status (writable)
        let data_addr = buf_addr + hdr_size as u64;
        self.txq.submit(
            &[
                (buf_addr, hdr_size as u32, BufDir::Readable),
                (data_addr, payload_len as u32, BufDir::Readable),
                (status_addr, status_size, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            2, // txq index
        );
        self.tx_inflight += 1;
    }
}

/// Initialize the VirtIO sound device. Returns the controller on success.
pub fn init(ecam_base: u64) -> Option<SoundController> {
    let pci_dev = PciDevice::find_by_id(ecam_base, VIRTIO_VENDOR, VIRTIO_SND_DEVICE)?;
    log!("virtio-sound: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1);

    // Read device config: jacks, streams, chmaps
    let cfg = device.device_config();
    let jacks = cfg.read_u32(0);
    let streams = cfg.read_u32(4);
    let chmaps = cfg.read_u32(8);
    log!("virtio-sound: {} jacks, {} streams, {} chmaps", jacks, streams, chmaps);

    assert!(streams > 0, "virtio-sound: no PCM streams available");

    let mut controlq = Virtqueue::new(dma_addr(PAGE_CONTROLQ));
    let mut eventq = Virtqueue::new(dma_addr(PAGE_EVENTQ));
    let mut txq = Virtqueue::new(dma_addr(PAGE_TXQ));

    device.setup_queue(0, &mut controlq);
    device.setup_queue(1, &mut eventq);
    device.setup_queue(2, &mut txq);
    device.activate();

    let req_buf = dma_addr(PAGE_CTRL_BUFS) + REQ_OFFSET as u64;
    let resp_buf = dma_addr(PAGE_CTRL_BUFS) + RESP_OFFSET as u64;

    let status_base = dma_addr(PAGE_TX_STATUS);
    let status_stride = core::mem::size_of::<VirtioSndPcmStatus>() as u64;
    let mut tx_buf_addrs = [0u64; TX_INFLIGHT_MAX];
    let mut tx_status_addrs = [0u64; TX_INFLIGHT_MAX];
    for i in 0..TX_INFLIGHT_MAX {
        tx_buf_addrs[i] = dma_addr(PAGE_TX_BUFS + i);
        tx_status_addrs[i] = status_base + i as u64 * status_stride;
    }
    let mut ctrl = SoundController {
        device,
        controlq,
        txq,
        req_buf,
        resp_buf,
        tx_buf_addrs,
        tx_status_addrs,
        tx_buf_idx: 0,
        tx_inflight: 0,
        started: false,
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
        core::ptr::read_unaligned((ctrl.resp_buf + core::mem::size_of::<VirtioSndHdr>() as u64) as *const VirtioSndPcmInfo)
    };
    log!("virtio-sound: stream 0: dir={} ch={}-{} fmts={:#x} rates={:#x}",
        pcm_info.direction, pcm_info.channels_min, pcm_info.channels_max,
        pcm_info.formats, pcm_info.rates);

    // Auto-configure: stereo s16le 44100Hz (start deferred to first write)
    ctrl.configure(44100, 2);

    log!("virtio-sound: initialized (playback starts on first write)");
    Some(ctrl)
}
