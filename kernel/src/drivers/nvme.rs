use core::ptr::{read_volatile, write_volatile, write_bytes, copy_nonoverlapping};
use core::sync::atomic::{fence, Ordering};
use crate::mm::Mmio;
use super::pci::PciDevice;
use super::DmaPool;
use crate::block::{BlockDevice, DeviceId};
use crate::log;
use crate::mm::KernelSlice;
use crate::sync::Lock;

// NVMe register offsets (BAR0 MMIO)
const REG_CAP: u64 = 0x00;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;

const QUEUE_DEPTH: usize = 16;

/// NVMe Identify Namespace data structure (partial — only fields we use).
#[repr(C)]
struct IdentifyNamespace {
    nsze: u64,            // offset 0: namespace size in LBAs
    ncap: u64,            // offset 8: namespace capacity
    nuse: u64,            // offset 16: namespace utilization
    nsfeat: u8,           // offset 24
    nlbaf: u8,            // offset 25: number of LBA formats (0-based)
    flbas: u8,            // offset 26: formatted LBA size
    _padding: [u8; 101],  // offsets 27..128
    lba_formats: [u32; 64], // offset 128: LBA format descriptors (4 bytes each)
}

// NVMe command opcodes
const ADMIN_CREATE_IO_SQ: u8 = 0x01;
const ADMIN_CREATE_IO_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

#[repr(C)]
#[derive(Clone, Copy)]
struct SqEntry {
    cdw0: u32,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    mptr: u64,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

impl SqEntry {
    const ZERO: Self = Self {
        cdw0: 0, nsid: 0, cdw2: 0, cdw3: 0,
        mptr: 0, prp1: 0, prp2: 0,
        cdw10: 0, cdw11: 0, cdw12: 0, cdw13: 0, cdw14: 0, cdw15: 0,
    };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CqEntry {
    dw0: u32,
    dw1: u32,
    sq_head: u16,
    sq_id: u16,
    cid: u16,
    status: u16, // bit 0 = phase, bits [15:1] = status
}

struct NvmeQueue {
    sq: *mut SqEntry,
    cq: *mut CqEntry,
    sq_tail: u16,
    cq_head: u16,
    phase: bool,
    sq_doorbell: u64,
    cq_doorbell: u64,
}

impl NvmeQueue {
    fn new(sq: *mut SqEntry, cq: *mut CqEntry, qid: u16, stride: u32) -> Self {
        let doorbell_stride = 4u64 << stride;
        Self {
            sq, cq,
            sq_tail: 0, cq_head: 0, phase: true,
            sq_doorbell: 0x1000 + (2 * qid as u64) * doorbell_stride,
            cq_doorbell: 0x1000 + (2 * qid as u64 + 1) * doorbell_stride,
        }
    }

    fn submit(&mut self, bar: &Mmio, cmd: SqEntry) {
        unsafe { write_volatile(self.sq.add(self.sq_tail as usize), cmd); }
        self.sq_tail = (self.sq_tail + 1) % QUEUE_DEPTH as u16;
        fence(Ordering::Release);
        bar.write_u32(self.sq_doorbell, self.sq_tail as u32);
    }

    fn wait_completion(&mut self, bar: &Mmio) -> u16 {
        loop {
            let cq = unsafe { read_volatile(self.cq.add(self.cq_head as usize)) };
            if ((cq.status & 1) != 0) == self.phase {
                let status = cq.status >> 1;
                self.cq_head = (self.cq_head + 1) % QUEUE_DEPTH as u16;
                if self.cq_head == 0 {
                    self.phase = !self.phase;
                }
                bar.write_u32(self.cq_doorbell, self.cq_head as u32);
                return status;
            }
            core::hint::spin_loop();
        }
    }

    fn submit_and_wait(&mut self, bar: &Mmio, cmd: SqEntry) -> u16 {
        self.submit(bar, cmd);
        self.wait_completion(bar)
    }
}

// DMA layout (byte offsets)
const OFF_ADMIN_SQ: usize   = 0x0000;
const OFF_ADMIN_CQ: usize   = 0x1000;
const OFF_IO_SQ: usize      = 0x2000;
const OFF_IO_CQ: usize      = 0x3000;
const OFF_IDENTIFY: usize   = 0x4000;
const OFF_PRP_LIST: usize   = 0x5000;
const OFF_DATA: usize       = 0x6000;
const MAX_DATA_PAGES: usize  = 32;
const DMA_SIZE: usize        = OFF_DATA + MAX_DATA_PAGES * 0x1000;

static DMA_POOL: Lock<Option<DmaPool>> = Lock::new(None);

fn dma() -> KernelSlice {
    DMA_POOL.lock().as_ref().unwrap().slice()
}

struct NvmeController {
    bar: Mmio,
    admin: NvmeQueue,
    io: NvmeQueue,
    next_cid: u16,
    sector_size: u32,
    ns_size: u64,
}

impl NvmeController {
    fn alloc_cid(&mut self) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cid
    }

    fn identify_controller(&mut self) {
        let dma = dma();
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_IDENTIFY as u32;
        cmd.prp1 = dma.phys() + OFF_IDENTIFY as u64;
        cmd.cdw10 = 1;
        self.admin.submit_and_wait(&self.bar, cmd);
        log!("NVMe: Identify Controller OK");
    }

    fn create_io_cq(&mut self) {
        unsafe { write_bytes(self.io.cq as *mut u8, 0, QUEUE_DEPTH * core::mem::size_of::<CqEntry>()); }
        let dma = dma();
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_CREATE_IO_CQ as u32;
        cmd.prp1 = dma.phys() + OFF_IO_CQ as u64;
        cmd.cdw10 = ((QUEUE_DEPTH as u32 - 1) << 16) | 1;
        cmd.cdw11 = 1;
        self.admin.submit_and_wait(&self.bar, cmd);
    }

    fn create_io_sq(&mut self) {
        unsafe { write_bytes(self.io.sq as *mut u8, 0, QUEUE_DEPTH * core::mem::size_of::<SqEntry>()); }
        let dma = dma();
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_CREATE_IO_SQ as u32;
        cmd.prp1 = dma.phys() + OFF_IO_SQ as u64;
        cmd.cdw10 = ((QUEUE_DEPTH as u32 - 1) << 16) | 1;
        cmd.cdw11 = (1 << 16) | 1;
        self.admin.submit_and_wait(&self.bar, cmd);
    }

    fn identify_namespace(&mut self) {
        let dma = dma();
        let identify_ptr = dma.ptr_at(OFF_IDENTIFY);
        unsafe { write_bytes(identify_ptr, 0, 4096); }
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_IDENTIFY as u32;
        cmd.nsid = 1;
        cmd.prp1 = dma.phys() + OFF_IDENTIFY as u64;
        cmd.cdw10 = 0;
        self.admin.submit_and_wait(&self.bar, cmd);

        let ns = unsafe { &*(identify_ptr as *const IdentifyNamespace) };
        let fmt_idx = (ns.flbas & 0x0F) as usize;
        let lba_ds = ((ns.lba_formats[fmt_idx] >> 16) & 0xFF) as u32;
        self.sector_size = 1 << lba_ds;
        self.ns_size = ns.nsze;
        log!("NVMe: NS1 size={} sectors, sector_size={}", ns.nsze, self.sector_size);
    }

    /// Read `sector_count` contiguous sectors starting at `lba` into `buf`.
    /// Handles PRP list setup for multi-page transfers.
    fn read_sectors(&mut self, lba: u64, sector_count: u32, buf: &mut [u8]) {
        let total_bytes = sector_count as usize * self.sector_size as usize;
        assert!(buf.len() >= total_bytes);
        assert!(total_bytes <= MAX_DATA_PAGES * 4096);

        let dma = dma();
        let pages = (total_bytes + 4095) / 4096;
        let data_phys = dma.phys() + OFF_DATA as u64;

        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | IO_READ as u32;
        cmd.nsid = 1;
        cmd.prp1 = data_phys;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = sector_count - 1;

        if pages == 2 {
            cmd.prp2 = data_phys + 0x1000;
        } else if pages > 2 {
            let prp_list = dma.ptr_at(OFF_PRP_LIST) as *mut u64;
            for i in 1..pages {
                unsafe { prp_list.add(i - 1).write(data_phys + i as u64 * 0x1000); }
            }
            cmd.prp2 = dma.phys() + OFF_PRP_LIST as u64;
        }

        self.io.submit_and_wait(&self.bar, cmd);

        unsafe { copy_nonoverlapping(dma.ptr_at(OFF_DATA) as *const u8, buf.as_mut_ptr(), total_bytes); }
    }

    fn write_sectors(&mut self, lba: u64, sector_count: u32, buf: &[u8]) {
        let total_bytes = sector_count as usize * self.sector_size as usize;
        assert!(buf.len() >= total_bytes);
        assert!(total_bytes <= MAX_DATA_PAGES * 4096);

        let dma = dma();
        let pages = (total_bytes + 4095) / 4096;
        let data_phys = dma.phys() + OFF_DATA as u64;

        unsafe { copy_nonoverlapping(buf.as_ptr(), dma.ptr_at(OFF_DATA), total_bytes); }

        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | IO_WRITE as u32;
        cmd.nsid = 1;
        cmd.prp1 = data_phys;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = sector_count - 1;

        if pages == 2 {
            cmd.prp2 = data_phys + 0x1000;
        } else if pages > 2 {
            let prp_list = dma.ptr_at(OFF_PRP_LIST) as *mut u64;
            for i in 1..pages {
                unsafe { prp_list.add(i - 1).write(data_phys + i as u64 * 0x1000); }
            }
            cmd.prp2 = dma.phys() + OFF_PRP_LIST as u64;
        }

        self.io.submit_and_wait(&self.bar, cmd);
    }
}

/// NVMe block device exposing 4KB block I/O through the BlockDevice trait.
///
/// # Safety
/// Raw pointers in NvmeController point to DMA memory owned by this device.
/// The device is only accessed by a single owner (the VFS root filesystem).
unsafe impl Send for NvmeBlockDevice {}

pub struct NvmeBlockDevice {
    ctrl: NvmeController,
    id: DeviceId,
    sectors_per_block: u32,
    block_count: u64,
}

impl NvmeBlockDevice {
    fn new(ctrl: NvmeController, id: DeviceId) -> Self {
        let sectors_per_block = 4096 / ctrl.sector_size;
        let block_count = ctrl.ns_size / sectors_per_block as u64;
        log!("NVMe: block device id={} blocks={} ({}MB)",
            id, block_count, block_count * 4096 / (1024 * 1024));
        Self { ctrl, id, sectors_per_block, block_count }
    }
}

impl BlockDevice for NvmeBlockDevice {
    fn device_id(&self) -> DeviceId { self.id }
    fn block_count(&self) -> u64 { self.block_count }

    fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]) {
        assert_eq!(buf.len(), count as usize * 4096);
        let mut remaining = count;
        let mut block = lba;
        let mut offset = 0usize;

        while remaining > 0 {
            let batch = remaining.min(MAX_DATA_PAGES as u32);
            let sector_lba = block * self.sectors_per_block as u64;
            let sector_count = batch * self.sectors_per_block;
            let bytes = batch as usize * 4096;

            self.ctrl.read_sectors(sector_lba, sector_count, &mut buf[offset..offset + bytes]);

            block += batch as u64;
            offset += bytes;
            remaining -= batch;
        }
    }

    fn write_blocks(&mut self, lba: u64, count: u32, buf: &[u8]) {
        assert_eq!(buf.len(), count as usize * 4096);
        let mut remaining = count;
        let mut block = lba;
        let mut offset = 0usize;

        while remaining > 0 {
            let batch = remaining.min(MAX_DATA_PAGES as u32);
            let sector_lba = block * self.sectors_per_block as u64;
            let sector_count = batch * self.sectors_per_block;
            let bytes = batch as usize * 4096;

            self.ctrl.write_sectors(sector_lba, sector_count, &buf[offset..offset + bytes]);

            block += batch as u64;
            offset += bytes;
            remaining -= batch;
        }
    }

    fn flush(&mut self) {
        // NVMe writes are synchronous (submit_and_wait), so data is on disk
        // after write_blocks returns. Nothing to flush.
    }
}

pub fn init(ecam: &crate::mm::Mmio) -> Option<NvmeBlockDevice> {
    let pci_dev = PciDevice::find(ecam, 0x01, 0x08, None)?;
    log!("NVMe: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    *DMA_POOL.lock() = Some(DmaPool::alloc(DMA_SIZE));

    let bar_addr = pci_dev.read_bar_64(0);
    pci_dev.enable_bus_master();
    log!("NVMe: BAR0={:#x}", bar_addr);

    let bar = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(bar_addr, 0x4000);

    let cap = bar.read_u64(REG_CAP);
    let stride = ((cap >> 32) & 0xF) as u32;

    // Disable controller
    let cc = bar.read_u32(REG_CC);
    if cc & 1 != 0 {
        bar.write_u32(REG_CC, cc & !1);
        while bar.read_u32(REG_CSTS) & 1 != 0 {
            core::hint::spin_loop();
        }
    }

    let dma = dma();
    let admin_sq = dma.ptr_at(OFF_ADMIN_SQ) as *mut SqEntry;
    let admin_cq = dma.ptr_at(OFF_ADMIN_CQ) as *mut CqEntry;
    let io_sq = dma.ptr_at(OFF_IO_SQ) as *mut SqEntry;
    let io_cq = dma.ptr_at(OFF_IO_CQ) as *mut CqEntry;

    unsafe {
        write_bytes(admin_sq as *mut u8, 0, 4096);
        write_bytes(admin_cq as *mut u8, 0, 4096);
    }

    let aqa = ((QUEUE_DEPTH as u32 - 1) << 16) | (QUEUE_DEPTH as u32 - 1);
    bar.write_u32(REG_AQA, aqa);
    bar.write_u64(REG_ASQ, dma.phys() + OFF_ADMIN_SQ as u64);
    bar.write_u64(REG_ACQ, dma.phys() + OFF_ADMIN_CQ as u64);

    let cc = 1 | (6 << 16) | (4 << 20);
    bar.write_u32(REG_CC, cc);

    while bar.read_u32(REG_CSTS) & 1 == 0 {
        core::hint::spin_loop();
    }
    log!("NVMe: controller enabled");

    let mut ctrl = NvmeController {
        bar,
        admin: NvmeQueue::new(admin_sq, admin_cq, 0, stride),
        io: NvmeQueue::new(io_sq, io_cq, 1, stride),
        next_cid: 0,
        sector_size: 512,
        ns_size: 0,
    };

    ctrl.identify_controller();
    ctrl.create_io_cq();
    ctrl.create_io_sq();
    ctrl.identify_namespace();

    Some(NvmeBlockDevice::new(ctrl, 1))
}
