use alloc::boxed::Box;
use core::ptr::{copy_nonoverlapping, write_bytes};

use super::pci::PciDevice;
use super::virtio::{BufDir, DescSlot, Virtqueue, VirtqueueRegions, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::log;
use crate::mm::KernelSlice;
use crate::net::NicInfo;
use crate::shared_memory;
use crate::sync::Lock;

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_NET_DEVICE: u16 = 0x1041; // 0x1040 + device_id 1

const VIRTIO_NET_F_MAC: u64 = 1 << 5;

// VirtIO 1.0 net header: always 12 bytes (includes num_buffers) with VERSION_1
const NET_HDR_SIZE: usize = 12;

const RX_QUEUE_SIZE: u16 = 256;
const RX_BUF_COUNT: usize = 256;
const RX_BUF_SIZE: u32 = 4096;

// DMA layout (byte offsets, 4KB-aligned):
const OFF_RXQ_DESC: usize  = 0x0000;
const OFF_RXQ_AVAIL: usize = 0x1000;
const OFF_RXQ_USED: usize  = 0x2000;
const OFF_TXQ: usize       = 0x3000;
const OFF_RX_BUFS: usize   = 0x4000;  // 256 × 4KB
const OFF_TX_BUF: usize    = 0x4000 + 256 * 0x1000;
const DMA_SIZE: usize       = OFF_TX_BUF + 0x1000;

const PCI_CAP_MSIX: u8 = 0x11;
const VIRTIO_NET_VECTOR: u8 = 0x22;

static DMA: Lock<Option<DmaPool>> = Lock::new(None);

fn dma() -> KernelSlice {
    DMA.lock().as_ref().unwrap().slice()
}

struct VirtioNic {
    device: VirtioDevice,
    rxq: Virtqueue,
    txq: Virtqueue,
    mac: [u8; 6],
    /// Physical addresses for device DMA descriptors.
    rx_phys: [u64; RX_BUF_COUNT],
    tx_phys: u64,
    /// Virtual pointers for kernel read/write.
    rx_ptrs: [*mut u8; RX_BUF_COUNT],
    tx_ptr: *mut u8,
    // Maps virtqueue descriptor index -> rx_bufs index
    desc_to_buf: [u16; RX_QUEUE_SIZE as usize],
    /// Stash area: slot returned by poll_used, indexed by buf_idx, consumed by refill_rx_buf.
    pending_rx_slots: [Option<DescSlot>; RX_BUF_COUNT],
    tx_slot: Option<DescSlot>,
}

unsafe impl Send for VirtioNic {}

impl VirtioNic {
    fn refill_rx(&mut self, buf_idx: usize, slot: DescSlot) {
        unsafe { write_bytes(self.rx_ptrs[buf_idx], 0, NET_HDR_SIZE); }
        let desc_id = self.rxq.submit(
            slot,
            &[(self.rx_phys[buf_idx], RX_BUF_SIZE, BufDir::Writable)],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0,
        );
        self.desc_to_buf[desc_id as usize] = buf_idx as u16;
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
            write_bytes(self.tx_ptr, 0, NET_HDR_SIZE);
            copy_nonoverlapping(
                frame.as_ptr(),
                self.tx_ptr.add(NET_HDR_SIZE),
                len,
            );
        }

        let slot = self.tx_slot.take().expect("virtio-net: no tx slot");
        self.tx_slot = Some(self.txq.submit_and_wait(
            slot,
            &[(self.tx_phys, (NET_HDR_SIZE + len) as u32, BufDir::Readable)],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            1,
        ));
    }

    fn has_packet(&self) -> bool {
        self.rxq.has_used()
    }

    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let (slot, written_len) = self.rxq.poll_used()?;
        let buf_idx = self.desc_to_buf[slot.id() as usize] as usize;
        let total = written_len as usize;
        if total <= NET_HDR_SIZE {
            self.refill_rx(buf_idx, slot);
            return None;
        }
        let frame_len = total - NET_HDR_SIZE;
        let copy_len = frame_len.min(buf.len());
        let src = unsafe { self.rx_ptrs[buf_idx].add(NET_HDR_SIZE) };
        unsafe {
            copy_nonoverlapping(src, buf.as_mut_ptr(), copy_len);
        }
        self.refill_rx(buf_idx, slot);
        Some(copy_len)
    }

    fn poll_rx(&mut self) -> Option<(usize, usize)> {
        let (slot, written_len) = self.rxq.poll_used()?;
        let buf_idx = self.desc_to_buf[slot.id() as usize] as usize;
        let total = written_len as usize;
        if total <= NET_HDR_SIZE {
            self.refill_rx(buf_idx, slot);
            return None;
        }
        // Stash the slot for refill_rx_buf to consume later
        self.pending_rx_slots[buf_idx] = Some(slot);
        Some((buf_idx, total - NET_HDR_SIZE))
    }

    fn refill_rx_buf(&mut self, buf_index: usize) {
        if buf_index < RX_BUF_COUNT {
            let slot = self.pending_rx_slots[buf_index].take()
                .expect("refill_rx_buf: no pending slot (poll_rx not called for this buf_index)");
            self.refill_rx(buf_index, slot);
        }
    }

    fn submit_tx(&mut self, total_len: usize) {
        let slot = self.tx_slot.take().expect("virtio-net: no tx slot");
        self.tx_slot = Some(self.txq.submit_and_wait(
            slot,
            &[(self.tx_phys, total_len as u32, BufDir::Readable)],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            1,
        ));
    }
}

fn setup_msix(pci_dev: &PciDevice, device: &super::virtio::VirtioDevice) {
    let cap = match pci_dev.capabilities().find(|c| c.id() == PCI_CAP_MSIX) {
        Some(c) => c,
        None => panic!("VirtIO net: no MSI-X capability"),
    };

    let table_info = cap.read_u32(4);
    let table_bir = (table_info & 0x7) as u8;
    let table_offset = (table_info & !0x7) as u64;
    let table_bar = pci_dev.read_bar_64(table_bir);
    let table_addr = table_bar + table_offset;

    let table = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(table_addr, 0x1000);

    // Configure MSI-X table entry 0: route to LAPIC with our vector
    table.write_u32(0x00, 0xFEE0_0000); // msg_addr_lo: LAPIC base
    table.write_u32(0x04, 0);            // msg_addr_hi
    table.write_u32(0x08, VIRTIO_NET_VECTOR as u32); // msg_data: vector
    table.write_u32(0x0C, 0);            // vector control: unmask

    // Enable MSI-X in PCI capability
    let msg_ctrl = cap.read_u16(2);
    cap.write_u16(2, (msg_ctrl | (1 << 15)) & !(1 << 14));

    use super::virtio::{COMMON_MSIX_CONFIG, COMMON_QUEUE_SELECT, COMMON_QUEUE_MSIX};
    let common = device.common_config();

    common.write_u16(COMMON_MSIX_CONFIG, 0);
    let config_vec = common.read_u16(COMMON_MSIX_CONFIG);
    if config_vec == 0xFFFF {
        panic!("VirtIO net: MSI-X config vector assignment failed");
    }

    // Set RX queue (0) MSI-X vector. queue_enable is called separately after.
    common.write_u16(COMMON_QUEUE_SELECT, 0);
    common.write_u16(COMMON_QUEUE_MSIX, 0);
    let queue_vec = common.read_u16(COMMON_QUEUE_MSIX);
    if queue_vec == 0xFFFF {
        panic!("VirtIO net: MSI-X queue vector assignment failed");
    }

    log!("VirtIO net: MSI-X enabled (vector {:#x}, config_vec={}, queue_vec={})",
        VIRTIO_NET_VECTOR, config_vec, queue_vec);
}

pub fn init(ecam: &crate::mm::Mmio) {
    let pci_dev = match PciDevice::find_by_id(ecam, VIRTIO_VENDOR, VIRTIO_NET_DEVICE) {
        Some(dev) => dev,
        None => {
            log!("VirtIO net: no device found");
            return;
        }
    };
    log!("VirtIO net: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    *DMA.lock() = Some(DmaPool::alloc(DMA_SIZE));
    let dma = dma();
    pci_dev.enable_bus_master();

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC);

    // RX queue: 256 entries, separate pages for desc/avail/used
    let rxq_regions = VirtqueueRegions::from_separate(
        dma.subslice(OFF_RXQ_DESC, 0x1000),
        dma.subslice(OFF_RXQ_AVAIL, 0x1000),
        dma.subslice(OFF_RXQ_USED, 0x1000),
        RX_QUEUE_SIZE,
    );
    let mut rxq = Virtqueue::from_regions(&rxq_regions, RX_QUEUE_SIZE);

    // TX queue: 16 entries, fits in one page
    let mut txq = Virtqueue::new(dma.subslice(OFF_TXQ, 0x1000), 16);

    device.setup_queue(0, &mut rxq);
    device.setup_queue(1, &mut txq);
    setup_msix(&pci_dev, &device);
    device.enable_queue(0);
    device.enable_queue(1);
    device.activate();

    // Read MAC address from device config space (bytes 0-5)
    let cfg = device.device_config();
    let mac = [
        cfg.read_u8(0), cfg.read_u8(1), cfg.read_u8(2),
        cfg.read_u8(3), cfg.read_u8(4), cfg.read_u8(5),
    ];
    log!("VirtIO net: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);

    let rx_phys: [u64; RX_BUF_COUNT] = core::array::from_fn(|i| {
        dma.phys() + (OFF_RX_BUFS + i * 0x1000) as u64
    });
    let rx_ptrs: [*mut u8; RX_BUF_COUNT] = core::array::from_fn(|i| {
        dma.ptr_at(OFF_RX_BUFS + i * 0x1000)
    });
    let tx_phys = dma.phys() + OFF_TX_BUF as u64;
    let tx_ptr = dma.ptr_at(OFF_TX_BUF);

    let dma_base_phys = dma.phys() & !(crate::mm::PAGE_2M - 1);
    let dma_token = shared_memory::register(crate::DirectMap::from_phys(dma_base_phys), crate::mm::PAGE_2M);

    crate::net::set_nic_info(NicInfo {
        dma_token: dma_token.raw(),
        rx_buf_offset: OFF_RX_BUFS as u32,
        tx_buf_offset: OFF_TX_BUF as u32,
        mac,
        rx_buf_count: RX_BUF_COUNT as u16,
        rx_buf_size: RX_BUF_SIZE as u16,
        net_hdr_size: NET_HDR_SIZE as u16,
    });

    let mut rx_slots = rxq.initial_slots();
    let mut tx_slots = txq.initial_slots();
    let tx_slot = tx_slots.pop().expect("virtio-net: no tx slots");
    drop(tx_slots);

    const NONE_SLOT: Option<DescSlot> = None;
    let mut nic = VirtioNic {
        device, rxq, txq, mac, rx_phys, tx_phys, rx_ptrs, tx_ptr,
        desc_to_buf: [0; RX_QUEUE_SIZE as usize],
        pending_rx_slots: [NONE_SLOT; RX_BUF_COUNT],
        tx_slot: Some(tx_slot),
    };

    for i in 0..RX_BUF_COUNT {
        let slot = rx_slots.pop().expect("virtio-net: not enough rx slots");
        nic.refill_rx(i, slot);
    }

    crate::net::register(Box::new(nic));
    log!("VirtIO net: {} RX buffers, queue size {}", RX_BUF_COUNT, RX_QUEUE_SIZE);
}
