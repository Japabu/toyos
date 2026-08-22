//! USB mass storage as a [`BlockDevice`].
//!
//! The disks themselves live inside the xHCI controller, because that is where
//! their transfer rings and DMA blocks are and because every command has to
//! serialise against the event ring the HID path also drains. What lives here
//! is the handle: one per bound disk, holding nothing but an index and the
//! geometry the device reported, so the controller lock is taken per operation
//! and never held across one.
//!
//! **This is where an operation's device-time budget is established**
//! ([`crate::block::begin_operation`]), because this is the layer at which one
//! call is one operation: below here the driver batches, retries and recovers,
//! and none of those loops knows what it is part of. What bounds a
//! `read_blocks` of 64 blocks is therefore the same instant that bounds its
//! first command.
//!
//! **Established on the running context and not passed down**, which is owner
//! ruling 1B: the deadline crosses `BlockAccess` and `BlockDevice`, two frames
//! that cannot carry it, so `xhci::wait/msc.rs`'s three operation entry points
//! recover it instead. The guard is a `let _op` and not a `let _`: `let _`
//! drops at the end of the statement, which would end the operation before the
//! call it bounds.

use crate::block::{self, BlockDevice, BlockError, BlockResult, DeviceId};
use crate::log;
use super::xhci;

/// Where USB disks start in the [`DeviceId`] space. NVMe takes 1; the page
/// cache keys itself on this, so two devices sharing a number would serve each
/// other's blocks.
const USB_DEVICE_ID_BASE: DeviceId = 16;

/// Disk numbers issued this boot, so `0..count()` names every disk this machine
/// has bound.
///
/// Numbers go out in bind order, which at boot is port order — so on a machine
/// that boots off USB the stick it booted from is normally 0. *Normally* is not
/// a guarantee: which device is the boot device is a question about the
/// firmware's boot entry, not about port order, and answering it is not this
/// driver's job.
///
/// A number never moves and is never reissued, which is what everything above
/// here depends on: [`open`] hands out a handle keyed on one and a mount holds
/// that handle for its whole life. An unplugged disk leaves its number behind
/// naming nothing, rather than passing it to whatever is plugged in next.
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
        lba_bytes: geometry.logical_block_bytes,
    })
}

pub struct UsbBlockDevice {
    index: usize,
    id: DeviceId,
    blocks: u64,
    /// What the device addresses in, kept because the caller that needs it had
    /// to ask the controller a second time to get it — and a second question
    /// has a second `None`, which is a skip nobody was going to log. One `open`
    /// is one answer.
    lba_bytes: u32,
}

impl UsbBlockDevice {
    /// The device's own logical block size, for a caller that has to speak in
    /// them: a GPT is laid out in these, not in the 4 KiB [`BlockDevice`]
    /// transfers.
    pub fn logical_block_bytes(&self) -> u32 {
        self.lba_bytes
    }

    /// Whether the controller will still speak to the disk under this index.
    ///
    /// Distinct from a failed transfer, which the trait reports: this answers
    /// "is there still something there", which is what a caller asks after a
    /// run of failures. It used to ask the geometry, which a device keeps after
    /// recovery has given up on it — so the one question this exists for was
    /// the one it got wrong, in the direction that keeps a caller retrying.
    /// Whether the controller still has this disk bound. Read by
    /// `usb-storage-gate`'s report and by nothing a shipping kernel compiles.
    #[cfg(feature = "boot-actuators")]
    pub fn healthy(&self) -> bool {
        xhci::storage_online(self.index) == Some(true)
    }
}

impl BlockDevice for UsbBlockDevice {
    fn device_id(&self) -> DeviceId {
        self.id
    }

    fn block_count(&self) -> u64 {
        self.blocks
    }

    fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]) -> BlockResult {
        let _op = block::begin_operation();
        let done = xhci::storage_read(self.index, lba, count, buf);
        if done.is_err() {
            log!("usb-storage: read of {count} blocks at {lba} {} on disk {}",
                gave_up(done), self.index);
        }
        done
    }

    fn write_blocks(&mut self, lba: u64, count: u32, buf: &[u8]) -> BlockResult {
        let _op = block::begin_operation();
        let done = xhci::storage_write(self.index, lba, count, buf);
        if done.is_err() {
            log!("usb-storage: write of {count} blocks at {lba} {} on disk {}",
                gave_up(done), self.index);
        }
        done
    }

    fn flush(&mut self) -> BlockResult {
        let _op = block::begin_operation();
        let done = xhci::storage_flush(self.index);
        if done.is_err() {
            log!("usb-storage: cache flush {} on disk {}", gave_up(done), self.index);
        }
        done
    }
}

/// What this layer's line says happened, which is not the same word for the
/// two refusals.
///
/// **"failed" is a claim about the disk**, and a budget refusal is a claim
/// about the caller's clock: the driver below already wrote the line naming
/// [`block::OPERATION`], and repeating "failed" over it is what made the
/// composite log read as a broken stick. `Ok` is unreachable here — the caller
/// checks — and answers the word that would be least wrong if it were not.
fn gave_up(done: BlockResult) -> &'static str {
    match done {
        Err(BlockError::BudgetExpired) => "ran out of its operation budget",
        _ => "failed",
    }
}
