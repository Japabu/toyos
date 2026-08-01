//! USB mass storage as a [`BlockDevice`].
//!
//! The disks themselves live inside the xHCI controller, because that is where
//! their transfer rings and DMA blocks are and because every command has to
//! serialise against the event ring the HID path also drains. What lives here
//! is the handle: one per bound disk, holding nothing but an index and the
//! geometry the device reported, so the controller lock is taken per operation
//! and never held across one.

use crate::block::{BlockDevice, DeviceId};
use crate::log;
use super::xhci;

/// Where USB disks start in the [`DeviceId`] space. NVMe takes 1; the page
/// cache keys itself on this, so two devices sharing a number would serve each
/// other's blocks.
const USB_DEVICE_ID_BASE: DeviceId = 16;

/// Disks the controller bound this boot, in enumeration order — which is port
/// order, so on a machine that boots off USB the stick it booted from is
/// normally index 0. *Normally* is not a guarantee: which device is the boot
/// device is a question about the firmware's boot entry, not about port order,
/// and answering it is not this driver's job.
pub fn count() -> usize {
    xhci::storage_count()
}

/// A handle to the `index`-th disk, or `None` if there is no such disk.
pub fn open(index: usize) -> Option<UsbBlockDevice> {
    let geometry = xhci::storage_geometry(index)?;
    Some(UsbBlockDevice {
        index,
        id: USB_DEVICE_ID_BASE + index as DeviceId,
        blocks: geometry.blocks,
    })
}

pub struct UsbBlockDevice {
    index: usize,
    id: DeviceId,
    blocks: u64,
}

impl UsbBlockDevice {
    /// Whether the last operation left the disk unusable.
    ///
    /// [`BlockDevice`] has no error channel — `read_blocks` returns `()` — so a
    /// transfer that fails can only log and leave the caller's buffer alone.
    /// That is survivable for a device the kernel owns and wrong for one a user
    /// can pull out mid-write; the trait is what has to change, and it is filed
    /// rather than changed here because NVMe, the page cache and bcachefs all
    /// sit on the current shape.
    pub fn healthy(&self) -> bool {
        xhci::storage_geometry(self.index).is_some_and(|g| g.blocks > 0)
    }
}

impl BlockDevice for UsbBlockDevice {
    fn device_id(&self) -> DeviceId {
        self.id
    }

    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]) {
        if !xhci::storage_read(self.index, lba, count, buf) {
            log!("usb-storage: read of {count} blocks at {lba} failed on disk {}", self.index);
        }
    }

    fn write_blocks(&mut self, lba: u64, count: u32, buf: &[u8]) {
        if !xhci::storage_write(self.index, lba, count, buf) {
            log!("usb-storage: write of {count} blocks at {lba} failed on disk {}", self.index);
        }
    }

    fn flush(&mut self) {
        if !xhci::storage_flush(self.index) {
            log!("usb-storage: cache flush failed on disk {}", self.index);
        }
    }
}
