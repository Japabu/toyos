use core::ptr::{read_volatile, write_volatile, write_bytes};
use core::sync::atomic::{fence, Ordering};

use super::mmio::Mmio;
use super::pci::PciDevice;
use crate::arch::paging;
use crate::log;

// VirtIO PCI capability types
const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// Vendor-specific PCI capability ID
const PCI_CAP_ID_VENDOR: u8 = 0x09;

// Device status bits
const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

// Feature bits
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// Common config field offsets (virtio_pci_common_cfg)
const COMMON_DEVICE_FEATURE_SELECT: u64 = 0x00;
const COMMON_DEVICE_FEATURE: u64 = 0x04;
const COMMON_DRIVER_FEATURE_SELECT: u64 = 0x08;
const COMMON_DRIVER_FEATURE: u64 = 0x0C;
const COMMON_DEVICE_STATUS: u64 = 0x14;
const COMMON_QUEUE_SELECT: u64 = 0x16;
const COMMON_QUEUE_SIZE: u64 = 0x18;
const COMMON_QUEUE_ENABLE: u64 = 0x1C;
const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1E;
const COMMON_QUEUE_DESC: u64 = 0x20;
const COMMON_QUEUE_DRIVER: u64 = 0x28;
const COMMON_QUEUE_DEVICE: u64 = 0x30;

// Virtqueue descriptor flags
const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const QUEUE_SIZE: u16 = 16;

// Virtqueue layout offsets within a DMA page
const DESC_OFFSET: u64 = 0x000;
const AVAIL_OFFSET: u64 = 0x100;
const USED_OFFSET: u64 = 0x200;

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE as usize],
}

#[repr(C)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; QUEUE_SIZE as usize],
}

/// Parsed VirtIO PCI capability locations.
struct VirtioPciConfig {
    common: Mmio,
    notify: Mmio,
    notify_off_multiplier: u32,
    #[allow(dead_code)] // parsed from spec, used for interrupt-based operation
    isr: Mmio,
    device: Mmio,
}

impl VirtioPciConfig {
    fn parse(pci_dev: &PciDevice) -> Self {
        let mut common = None;
        let mut notify = None;
        let mut notify_off_multiplier = 0u32;
        let mut isr = None;
        let mut device = None;

        for cap in pci_dev.capabilities() {
            if cap.id() != PCI_CAP_ID_VENDOR {
                continue;
            }
            let cfg_type = cap.read_u8(3);
            let bar_idx = cap.read_u8(4);
            let offset = cap.read_u32(8) as u64;

            let bar_addr = pci_dev.read_bar_64(bar_idx);
            let mmio = Mmio::new(bar_addr + offset);

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if common.is_none() => common = Some(mmio),
                VIRTIO_PCI_CAP_NOTIFY_CFG if notify.is_none() => {
                    notify = Some(mmio);
                    notify_off_multiplier = cap.read_u32(16);
                }
                VIRTIO_PCI_CAP_ISR_CFG if isr.is_none() => isr = Some(mmio),
                VIRTIO_PCI_CAP_DEVICE_CFG if device.is_none() => device = Some(mmio),
                _ => {}
            }
        }

        Self {
            common: common.expect("VirtIO: missing COMMON_CFG capability"),
            notify: notify.expect("VirtIO: missing NOTIFY_CFG capability"),
            notify_off_multiplier,
            isr: isr.expect("VirtIO: missing ISR_CFG capability"),
            device: device.expect("VirtIO: missing DEVICE_CFG capability"),
        }
    }
}

/// A VirtIO split virtqueue backed by a single DMA page.
pub struct Virtqueue {
    base: u64,
    next_desc: u16,
    last_used_idx: u16,
    notify_offset: u16,
}

/// Direction of a buffer in a descriptor chain.
pub enum BufDir {
    /// Driver → device (device reads this buffer).
    Readable,
    /// Device → driver (device writes this buffer).
    Writable,
}

impl Virtqueue {
    /// Create a new virtqueue backed by the given DMA page.
    /// The page is zeroed and laid out with desc/avail/used regions.
    pub fn new(dma_page_addr: u64) -> Self {
        unsafe { write_bytes(dma_page_addr as *mut u8, 0, 4096); }
        Self {
            base: dma_page_addr,
            next_desc: 0,
            last_used_idx: 0,
            notify_offset: 0,
        }
    }

    fn descs(&self) -> *mut VirtqDesc { (self.base + DESC_OFFSET) as *mut VirtqDesc }
    fn avail(&self) -> *mut VirtqAvail { (self.base + AVAIL_OFFSET) as *mut VirtqAvail }
    fn used(&self) -> *const VirtqUsed { (self.base + USED_OFFSET) as *const VirtqUsed }

    /// Submit a descriptor chain and wait for the device to complete it.
    pub fn submit_and_wait(
        &mut self,
        bufs: &[(u64, u32, BufDir)],
        notify_mmio: Mmio,
        notify_multiplier: u32,
        queue_index: u16,
    ) {
        let descs = self.descs();

        // Build descriptor chain
        let first_desc = self.next_desc;
        for (i, (addr, len, dir)) in bufs.iter().enumerate() {
            let desc_idx = (first_desc + i as u16) % QUEUE_SIZE;
            let is_last = i == bufs.len() - 1;
            let next_idx = (desc_idx + 1) % QUEUE_SIZE;

            let mut flags: u16 = match dir {
                BufDir::Readable => 0,
                BufDir::Writable => VIRTQ_DESC_F_WRITE,
            };
            if !is_last {
                flags |= VIRTQ_DESC_F_NEXT;
            }

            let desc = VirtqDesc { addr: *addr, len: *len, flags, next: next_idx };
            unsafe { write_volatile(descs.add(desc_idx as usize), desc); }
        }
        self.next_desc = (first_desc + bufs.len() as u16) % QUEUE_SIZE;

        // Add to available ring
        let avail = self.avail();
        let avail_idx = unsafe { read_volatile(&raw const (*avail).idx) };
        unsafe {
            write_volatile(&raw mut (*avail).ring[(avail_idx % QUEUE_SIZE) as usize], first_desc);
            fence(Ordering::Release);
            write_volatile(&raw mut (*avail).idx, avail_idx.wrapping_add(1));
        }

        // Notify device
        fence(Ordering::Release);
        let notify_off = self.notify_offset as u64 * notify_multiplier as u64;
        notify_mmio.write_u16(notify_off, queue_index);

        // Poll used ring for completion
        let used = self.used();
        loop {
            let used_idx = unsafe { read_volatile(&raw const (*used).idx) };
            if used_idx != self.last_used_idx {
                self.last_used_idx = used_idx;
                break;
            }
            core::hint::spin_loop();
        }
    }
}

/// A fully initialized VirtIO device.
pub struct VirtioDevice {
    config: VirtioPciConfig,
}

impl VirtioDevice {
    /// Initialize a VirtIO PCI device: reset, negotiate features, prepare for queue setup.
    pub fn init(pci_dev: &PciDevice, accepted_features: u64) -> Self {
        pci_dev.enable_bus_master();

        let config = VirtioPciConfig::parse(pci_dev);

        // Map the BAR regions used by capabilities
        // The BAR addresses are already physical; we need them identity-mapped.
        // Map a generous region covering all capability offsets.
        for cap in pci_dev.capabilities() {
            if cap.id() != PCI_CAP_ID_VENDOR { continue; }
            let bar_idx = cap.read_u8(4);
            let bar_addr = pci_dev.read_bar_64(bar_idx);
            if bar_addr != 0 {
                paging::map_kernel(bar_addr, 0x4000);
            }
        }

        let common = config.common;

        // 1. Reset
        common.write_u32(COMMON_DEVICE_STATUS, 0);
        while common.read_u32(COMMON_DEVICE_STATUS) != 0 {
            core::hint::spin_loop();
        }

        // 2. ACKNOWLEDGE
        common.write_u32(COMMON_DEVICE_STATUS, STATUS_ACKNOWLEDGE as u32);

        // 3. DRIVER
        common.write_u32(COMMON_DEVICE_STATUS,
            STATUS_ACKNOWLEDGE as u32 | STATUS_DRIVER as u32);

        // 4. Negotiate features
        // Read device features (low 32 bits)
        common.write_u32(COMMON_DEVICE_FEATURE_SELECT, 0);
        let device_features_lo = common.read_u32(COMMON_DEVICE_FEATURE);
        // Read device features (high 32 bits)
        common.write_u32(COMMON_DEVICE_FEATURE_SELECT, 1);
        let device_features_hi = common.read_u32(COMMON_DEVICE_FEATURE);
        let device_features = (device_features_hi as u64) << 32 | device_features_lo as u64;

        let features = device_features & accepted_features;
        log!("VirtIO: device features={:#x} negotiated={:#x}", device_features, features);

        // Write accepted features
        common.write_u32(COMMON_DRIVER_FEATURE_SELECT, 0);
        common.write_u32(COMMON_DRIVER_FEATURE, features as u32);
        common.write_u32(COMMON_DRIVER_FEATURE_SELECT, 1);
        common.write_u32(COMMON_DRIVER_FEATURE, (features >> 32) as u32);

        // 5. FEATURES_OK
        let status = STATUS_ACKNOWLEDGE as u32 | STATUS_DRIVER as u32 | STATUS_FEATURES_OK as u32;
        common.write_u32(COMMON_DEVICE_STATUS, status);

        // 6. Verify FEATURES_OK stuck
        assert!(
            common.read_u32(COMMON_DEVICE_STATUS) & STATUS_FEATURES_OK as u32 != 0,
            "VirtIO: device rejected features"
        );

        Self { config }
    }

    /// Configure a virtqueue. Must be called before `activate()`.
    pub fn setup_queue(&self, index: u16, queue: &mut Virtqueue) {
        let common = self.config.common;

        common.write_u16(COMMON_QUEUE_SELECT, index);

        let max_size = common.read_u16(COMMON_QUEUE_SIZE);
        assert!(max_size >= QUEUE_SIZE, "VirtIO: queue {} too small (max={})", index, max_size);
        common.write_u16(COMMON_QUEUE_SIZE, QUEUE_SIZE);

        common.write_u64(COMMON_QUEUE_DESC, queue.descs() as u64);
        common.write_u64(COMMON_QUEUE_DRIVER, queue.avail() as u64);
        common.write_u64(COMMON_QUEUE_DEVICE, queue.used() as u64);

        queue.notify_offset = common.read_u16(COMMON_QUEUE_NOTIFY_OFF);

        common.write_u16(COMMON_QUEUE_ENABLE, 1);
    }

    /// Set DRIVER_OK — device is now live.
    pub fn activate(&self) {
        let status = STATUS_ACKNOWLEDGE as u32
            | STATUS_DRIVER as u32
            | STATUS_FEATURES_OK as u32
            | STATUS_DRIVER_OK as u32;
        self.config.common.write_u32(COMMON_DEVICE_STATUS, status);
    }

    pub fn notify_mmio(&self) -> Mmio {
        self.config.notify
    }

    pub fn notify_off_multiplier(&self) -> u32 {
        self.config.notify_off_multiplier
    }

    pub fn device_config(&self) -> Mmio {
        self.config.device
    }
}
