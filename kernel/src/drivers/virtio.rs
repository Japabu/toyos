use core::ptr::{read_volatile, write_volatile, write_bytes};
use core::sync::atomic::{fence, Ordering};

use super::mmio::Mmio;
use super::pci::PciDevice;
use crate::arch::paging;
use crate::log;
use crate::PhysAddr;

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
pub const COMMON_DEVICE_FEATURE_SELECT: u64 = 0x00;
pub const COMMON_DEVICE_FEATURE: u64 = 0x04;
pub const COMMON_DRIVER_FEATURE_SELECT: u64 = 0x08;
pub const COMMON_DRIVER_FEATURE: u64 = 0x0C;
pub const COMMON_MSIX_CONFIG: u64 = 0x10;
pub const COMMON_DEVICE_STATUS: u64 = 0x14;
pub const COMMON_QUEUE_SELECT: u64 = 0x16;
pub const COMMON_QUEUE_SIZE: u64 = 0x18;
pub const COMMON_QUEUE_MSIX: u64 = 0x1A;
pub const COMMON_QUEUE_ENABLE: u64 = 0x1C;
pub const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1E;
pub const COMMON_QUEUE_DESC: u64 = 0x20;
pub const COMMON_QUEUE_DRIVER: u64 = 0x28;
pub const COMMON_QUEUE_DEVICE: u64 = 0x30;

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
            let mmio = Mmio::new(crate::PhysAddr::new(bar_addr + offset));

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
    base_phys: crate::DmaAddr,
    base_virt: *mut u8,
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
    /// `phys` is the physical address (for device registers), `virt` is the kernel pointer.
    pub fn new(phys: crate::DmaAddr, virt: *mut u8) -> Self {
        unsafe { write_bytes(virt, 0, 4096); }
        Self {
            base_phys: phys,
            base_virt: virt,
            next_desc: 0,
            last_used_idx: 0,
            notify_offset: 0,
        }
    }

    /// Virtual pointers for kernel read/write access.
    fn descs(&self) -> *mut VirtqDesc { unsafe { self.base_virt.add(DESC_OFFSET as usize) as *mut VirtqDesc } }
    fn avail(&self) -> *mut VirtqAvail { unsafe { self.base_virt.add(AVAIL_OFFSET as usize) as *mut VirtqAvail } }
    fn used(&self) -> *const VirtqUsed { unsafe { self.base_virt.add(USED_OFFSET as usize) as *const VirtqUsed } }

    /// Physical addresses for device register programming.
    fn descs_phys(&self) -> u64 { self.base_phys.raw() + DESC_OFFSET }
    fn avail_phys(&self) -> u64 { self.base_phys.raw() + AVAIL_OFFSET }
    fn used_phys(&self) -> u64 { self.base_phys.raw() + USED_OFFSET }

    /// The descriptor index that will be used by the next submit call.
    pub fn next_desc_id(&self) -> u16 {
        self.next_desc
    }

    /// Submit a descriptor chain and notify the device (non-blocking).
    /// Returns the first descriptor index of the chain.
    pub fn submit(
        &mut self,
        bufs: &[(u64, u32, BufDir)],
        notify_mmio: Mmio,
        notify_multiplier: u32,
        queue_index: u16,
    ) -> u16 {
        let descs = self.descs();

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

        let avail = self.avail();
        let avail_idx = unsafe { read_volatile(&raw const (*avail).idx) };
        unsafe {
            write_volatile(&raw mut (*avail).ring[(avail_idx % QUEUE_SIZE) as usize], first_desc);
            fence(Ordering::Release);
            write_volatile(&raw mut (*avail).idx, avail_idx.wrapping_add(1));
        }

        fence(Ordering::Release);
        let notify_off = self.notify_offset as u64 * notify_multiplier as u64;
        notify_mmio.write_u16(notify_off, queue_index);

        first_desc
    }

    /// Check if the device has completed any request.
    pub fn has_used(&self) -> bool {
        let used = self.used();
        let used_idx = unsafe { read_volatile(&raw const (*used).idx) };
        used_idx != self.last_used_idx
    }

    /// Non-blocking poll of the used ring. Returns `(descriptor_id, written_len)` if
    /// the device has completed a request, or `None` if nothing new.
    pub fn poll_used(&mut self) -> Option<(u16, u32)> {
        let used = self.used();
        let used_idx = unsafe { read_volatile(&raw const (*used).idx) };
        if used_idx == self.last_used_idx {
            return None;
        }
        let entry = unsafe {
            read_volatile(&raw const (*used).ring[(self.last_used_idx % QUEUE_SIZE) as usize])
        };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((entry.id as u16, entry.len))
    }

    /// Submit a descriptor chain and wait for the device to complete it.
    pub fn submit_and_wait(
        &mut self,
        bufs: &[(u64, u32, BufDir)],
        notify_mmio: Mmio,
        notify_multiplier: u32,
        queue_index: u16,
    ) {
        self.submit(bufs, notify_mmio, notify_multiplier, queue_index);
        loop {
            if self.poll_used().is_some() {
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
        // The BAR addresses are physical; we access them via the kernel direct map.
        // Map a generous region covering all capability offsets.
        for cap in pci_dev.capabilities() {
            if cap.id() != PCI_CAP_ID_VENDOR { continue; }
            let bar_idx = cap.read_u8(4);
            let bar_addr = pci_dev.read_bar_64(bar_idx);
            if bar_addr != 0 {
                paging::map_kernel(PhysAddr::new(bar_addr), 0x4000);
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

    /// Configure a virtqueue's addresses and size. Does NOT enable the queue —
    /// call `enable_queue()` after setting MSI-X vectors (if applicable).
    pub fn setup_queue(&self, index: u16, queue: &mut Virtqueue) {
        let common = self.config.common;

        common.write_u16(COMMON_QUEUE_SELECT, index);

        let max_size = common.read_u16(COMMON_QUEUE_SIZE);
        assert!(max_size >= QUEUE_SIZE, "VirtIO: queue {} too small (max={})", index, max_size);
        common.write_u16(COMMON_QUEUE_SIZE, QUEUE_SIZE);

        common.write_u64(COMMON_QUEUE_DESC, queue.descs_phys());
        common.write_u64(COMMON_QUEUE_DRIVER, queue.avail_phys());
        common.write_u64(COMMON_QUEUE_DEVICE, queue.used_phys());

        queue.notify_offset = common.read_u16(COMMON_QUEUE_NOTIFY_OFF);
    }

    /// Enable a previously configured virtqueue.
    pub fn enable_queue(&self, index: u16) {
        let common = self.config.common;
        common.write_u16(COMMON_QUEUE_SELECT, index);
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

    pub fn common_config(&self) -> Mmio {
        self.config.common
    }

    pub fn device_config(&self) -> Mmio {
        self.config.device
    }
}
