use core::ptr::{read_volatile, write_volatile, write_bytes};
use core::sync::atomic::{fence, Ordering};

use crate::mm::Mmio;
use super::pci::PciDevice;
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

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// Avail ring layout: flags(u16) + idx(u16) + ring[size](u16 each)
const AVAIL_IDX_OFF: usize = 2;
const AVAIL_RING_OFF: usize = 4;

// Used ring layout: flags(u16) + idx(u16) + ring[size](id:u32 + len:u32 each)
const USED_IDX_OFF: usize = 2;
const USED_RING_OFF: usize = 4;
const USED_ELEM_SIZE: usize = 8; // id(u32) + len(u32)

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

        // Map all BAR regions used by VirtIO capabilities
        let mut mapped_bars: [Option<crate::mm::Mmio>; 6] = [None, None, None, None, None, None];
        for cap in pci_dev.capabilities() {
            if cap.id() != PCI_CAP_ID_VENDOR { continue; }
            let bar_idx = cap.read_u8(4) as usize;
            if bar_idx < 6 && mapped_bars[bar_idx].is_none() {
                let bar_addr = pci_dev.read_bar_64(bar_idx as u8);
                if bar_addr != 0 {
                    mapped_bars[bar_idx] = Some(crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(bar_addr, 0x4000));
                }
            }
        }

        for cap in pci_dev.capabilities() {
            if cap.id() != PCI_CAP_ID_VENDOR {
                continue;
            }
            let cfg_type = cap.read_u8(3);
            let bar_idx = cap.read_u8(4) as usize;
            let offset = cap.read_u32(8) as u64;
            let length = cap.read_u32(12) as u64;

            let bar = mapped_bars[bar_idx].as_ref().expect("VirtIO: BAR not mapped");
            let mmio = bar.subregion(offset, length.max(4));

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

/// DMA region specification for a virtqueue.
pub struct VirtqueueRegions {
    pub desc_phys: u64,
    pub desc_virt: *mut u8,
    pub desc_size: usize,
    pub avail_phys: u64,
    pub avail_virt: *mut u8,
    pub avail_size: usize,
    pub used_phys: u64,
    pub used_virt: *mut u8,
    pub used_size: usize,
}

impl VirtqueueRegions {
    /// Compute regions from a single contiguous DMA buffer.
    /// Desc, avail, used are packed sequentially with proper alignment.
    pub fn from_contiguous(phys: u64, virt: *mut u8, queue_size: u16) -> Self {
        let desc_size = queue_size as usize * core::mem::size_of::<VirtqDesc>();
        let avail_size = 4 + queue_size as usize * 2; // flags + idx + ring
        let used_size = 4 + queue_size as usize * USED_ELEM_SIZE; // flags + idx + ring

        // Avail starts after desc, aligned to 2
        let avail_off = (desc_size + 1) & !1;
        // Used starts after avail, aligned to 4
        let used_off = (avail_off + avail_size + 3) & !3;

        Self {
            desc_phys: phys,
            desc_virt: virt,
            desc_size,
            avail_phys: phys + avail_off as u64,
            avail_virt: unsafe { virt.add(avail_off) },
            avail_size,
            used_phys: phys + used_off as u64,
            used_virt: unsafe { virt.add(used_off) },
            used_size,
        }
    }

    /// Compute regions from three separate DMA pages.
    pub fn from_separate_pages(
        desc_phys: u64, desc_virt: *mut u8,
        avail_phys: u64, avail_virt: *mut u8,
        used_phys: u64, used_virt: *mut u8,
        queue_size: u16,
    ) -> Self {
        Self {
            desc_phys,
            desc_virt,
            desc_size: queue_size as usize * core::mem::size_of::<VirtqDesc>(),
            avail_phys,
            avail_virt,
            avail_size: 4 + queue_size as usize * 2,
            used_phys,
            used_virt,
            used_size: 4 + queue_size as usize * USED_ELEM_SIZE,
        }
    }

}

/// Proof that a descriptor slot is available for submission.
/// Non-Copy, non-Clone: must be obtained from `poll_used()` or `initial_slots()`.
/// Consumed by `submit()` — prevents overwriting in-flight descriptors.
pub struct DescSlot(u16);

impl DescSlot {
    /// The raw descriptor index.
    pub fn id(&self) -> u16 { self.0 }
}

/// A VirtIO split virtqueue.
pub struct Virtqueue {
    desc_virt: *mut VirtqDesc,
    avail_virt: *mut u8,
    used_virt: *const u8,
    desc_phys: u64,
    avail_phys: u64,
    used_phys: u64,
    size: u16,
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
    /// Create a new virtqueue from a contiguous DMA region that fits in one page.
    pub fn new(phys: crate::DmaAddr, virt: *mut u8) -> Self {
        let regions = VirtqueueRegions::from_contiguous(phys.raw(), virt, 16);
        unsafe { write_bytes(virt, 0, 4096); }
        Self::from_regions(&regions, 16)
    }

    /// Create a new virtqueue from explicit DMA regions.
    pub fn from_regions(regions: &VirtqueueRegions, queue_size: u16) -> Self {
        unsafe {
            write_bytes(regions.desc_virt, 0, regions.desc_size);
            write_bytes(regions.avail_virt, 0, regions.avail_size);
            write_bytes(regions.used_virt as *mut u8, 0, regions.used_size);
        }
        Self {
            desc_virt: regions.desc_virt as *mut VirtqDesc,
            avail_virt: regions.avail_virt,
            used_virt: regions.used_virt as *const u8,
            desc_phys: regions.desc_phys,
            avail_phys: regions.avail_phys,
            used_phys: regions.used_phys,
            size: queue_size,
            last_used_idx: 0,
            notify_offset: 0,
        }
    }

    /// Physical addresses for device register programming.
    pub fn descs_phys(&self) -> u64 { self.desc_phys }
    pub fn avail_phys(&self) -> u64 { self.avail_phys }
    pub fn used_phys(&self) -> u64 { self.used_phys }

    // Avail ring accessors via raw pointer math
    fn avail_idx_ptr(&self) -> *mut u16 {
        unsafe { self.avail_virt.add(AVAIL_IDX_OFF) as *mut u16 }
    }
    fn avail_ring_ptr(&self, i: u16) -> *mut u16 {
        unsafe { self.avail_virt.add(AVAIL_RING_OFF + i as usize * 2) as *mut u16 }
    }

    // Used ring accessors via raw pointer math
    fn used_idx_ptr(&self) -> *const u16 {
        unsafe { self.used_virt.add(USED_IDX_OFF) as *const u16 }
    }
    fn used_ring_id_ptr(&self, i: u16) -> *const u32 {
        unsafe { self.used_virt.add(USED_RING_OFF + i as usize * USED_ELEM_SIZE) as *const u32 }
    }
    fn used_ring_len_ptr(&self, i: u16) -> *const u32 {
        unsafe { self.used_virt.add(USED_RING_OFF + i as usize * USED_ELEM_SIZE + 4) as *const u32 }
    }

    /// Return the initial pool of descriptor slots. Call once after construction.
    /// The caller manages these tokens — `submit()` consumes one, `poll_used()` returns one.
    pub fn initial_slots(&self) -> alloc::vec::Vec<DescSlot> {
        (0..self.size).map(DescSlot).collect()
    }

    /// Submit a descriptor chain and notify the device (non-blocking).
    /// Consumes a `DescSlot` proving a descriptor is available.
    /// Returns the descriptor index used (for caller bookkeeping).
    pub fn submit(
        &mut self,
        slot: DescSlot,
        bufs: &[(u64, u32, BufDir)],
        notify_mmio: Mmio,
        notify_multiplier: u32,
        queue_index: u16,
    ) -> u16 {
        let size = self.size;
        let first_desc = slot.0;
        for (i, (addr, len, dir)) in bufs.iter().enumerate() {
            let desc_idx = (first_desc + i as u16) % size;
            let is_last = i == bufs.len() - 1;
            let next_idx = (desc_idx + 1) % size;

            let mut flags: u16 = match dir {
                BufDir::Readable => 0,
                BufDir::Writable => VIRTQ_DESC_F_WRITE,
            };
            if !is_last {
                flags |= VIRTQ_DESC_F_NEXT;
            }

            let desc = VirtqDesc { addr: *addr, len: *len, flags, next: next_idx };
            unsafe { write_volatile(self.desc_virt.add(desc_idx as usize), desc); }
        }

        let avail_idx = unsafe { read_volatile(self.avail_idx_ptr()) };
        unsafe {
            write_volatile(self.avail_ring_ptr(avail_idx % size), first_desc);
            fence(Ordering::Release);
            write_volatile(self.avail_idx_ptr(), avail_idx.wrapping_add(1));
        }

        fence(Ordering::Release);
        let notify_off = self.notify_offset as u64 * notify_multiplier as u64;
        notify_mmio.write_u16(notify_off, queue_index);

        first_desc
    }

    /// Check if the device has completed any request.
    pub fn has_used(&self) -> bool {
        let used_idx = unsafe { read_volatile(self.used_idx_ptr()) };
        used_idx != self.last_used_idx
    }

    /// Non-blocking poll of the used ring. Returns `(DescSlot, written_len)` if
    /// the device has completed a request, or `None` if nothing new.
    /// The returned `DescSlot` can be reused for a new submission.
    pub fn poll_used(&mut self) -> Option<(DescSlot, u32)> {
        let used_idx = unsafe { read_volatile(self.used_idx_ptr()) };
        if used_idx == self.last_used_idx {
            return None;
        }
        fence(Ordering::Acquire);
        let slot = self.last_used_idx % self.size;
        let id = unsafe { read_volatile(self.used_ring_id_ptr(slot)) };
        let len = unsafe { read_volatile(self.used_ring_len_ptr(slot)) };
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((DescSlot(id as u16), len))
    }

    /// Submit a descriptor chain and wait for the device to complete it.
    /// Consumes a `DescSlot` and returns the one recovered from the used ring.
    pub fn submit_and_wait(
        &mut self,
        slot: DescSlot,
        bufs: &[(u64, u32, BufDir)],
        notify_mmio: Mmio,
        notify_multiplier: u32,
        queue_index: u16,
    ) -> DescSlot {
        self.submit(slot, bufs, notify_mmio, notify_multiplier, queue_index);
        loop {
            if let Some((slot, _)) = self.poll_used() {
                return slot;
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
        assert!(max_size >= queue.size, "VirtIO: queue {} too small (max={}, need={})", index, max_size, queue.size);
        common.write_u16(COMMON_QUEUE_SIZE, queue.size);

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
