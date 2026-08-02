//! Which partitions this machine was given, and where they are.
//!
//! Two halves that must not be confused. **Identity** comes from the
//! bootloader: the unique partition GUID of the volume firmware loaded it
//! from, read out of its own `LoadedImage` device path while Boot Services are
//! alive, and the unique GUID of the log partition, read out of a file on that
//! same volume. Both arrive in [`KernelArgs`]. **Location** comes from the
//! disk: [`probe`] parses a block device's GPT and looks for those exact GUIDs.
//!
//! The kernel is *given* both. It never goes looking for either. The difference
//! is not academic — "mount whatever looks like an ESP" is the same shape as
//! "format whatever fails to mount", and the second one nearly took the owner's
//! 244 GB NVMe (`5dff9aa`, and `bcachefs_adapter::probe`'s comment). It is the
//! reason the log partition is not found by its type or by being the only other
//! FAT32 on the stick, both of which would have needed no handoff at all.
//!
//! The two identities are not equally sourced and the code says so. The boot
//! partition has two independent accounts, firmware's and the table's, and a
//! disagreement refuses it. The log partition has one, so it is anchored to the
//! boot partition instead: it counts only on the device that carries the boot
//! partition, because the file that named it is on that volume.
//!
//! Nothing here writes. Parsing is [`toyos_gpt`], which treats the table as
//! hostile bytes and has no panicking path; this file is the adapter from the
//! kernel's 4 KiB `BlockDevice` down to the device's own logical block, plus
//! the two things only the kernel can decide: whether firmware and the table
//! agree, and what to do when two devices claim the same partition.

use crate::block::{BlockDevice, DeviceId};
use crate::sync::Lock;
use toyos_abi::boot::KernelArgs;
use toyos_gpt::{GptError, Guid, Sectors};

/// The partition firmware loaded the bootloader from, in firmware's terms.
#[derive(Clone, Copy, Debug)]
pub struct BootPartition {
    pub guid: Guid,
    /// In the boot device's logical blocks, whose size firmware does not tell
    /// us — so these two are for cross-checking a GPT entry, never for I/O.
    pub start_lba: u64,
    pub blocks: u64,
}

/// A partition this kernel was given, as a place on a device it can read.
#[derive(Clone, Copy, Debug)]
pub struct Volume {
    pub device: DeviceId,
    /// The device's logical block size. Both LBAs below are in these units,
    /// not in the 4 KiB blocks `BlockDevice` speaks.
    pub lba_bytes: u32,
    pub start_lba: u64,
    pub blocks: u64,
}

/// What the kernel knows about where the partitions it was given live.
///
/// `Ambiguous` is a state and not an error return because it is a property of
/// the machine, not of a call: two devices carrying one unique partition GUID
/// means one is a clone of the other, and nothing on this side can tell which
/// one firmware read. The only safe answer to "which is mine" is then "I do
/// not know", forever, and never "the first one I saw".
///
/// `log` is an `Option` inside `Found` because that is a state a real stick
/// reaches: an image flashed before the log partition existed, or a table that
/// no longer carries the entry the ESP names. It means the machine keeps its
/// `/boot` and has no `/log`, and it is a refusal that says which GUID it could
/// not find — never a fallback onto some other partition.
enum Resolution {
    Unknown,
    Found { boot: Volume, log: Option<Volume> },
    Ambiguous,
}

static FIRMWARE: Lock<Option<BootPartition>> = Lock::new(None);
/// The log partition's identity. `None` only before [`init`] — the handoff
/// always carries one, because the bootloader refuses a volume that does not
/// name it.
static LOG_GUID: Lock<Option<Guid>> = Lock::new(None);
static RESOLVED: Lock<Resolution> = Lock::new(Resolution::Unknown);

/// Take both partitions' identities out of the bootloader's handoff.
pub fn init(args: &KernelArgs) {
    let log_guid = Guid(args.log_partition_guid);
    log!("gpt: the boot volume names {log_guid} as the log partition");
    *LOG_GUID.lock() = Some(log_guid);

    if args.boot_partition_present == 0 {
        log!("gpt: firmware named no boot partition — this machine has none");
        return;
    }
    let part = BootPartition {
        guid: Guid(args.boot_partition_guid),
        start_lba: args.boot_partition_start_lba,
        blocks: args.boot_partition_blocks,
    };
    log!(
        "gpt: firmware booted us from partition {} at LBA {}+{}",
        part.guid, part.start_lba, part.blocks
    );
    *FIRMWARE.lock() = Some(part);
}

pub fn boot_partition() -> Option<BootPartition> {
    *FIRMWARE.lock()
}

/// Where the boot partition is, if a device has been found to carry it.
///
/// This is what a filesystem mount asks: the answer is an LBA range on a named
/// device, or nothing at all. There is no third answer and no "probably".
pub fn boot_volume() -> Option<Volume> {
    match *RESOLVED.lock() {
        Resolution::Found { boot, .. } => Some(boot),
        Resolution::Unknown | Resolution::Ambiguous => None,
    }
}

/// Where the log partition is, on the device that carries the boot partition.
pub fn log_volume() -> Option<Volume> {
    match *RESOLVED.lock() {
        Resolution::Found { log, .. } => log,
        Resolution::Unknown | Resolution::Ambiguous => None,
    }
}

/// Ask one block device whether it carries the boot partition.
///
/// Read-only, and called once per device as the device is discovered. A device
/// that does not carry it is not a failure of any kind — most machines have
/// several disks and at most one of them is the one we booted from.
pub fn probe(dev: &mut dyn BlockDevice, lba_bytes: u32) {
    let id = dev.device_id();
    let Some(firmware) = boot_partition() else {
        return;
    };

    let mut sectors = DeviceSectors::new(dev, lba_bytes);
    let found = match toyos_gpt::locate(&mut sectors, firmware.guid) {
        Ok(found) => found,
        Err(GptError::NotFound { used_entries }) => {
            log!("gpt: device {id} has {used_entries} partitions and none of them is ours");
            return;
        }
        Err(e) => {
            log!("gpt: device {id} has no partition table we can use: {e:?}");
            return;
        }
    };

    // Two independent accounts of the same extent: firmware's, read from a
    // device path before ExitBootServices, and the table's, read from the
    // platter just now. They describe the same partition or this is not the
    // disk firmware was looking at, and there is no repair for that — only a
    // refusal, because the next thing anyone does with this answer is write.
    let part = found.partition;
    if part.first_lba != firmware.start_lba || part.lba_count() != firmware.blocks {
        log!(
            "gpt: device {id} puts {} at LBA {}+{} but firmware said {}+{} — not treating it as \
             the boot volume",
            part.unique_guid,
            part.first_lba,
            part.lba_count(),
            firmware.start_lba,
            firmware.blocks
        );
        return;
    }

    let volume = Volume {
        device: id,
        lba_bytes,
        start_lba: part.first_lba,
        blocks: part.lba_count(),
    };

    let mut resolved = RESOLVED.lock();
    match *resolved {
        Resolution::Unknown => {
            // Only here, and so only on this device. The GUID came off a file
            // on the volume this table has just placed, so a partition of that
            // name on some *other* disk is not the one that file meant — and a
            // machine whose boot volume is ambiguous has no such file to trust.
            let log = locate_log(&mut sectors, id, lba_bytes);
            log!(
                "gpt: device {id} carries the boot partition at LBA {}+{} ({}-byte blocks), \
                 entry {} of {} on disk {}{}",
                volume.start_lba,
                volume.blocks,
                lba_bytes,
                part.index,
                found.used_entries,
                found.disk_guid,
                if part.is_efi_system() { "" } else { " — and its type is not ESP" }
            );
            *resolved = Resolution::Found { boot: volume, log };
        }
        Resolution::Found { boot: first, .. } => {
            log!(
                "gpt: device {id} carries the same partition GUID as device {} — one of them is \
                 a copy and nothing here can say which one we booted from, so this machine now \
                 has no boot volume",
                first.device
            );
            *resolved = Resolution::Ambiguous;
        }
        Resolution::Ambiguous => {
            log!("gpt: device {id} also carries the boot partition GUID");
        }
    }
}

/// The log partition on the device that has just proved it carries the boot
/// partition, or a refusal that names what it looked for.
///
/// No second account to cross-check against: firmware describes one partition
/// and this is not it. What stands in for that is where the name came from —
/// `\toyos\log.guid` on the volume firmware *did* describe — plus this call
/// being reachable only from the device carrying that volume.
fn locate_log(sectors: &mut DeviceSectors<'_>, id: DeviceId, lba_bytes: u32) -> Option<Volume> {
    let target = LOG_GUID.lock().expect("gpt::init runs before any device is probed");
    match toyos_gpt::locate(sectors, target) {
        Ok(found) => {
            let part = found.partition;
            log!(
                "gpt: device {id} carries the log partition {target} at LBA {}+{}, entry {} of {}",
                part.first_lba,
                part.lba_count(),
                part.index,
                found.used_entries
            );
            Some(Volume {
                device: id,
                lba_bytes,
                start_lba: part.first_lba,
                blocks: part.lba_count(),
            })
        }
        Err(e) => {
            log!(
                "gpt: device {id} carries the boot partition but nothing with the log partition's \
                 GUID {target}: {e:?} — this stick has no log partition and the kernel's log \
                 stays in memory"
            );
            None
        }
    }
}

/// The kernel's 4 KiB `BlockDevice`, seen in the device's own logical blocks.
///
/// A GPT is laid out in the device's blocks, so the parser reads 512-byte LBAs
/// off a driver whose smallest read is 4096. One block of cache turns the 34
/// LBA reads a parse needs into 5 device round trips; without it every one of
/// them is a separate NVMe command for bytes we already had.
struct DeviceSectors<'a> {
    dev: &'a mut dyn BlockDevice,
    lba_bytes: u32,
    lbas_per_block: u64,
    cached: Option<u64>,
    buf: [u8; 4096],
}

impl<'a> DeviceSectors<'a> {
    fn new(dev: &'a mut dyn BlockDevice, lba_bytes: u32) -> Self {
        // Zero for a block size that does not divide 4096, which makes
        // `lba_count` zero and every read fail — the parser then refuses on
        // `UnsupportedLbaSize` before asking for anything.
        let lbas_per_block = if lba_bytes != 0 && 4096 % lba_bytes == 0 {
            (4096 / lba_bytes) as u64
        } else {
            0
        };
        Self { dev, lba_bytes, lbas_per_block, cached: None, buf: [0; 4096] }
    }
}

impl Sectors for DeviceSectors<'_> {
    fn lba_bytes(&self) -> u32 {
        self.lba_bytes
    }

    fn lba_count(&self) -> u64 {
        self.dev.block_count().saturating_mul(self.lbas_per_block)
    }

    fn read_lba(&mut self, lba: u64, out: &mut [u8]) -> bool {
        if self.lbas_per_block == 0 || out.len() != self.lba_bytes as usize {
            return false;
        }
        let block = lba / self.lbas_per_block;
        if block >= self.dev.block_count() {
            return false;
        }
        if self.cached != Some(block) {
            // The tag has to be dropped with the read, not just the read
            // refused: a failed read leaves the buffer holding the *previous*
            // block, and a cache still claiming this one would serve those
            // bytes to the next LBA in the same block with nothing to mark
            // them stale.
            if self.dev.read_blocks(block, 1, &mut self.buf).is_err() {
                self.cached = None;
                return false;
            }
            self.cached = Some(block);
        }
        let at = (lba % self.lbas_per_block) as usize * self.lba_bytes as usize;
        out.copy_from_slice(&self.buf[at..at + out.len()]);
        true
    }
}
