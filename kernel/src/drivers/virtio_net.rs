use alloc::boxed::Box;
use core::ptr::{copy_nonoverlapping, write_bytes};

use super::pci::PciDevice;
use super::virtio::{BufDir, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::log;
use crate::net::NicInfo;
use crate::shared_memory;
use crate::sync::Lock;

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_NET_DEVICE: u16 = 0x1041; // 0x1040 + device_id 1

const VIRTIO_NET_F_MAC: u64 = 1 << 5;

// VirtIO 1.0 net header: always 12 bytes (includes num_buffers) with VERSION_1
const NET_HDR_SIZE: usize = 12;

// DMA page assignments
const PAGE_RXQ: usize = 0;
const PAGE_TXQ: usize = 1;
const PAGE_RX_BUFS: usize = 2; // 3 pages for RX buffers
const PAGE_TX_BUF: usize = 5;

const RX_BUF_COUNT: usize = 3;
const RX_BUF_SIZE: u32 = 4096;

static DMA: Lock<DmaPool<6>> = Lock::new(DmaPool::new());

fn dma_addr(page: usize) -> u64 {
    DMA.lock().page_addr(page)
}

struct VirtioNic {
    device: VirtioDevice,
    rxq: Virtqueue,
    txq: Virtqueue,
    mac: [u8; 6],
    rx_bufs: [u64; RX_BUF_COUNT],
    tx_buf: u64,
    // Maps virtqueue descriptor index -> rx_bufs index
    desc_to_buf: [usize; 16],
}

unsafe impl Send for VirtioNic {}

impl VirtioNic {
    fn refill_rx(&mut self, buf_idx: usize) {
        let buf_addr = self.rx_bufs[buf_idx];
        unsafe { write_bytes(buf_addr as *mut u8, 0, NET_HDR_SIZE); }
        let desc_id = self.rxq.submit(
            &[(buf_addr, RX_BUF_SIZE, BufDir::Writable)],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0,
        );
        self.desc_to_buf[desc_id as usize] = buf_idx;
    }
}

impl crate::net::Nic for VirtioNic {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn send(&mut self, frame: &[u8]) {
        let max_frame = 4096 - NET_HDR_SIZE;
        let len = frame.len().min(max_frame);

        unsafe {
            write_bytes(self.tx_buf as *mut u8, 0, NET_HDR_SIZE);
            copy_nonoverlapping(
                frame.as_ptr(),
                (self.tx_buf as *mut u8).add(NET_HDR_SIZE),
                len,
            );
        }

        self.txq.submit_and_wait(
            &[(self.tx_buf, (NET_HDR_SIZE + len) as u32, BufDir::Readable)],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            1,
        );
    }

    fn has_packet(&self) -> bool {
        self.rxq.has_used()
    }

    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let (desc_id, written_len) = self.rxq.poll_used()?;
        let buf_idx = self.desc_to_buf[desc_id as usize];
        let total = written_len as usize;
        if total <= NET_HDR_SIZE {
            self.refill_rx(buf_idx);
            return None;
        }
        let frame_len = total - NET_HDR_SIZE;
        let copy_len = frame_len.min(buf.len());
        let src = self.rx_bufs[buf_idx] + NET_HDR_SIZE as u64;
        unsafe {
            copy_nonoverlapping(src as *const u8, buf.as_mut_ptr(), copy_len);
        }
        self.refill_rx(buf_idx);
        Some(copy_len)
    }

    fn poll_rx(&mut self) -> Option<(usize, usize)> {
        let (desc_id, written_len) = self.rxq.poll_used()?;
        let buf_idx = self.desc_to_buf[desc_id as usize];
        let total = written_len as usize;
        if total <= NET_HDR_SIZE {
            self.refill_rx(buf_idx);
            return None;
        }
        Some((buf_idx, total - NET_HDR_SIZE))
    }

    fn refill_rx_buf(&mut self, buf_index: usize) {
        if buf_index < RX_BUF_COUNT {
            self.refill_rx(buf_index);
        }
    }

    fn submit_tx(&mut self, total_len: usize) {
        self.txq.submit_and_wait(
            &[(self.tx_buf, total_len as u32, BufDir::Readable)],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            1,
        );
    }
}

pub fn init(ecam_base: u64) {
    let pci_dev = match PciDevice::find_by_id(ecam_base, VIRTIO_VENDOR, VIRTIO_NET_DEVICE) {
        Some(dev) => dev,
        None => {
            log!("VirtIO net: no device found");
            return;
        }
    };
    log!("VirtIO net: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC);

    let mut rxq = Virtqueue::new(dma_addr(PAGE_RXQ));
    let mut txq = Virtqueue::new(dma_addr(PAGE_TXQ));

    device.setup_queue(0, &mut rxq);
    device.setup_queue(1, &mut txq);
    device.activate();

    // Read MAC address from device config space (bytes 0-5)
    let cfg = device.device_config();
    let mac = [
        cfg.read_u8(0), cfg.read_u8(1), cfg.read_u8(2),
        cfg.read_u8(3), cfg.read_u8(4), cfg.read_u8(5),
    ];
    log!("VirtIO net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    let rx_bufs = [
        dma_addr(PAGE_RX_BUFS),
        dma_addr(PAGE_RX_BUFS + 1),
        dma_addr(PAGE_RX_BUFS + 2),
    ];
    let tx_buf = dma_addr(PAGE_TX_BUF);

    // Register DMA buffers as shared memory for direct userland access
    let rx_tokens: [u32; 3] = core::array::from_fn(|i| {
        shared_memory::register(rx_bufs[i], 4096).raw()
    });
    let tx_token = shared_memory::register(tx_buf, 4096).raw();

    crate::net::set_nic_info(NicInfo {
        rx_buf_tokens: rx_tokens,
        tx_buf_token: tx_token,
        mac,
        rx_buf_count: RX_BUF_COUNT as u8,
        net_hdr_size: NET_HDR_SIZE as u8,
    });

    let mut nic = VirtioNic {
        device, rxq, txq, mac, rx_bufs, tx_buf,
        desc_to_buf: [0; 16],
    };

    for i in 0..RX_BUF_COUNT {
        nic.refill_rx(i);
    }

    crate::net::register(Box::new(nic));
    log!("VirtIO net: initialized");
}
