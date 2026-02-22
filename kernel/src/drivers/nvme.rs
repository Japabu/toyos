use core::ptr::{read_volatile, write_volatile, write_bytes, copy_nonoverlapping};
use core::sync::atomic::{fence, Ordering};
use super::mmio::Mmio;
use super::pci::PciDevice;
use super::DmaPool;
use crate::log;
use crate::sync::SyncCell;

// NVMe register offsets (BAR0 MMIO)
const REG_CAP: u64 = 0x00;
const REG_CC: u64 = 0x14;
const REG_CSTS: u64 = 0x1C;
const REG_AQA: u64 = 0x24;
const REG_ASQ: u64 = 0x28;
const REG_ACQ: u64 = 0x30;

const QUEUE_DEPTH: usize = 16;

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

// DMA memory pool
//   Page 0: Admin SQ (16 * 64 = 1024 bytes)
//   Page 1: Admin CQ (16 * 16 = 256 bytes)
//   Page 2: I/O SQ
//   Page 3: I/O CQ
//   Page 4: Identify buffer (4096 bytes)
//   Page 5: Data buffer (4096 bytes)
const DMA_PAGES: usize = 6;
static DMA_POOL: SyncCell<DmaPool<DMA_PAGES>> = SyncCell::new(DmaPool::new());

fn dma_page(index: usize) -> u64 {
    DMA_POOL.get().page_addr(index)
}

pub struct NvmeController {
    bar: Mmio,
    stride: u32,
    admin_sq: *mut SqEntry,
    admin_cq: *mut CqEntry,
    io_sq: *mut SqEntry,
    io_cq: *mut CqEntry,
    data_buf: *mut u8,
    admin_sq_tail: u16,
    admin_cq_head: u16,
    admin_phase: bool,
    io_sq_tail: u16,
    io_cq_head: u16,
    io_phase: bool,
    next_cid: u16,
    sector_size: u32,
    ns_size: u64, // namespace size in sectors
}

impl NvmeController {
    fn sq_doorbell_offset(&self, qid: u16) -> u64 {
        0x1000 + (2 * qid as u64) * (4 << self.stride)
    }

    fn cq_doorbell_offset(&self, qid: u16) -> u64 {
        0x1000 + (2 * qid as u64 + 1) * (4 << self.stride)
    }

    fn submit_admin_cmd(&mut self, cmd: SqEntry) {
        unsafe {
            write_volatile(self.admin_sq.add(self.admin_sq_tail as usize), cmd);
        }
        self.admin_sq_tail = (self.admin_sq_tail + 1) % QUEUE_DEPTH as u16;
        fence(Ordering::Release);
        self.bar.write_u32(self.sq_doorbell_offset(0), self.admin_sq_tail as u32);
    }

    fn wait_admin_completion(&mut self) -> u16 {
        loop {
            let cq = unsafe {
                read_volatile(self.admin_cq.add(self.admin_cq_head as usize))
            };
            let phase = (cq.status & 1) != 0;
            if phase == self.admin_phase {
                let status = cq.status >> 1;
                self.admin_cq_head = (self.admin_cq_head + 1) % QUEUE_DEPTH as u16;
                if self.admin_cq_head == 0 {
                    self.admin_phase = !self.admin_phase;
                }
                self.bar.write_u32(self.cq_doorbell_offset(0), self.admin_cq_head as u32);
                if status != 0 {
                    log!("NVMe: admin cmd failed, status={:#x}", status);
                }
                return status;
            }
            core::hint::spin_loop();
        }
    }

    fn submit_io_cmd(&mut self, cmd: SqEntry) {
        unsafe {
            write_volatile(self.io_sq.add(self.io_sq_tail as usize), cmd);
        }
        self.io_sq_tail = (self.io_sq_tail + 1) % QUEUE_DEPTH as u16;
        fence(Ordering::Release);
        self.bar.write_u32(self.sq_doorbell_offset(1), self.io_sq_tail as u32);
    }

    fn wait_io_completion(&mut self) -> u16 {
        loop {
            let cq = unsafe {
                read_volatile(self.io_cq.add(self.io_cq_head as usize))
            };
            let phase = (cq.status & 1) != 0;
            if phase == self.io_phase {
                let status = cq.status >> 1;
                self.io_cq_head = (self.io_cq_head + 1) % QUEUE_DEPTH as u16;
                if self.io_cq_head == 0 {
                    self.io_phase = !self.io_phase;
                }
                self.bar.write_u32(self.cq_doorbell_offset(1), self.io_cq_head as u32);
                if status != 0 {
                    log!("NVMe: I/O cmd failed, status={:#x}", status);
                }
                return status;
            }
            core::hint::spin_loop();
        }
    }

    fn alloc_cid(&mut self) -> u16 {
        let cid = self.next_cid;
        self.next_cid = self.next_cid.wrapping_add(1);
        cid
    }

    fn identify_controller(&mut self) {
        let identify_buf = dma_page(4);
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_IDENTIFY as u32;
        cmd.prp1 = identify_buf;
        cmd.cdw10 = 1; // CNS = 1 (controller)
        self.submit_admin_cmd(cmd);
        self.wait_admin_completion();
        log::println("NVMe: Identify Controller OK");
    }

    fn create_io_cq(&mut self) {
        let io_cq_phys = self.io_cq as u64;
        unsafe { write_bytes(self.io_cq as *mut u8, 0, QUEUE_DEPTH * core::mem::size_of::<CqEntry>()); }
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_CREATE_IO_CQ as u32;
        cmd.prp1 = io_cq_phys;
        cmd.cdw10 = ((QUEUE_DEPTH as u32 - 1) << 16) | 1; // size (0-based) | QID=1
        cmd.cdw11 = 1; // physically contiguous
        self.submit_admin_cmd(cmd);
        self.wait_admin_completion();
        log::println("NVMe: I/O CQ created");
    }

    fn create_io_sq(&mut self) {
        let io_sq_phys = self.io_sq as u64;
        unsafe { write_bytes(self.io_sq as *mut u8, 0, QUEUE_DEPTH * core::mem::size_of::<SqEntry>()); }
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_CREATE_IO_SQ as u32;
        cmd.prp1 = io_sq_phys;
        cmd.cdw10 = ((QUEUE_DEPTH as u32 - 1) << 16) | 1; // size (0-based) | QID=1
        cmd.cdw11 = (1 << 16) | 1; // CQID=1 | physically contiguous
        self.submit_admin_cmd(cmd);
        self.wait_admin_completion();
        log::println("NVMe: I/O SQ created");
    }

    fn identify_namespace(&mut self) {
        let identify_buf = dma_page(4);
        unsafe { write_bytes(identify_buf as *mut u8, 0, 4096); }
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | ADMIN_IDENTIFY as u32;
        cmd.nsid = 1;
        cmd.prp1 = identify_buf;
        cmd.cdw10 = 0; // CNS = 0 (namespace)
        self.submit_admin_cmd(cmd);
        self.wait_admin_completion();

        unsafe {
            let buf = identify_buf as *const u8;
            // NSZE (namespace size in LBAs) at offset 0, 8 bytes
            let nsze = core::ptr::read_unaligned(buf as *const u64);
            // FLBAS at offset 26
            let flbas = read_volatile(buf.add(26));
            let fmt_idx = (flbas & 0x0F) as usize;
            // LBA format table at offset 128, 4 bytes each; LBA data size power-of-2 in bits [23:16]
            let lba_fmt = core::ptr::read_unaligned(buf.add(128 + fmt_idx * 4) as *const u32);
            let lba_ds = ((lba_fmt >> 16) & 0xFF) as u32;
            self.sector_size = 1 << lba_ds;
            self.ns_size = nsze;
            log!("NVMe: NS1 size={} sectors, sector_size={}", nsze, self.sector_size);
        }
    }

    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    pub fn total_bytes(&self) -> u64 {
        self.ns_size * self.sector_size as u64
    }

    pub fn read(&mut self, lba: u64, buf: &mut [u8]) {
        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | IO_READ as u32;
        cmd.nsid = 1;
        cmd.prp1 = self.data_buf as u64;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = 0; // read 1 sector (NLB is 0-based)
        self.submit_io_cmd(cmd);
        self.wait_io_completion();

        let len = buf.len().min(self.sector_size as usize);
        unsafe {
            copy_nonoverlapping(self.data_buf, buf.as_mut_ptr(), len);
        }
    }

    pub fn write(&mut self, lba: u64, buf: &[u8]) {
        let len = buf.len().min(self.sector_size as usize);
        unsafe {
            write_bytes(self.data_buf, 0, self.sector_size as usize);
            copy_nonoverlapping(buf.as_ptr(), self.data_buf, len);
        }

        let cid = self.alloc_cid();
        let mut cmd = SqEntry::ZERO;
        cmd.cdw0 = (cid as u32) << 16 | IO_WRITE as u32;
        cmd.nsid = 1;
        cmd.prp1 = self.data_buf as u64;
        cmd.cdw10 = lba as u32;
        cmd.cdw11 = (lba >> 32) as u32;
        cmd.cdw12 = 0; // write 1 sector (NLB is 0-based)
        self.submit_io_cmd(cmd);
        self.wait_io_completion();
    }
}

/// Byte-addressable disk backed by an NVMe controller, with single-sector write cache.
pub struct NvmeDisk {
    ctrl: NvmeController,
    sector_size: u64,
    cache_lba: Option<u64>,
    cache_buf: alloc::vec::Vec<u8>,
    cache_dirty: bool,
}

impl NvmeDisk {
    pub fn new(ctrl: NvmeController) -> Self {
        let sector_size = ctrl.sector_size() as u64;
        Self {
            cache_buf: alloc::vec![0u8; sector_size as usize],
            ctrl,
            sector_size,
            cache_lba: None,
            cache_dirty: false,
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.ctrl.total_bytes()
    }

    fn ensure_sector(&mut self, lba: u64) {
        if self.cache_lba == Some(lba) {
            return;
        }
        self.flush_cache();
        self.ctrl.read(lba, &mut self.cache_buf);
        self.cache_lba = Some(lba);
        self.cache_dirty = false;
    }

    fn flush_cache(&mut self) {
        if self.cache_dirty {
            if let Some(lba) = self.cache_lba {
                self.ctrl.write(lba, &self.cache_buf);
                self.cache_dirty = false;
            }
        }
    }
}

impl tyfs::Disk for NvmeDisk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) {
        let mut remaining = buf.len() as u64;
        let mut pos = offset;
        let mut buf_off: usize = 0;

        while remaining > 0 {
            let lba = pos / self.sector_size;
            let sector_off = (pos % self.sector_size) as usize;
            let chunk = core::cmp::min(remaining, self.sector_size - sector_off as u64) as usize;

            self.ensure_sector(lba);
            buf[buf_off..buf_off + chunk]
                .copy_from_slice(&self.cache_buf[sector_off..sector_off + chunk]);

            pos += chunk as u64;
            buf_off += chunk;
            remaining -= chunk as u64;
        }
    }

    fn write(&mut self, offset: u64, buf: &[u8]) {
        let mut remaining = buf.len() as u64;
        let mut pos = offset;
        let mut buf_off: usize = 0;

        while remaining > 0 {
            let lba = pos / self.sector_size;
            let sector_off = (pos % self.sector_size) as usize;
            let chunk = core::cmp::min(remaining, self.sector_size - sector_off as u64) as usize;

            self.ensure_sector(lba);
            self.cache_buf[sector_off..sector_off + chunk]
                .copy_from_slice(&buf[buf_off..buf_off + chunk]);
            self.cache_dirty = true;

            pos += chunk as u64;
            buf_off += chunk;
            remaining -= chunk as u64;
        }
    }

    fn flush(&mut self) {
        self.flush_cache();
    }
}

pub fn init(ecam_base: u64) -> Option<NvmeController> {
    // Find NVMe controller (class=01 Mass Storage, subclass=08 NVM)
    let pci_dev = PciDevice::find(ecam_base, 0x01, 0x08, None)?;
    log!("NVMe: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    // Read BAR0 and enable bus mastering
    let bar = Mmio::new(pci_dev.bar0_64());
    pci_dev.enable_bus_master();
    log!("NVMe: BAR0={:#x}", bar.addr());

    // Map BAR MMIO region into our page tables
    crate::arch::paging::map_kernel(bar.addr(), 0x4000); // NVMe register space

    // Read capabilities
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
    log::println("NVMe: controller disabled");

    // Set up DMA pointers (page-aligned from static pool)
    let admin_sq = dma_page(0) as *mut SqEntry;
    let admin_cq = dma_page(1) as *mut CqEntry;
    let io_sq = dma_page(2) as *mut SqEntry;
    let io_cq = dma_page(3) as *mut CqEntry;
    let data_buf = dma_page(5) as *mut u8;

    // Zero admin queue memory
    unsafe {
        write_bytes(admin_sq as *mut u8, 0, 4096);
        write_bytes(admin_cq as *mut u8, 0, 4096);
    }

    // Set admin queue attributes (ACQS | ASQS, both 0-based)
    let aqa = ((QUEUE_DEPTH as u32 - 1) << 16) | (QUEUE_DEPTH as u32 - 1);
    bar.write_u32(REG_AQA, aqa);

    // Set admin queue base addresses (physical = virtual, identity mapped)
    bar.write_u64(REG_ASQ, admin_sq as u64);
    bar.write_u64(REG_ACQ, admin_cq as u64);
    log!("NVMe: ASQ={:#x} ACQ={:#x}", admin_sq as u64, admin_cq as u64);

    // Enable controller: EN=1, CSS=0 (NVM), MPS=0 (4KB), IOSQES=6 (64B), IOCQES=4 (16B)
    let cc = 1 | (6 << 16) | (4 << 20);
    bar.write_u32(REG_CC, cc);

    // Wait for ready
    while bar.read_u32(REG_CSTS) & 1 == 0 {
        core::hint::spin_loop();
    }
    log::println("NVMe: controller enabled");

    let mut ctrl = NvmeController {
        bar,
        stride,
        admin_sq,
        admin_cq,
        io_sq,
        io_cq,
        data_buf,
        admin_sq_tail: 0,
        admin_cq_head: 0,
        admin_phase: true,
        io_sq_tail: 0,
        io_cq_head: 0,
        io_phase: true,
        next_cid: 0,
        sector_size: 512, // default, overwritten by identify_namespace
        ns_size: 0,
    };

    ctrl.identify_controller();
    ctrl.create_io_cq();
    ctrl.create_io_sq();
    ctrl.identify_namespace();

    Some(ctrl)
}
