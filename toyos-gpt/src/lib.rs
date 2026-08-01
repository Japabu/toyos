//! GPT parsing, for the one question ToyOS asks a partition table: **where is
//! the partition firmware booted us from?**
//!
//! Not "which partition looks like an ESP". The bootloader takes the unique
//! partition GUID out of its own `LoadedImage` device path while Boot Services
//! are still alive and hands it to the kernel; this crate finds *that* GUID in
//! *that* device's table, or refuses. Searching for a type GUID, or for the
//! first FAT-looking thing, is how an operating system reformats a disk that
//! belongs to somebody else — the same class of defect `bcachefs_adapter::probe`
//! exists to prevent, and the reason `5dff9aa` exists.
//!
//! Everything here treats the disk as hostile. A GPT is bytes an attacker (or
//! a dying flash controller) may have written: every length, count and LBA in
//! it is checked before it is used, nothing is indexed without a bound, and no
//! path panics. The kernel's fail-fast rule is for kernel bugs, never for
//! input that crossed a trust boundary.
//!
//! `no_std`, no allocation, no `unsafe`: the entry array is streamed a block
//! at a time through [`Sectors`], so nothing here is sized by a number the
//! disk chose.

#![no_std]
#![forbid(unsafe_code)]

mod crc32;
mod guid;

pub use crc32::{crc32, Crc32};
pub use guid::Guid;

/// The block sizes this crate will parse a GPT out of.
///
/// The floor is the smallest logical block any device has ever reported and
/// the value every GPT in the wild is laid out in; the ceiling is 4Kn, and is
/// also what the rest of this kernel is written in. It matches the NVMe
/// driver's own accepted range, which is not a coincidence: above 4096 the
/// block no longer divides the kernel's 4 KiB block, and below 512 the GPT
/// header does not fit in one.
pub const MIN_LBA_BYTES: u32 = 512;
pub const MAX_LBA_BYTES: u32 = 4096;

/// The largest partition entry array this crate will walk, in bytes.
///
/// Policy, not physics, and generous: UEFI requires the array to be at least
/// 16,384 bytes and every table in practice is exactly that — 128 entries of
/// 128 bytes — so this is eight times the mandated minimum. It bounds *work*
/// rather than memory, because the array is never held: a header claiming
/// four billion entries would otherwise be four billion entries' worth of
/// device reads before the CRC that proves it was a lie.
pub const MAX_ENTRY_ARRAY_BYTES: u64 = 128 * 1024;

/// The smallest a GPT header may claim to be, from the UEFI specification.
const MIN_HEADER_BYTES: u32 = 92;
/// The smallest a partition entry may be, from the UEFI specification.
const MIN_ENTRY_BYTES: u32 = 128;
const HEADER_SIGNATURE: &[u8; 8] = b"EFI PART";
const HEADER_REVISION_1_0: u32 = 0x0001_0000;
/// The MBR partition type that says "this disk is GPT, keep out".
const MBR_TYPE_PROTECTIVE: u8 = 0xEE;

/// A device that can be read one logical block at a time.
///
/// The unit is the device's own logical block, because that is the unit a GPT
/// is written in. A caller whose driver speaks a coarser block — the kernel's
/// `BlockDevice` is 4 KiB — adapts in its implementation of this trait, not
/// inside the parser: the parser must not have to know 4096-byte reads exist.
pub trait Sectors {
    fn lba_bytes(&self) -> u32;
    fn lba_count(&self) -> u64;
    /// Fill `buf` — exactly `lba_bytes()` long — with logical block `lba`.
    /// `false` means the read did not happen and its contents are unknown.
    fn read_lba(&mut self, lba: u64, buf: &mut [u8]) -> bool;
}

/// Every way a table can fail to name a partition, and nothing that panics.
///
/// One variant per refusal rather than a single "malformed", because the
/// caller logs this on a machine whose only channel out may be a screen: what
/// a first bare-metal boot needs is which field was wrong and what it said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GptError {
    /// The device's logical block size is not one this crate parses.
    UnsupportedLbaSize(u32),
    /// A read the parse needed did not happen.
    ReadFailed(u64),
    /// The device is too small to hold a GPT at all.
    DeviceTooSmall(u64),
    /// LBA 0 is not a GPT protective MBR. A disk with a real MBR partition
    /// table, or with a hybrid one, lands here and is refused rather than
    /// interpreted.
    NoProtectiveMbr,
    /// LBA 1 does not begin `EFI PART`.
    NoHeader,
    UnsupportedRevision(u32),
    /// `header_size` outside `92..=lba_bytes`.
    HeaderSize(u32),
    /// The header's reserved word is not zero.
    HeaderReserved(u32),
    /// The header does not claim to live at LBA 1, so it is not the primary
    /// header and this is not the disk it was written for.
    HeaderMisplaced(u64),
    HeaderCrc { stored: u32, computed: u32 },
    /// `first_usable_lba`/`last_usable_lba` are not a range inside the device.
    UsableRange { first: u64, last: u64 },
    /// `size_of_partition_entry` is not at least 128, a power of two, and a
    /// divisor of the logical block — the three together are what keep an
    /// entry from straddling two reads.
    EntrySize(u32),
    /// The array is larger than [`MAX_ENTRY_ARRAY_BYTES`].
    EntryArrayTooBig { entries: u32, entry_size: u32 },
    /// The array does not fit between the header and the first usable block,
    /// or runs off the end of the device.
    EntryArrayMisplaced { lba: u64, lbas: u64 },
    EntryArrayCrc { stored: u32, computed: u32 },
    /// The table is well-formed and does not contain the GUID asked for.
    /// Carries the partition count because "this disk has three partitions and
    /// none of them is ours" and "this disk has none" are different facts, and
    /// on a machine with no serial port the log line is the whole diagnostic.
    NotFound { used_entries: u32 },
    /// The matching entry's blocks are not inside the disk's usable range.
    PartitionRange { first: u64, last: u64 },
    /// Another entry claims blocks the matching entry also claims. Refused
    /// rather than resolved: the caller's next move is to write to those
    /// blocks, and there is no reading of this table under which that is safe.
    PartitionOverlap { index: u32 },
}

/// One partition entry, after it has been checked against the disk it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// Position in the entry array, from 0.
    pub index: u32,
    pub type_guid: Guid,
    pub unique_guid: Guid,
    pub first_lba: u64,
    /// Inclusive, as GPT stores it.
    pub last_lba: u64,
}

impl Partition {
    pub const fn lba_count(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }

    /// Whether this is an ESP *by type*. A sanity check for a log line and
    /// nothing more — the selection has already happened, by unique GUID.
    pub fn is_efi_system(&self) -> bool {
        self.type_guid == Guid::EFI_SYSTEM
    }
}

/// A located partition plus what the table around it looked like.
///
/// The extra fields are not decoration: a log line saying "this disk has three
/// partitions and none of them is ours" is a different diagnostic from "this
/// disk has no partition table", and on a machine with no serial port the
/// difference is the whole debugging session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Located {
    pub partition: Partition,
    pub disk_guid: Guid,
    /// Entries with a non-zero type GUID, i.e. partitions that exist.
    pub used_entries: u32,
}

/// The primary GPT header, once every field in it has been checked.
#[derive(Debug, Clone, Copy)]
struct Header {
    disk_guid: Guid,
    first_usable_lba: u64,
    last_usable_lba: u64,
    entry_array_lba: u64,
    entry_count: u32,
    entry_bytes: u32,
    entry_array_crc: u32,
}

/// Find the partition carrying `target` on `dev`.
///
/// Reads only. The order is not negotiable: the protective MBR, then the
/// header and its CRC, then the entry array and *its* CRC — and only then is
/// anything the array said allowed to mean something. A match found on the way
/// through is held back until the array's CRC proves the bytes it came from
/// were not garbage.
pub fn locate(dev: &mut dyn Sectors, target: Guid) -> Result<Located, GptError> {
    let lba_bytes = dev.lba_bytes();
    if !(MIN_LBA_BYTES..=MAX_LBA_BYTES).contains(&lba_bytes) || !lba_bytes.is_power_of_two() {
        return Err(GptError::UnsupportedLbaSize(lba_bytes));
    }
    let lba_count = dev.lba_count();
    // LBA 0 protective MBR, LBA 1 header, at least one block of entries.
    if lba_count < 3 {
        return Err(GptError::DeviceTooSmall(lba_count));
    }

    let mut block = [0u8; MAX_LBA_BYTES as usize];
    let block = &mut block[..lba_bytes as usize];

    read(dev, 0, block)?;
    check_protective_mbr(block)?;

    read(dev, 1, block)?;
    let header = parse_header(block, lba_bytes, lba_count)?;

    let (found, used_entries) = scan_entries(dev, &header, target, lba_bytes)?;
    let Some(partition) = found else {
        return Err(GptError::NotFound { used_entries });
    };

    if partition.first_lba > partition.last_lba
        || partition.first_lba < header.first_usable_lba
        || partition.last_lba > header.last_usable_lba
    {
        return Err(GptError::PartitionRange {
            first: partition.first_lba,
            last: partition.last_lba,
        });
    }
    check_no_overlap(dev, &header, &partition, lba_bytes)?;

    Ok(Located { partition, disk_guid: header.disk_guid, used_entries })
}

fn read(dev: &mut dyn Sectors, lba: u64, buf: &mut [u8]) -> Result<(), GptError> {
    if lba >= dev.lba_count() || !dev.read_lba(lba, buf) {
        return Err(GptError::ReadFailed(lba));
    }
    Ok(())
}

/// LBA 0 must be a protective MBR and nothing else.
///
/// One 0xEE record covering the disk, the other three empty, and the boot
/// signature. A hybrid MBR — a protective record next to real ones, which is
/// what a Mac installer leaves behind — is refused here rather than ignored:
/// it means two tables describe this disk and they can disagree, and picking
/// one of them is guessing.
fn check_protective_mbr(lba0: &[u8]) -> Result<(), GptError> {
    // The MBR is 512 bytes at the front of LBA 0 whatever the block size is.
    let Some(mbr) = lba0.get(..512) else {
        return Err(GptError::NoProtectiveMbr);
    };
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Err(GptError::NoProtectiveMbr);
    }
    let mut protective = 0;
    for record in 0..4 {
        let ty = mbr[446 + record * 16 + 4];
        match ty {
            0 => {}
            MBR_TYPE_PROTECTIVE => protective += 1,
            _ => return Err(GptError::NoProtectiveMbr),
        }
    }
    if protective != 1 {
        return Err(GptError::NoProtectiveMbr);
    }
    Ok(())
}

fn parse_header(lba1: &[u8], lba_bytes: u32, lba_count: u64) -> Result<Header, GptError> {
    if lba1.get(..8) != Some(&HEADER_SIGNATURE[..]) {
        return Err(GptError::NoHeader);
    }
    let revision = le_u32(lba1, 8);
    if revision != HEADER_REVISION_1_0 {
        return Err(GptError::UnsupportedRevision(revision));
    }
    let header_bytes = le_u32(lba1, 12);
    if header_bytes < MIN_HEADER_BYTES || header_bytes > lba_bytes {
        return Err(GptError::HeaderSize(header_bytes));
    }
    let reserved = le_u32(lba1, 20);
    if reserved != 0 {
        return Err(GptError::HeaderReserved(reserved));
    }
    let my_lba = le_u64(lba1, 24);
    if my_lba != 1 {
        return Err(GptError::HeaderMisplaced(my_lba));
    }

    let stored_crc = le_u32(lba1, 16);
    let computed = header_crc(lba1, header_bytes as usize);
    if stored_crc != computed {
        return Err(GptError::HeaderCrc { stored: stored_crc, computed });
    }

    let first_usable_lba = le_u64(lba1, 40);
    let last_usable_lba = le_u64(lba1, 48);
    if first_usable_lba < 2 || last_usable_lba < first_usable_lba || last_usable_lba >= lba_count {
        return Err(GptError::UsableRange { first: first_usable_lba, last: last_usable_lba });
    }

    let entry_bytes = le_u32(lba1, 84);
    if entry_bytes < MIN_ENTRY_BYTES
        || !entry_bytes.is_power_of_two()
        || entry_bytes > lba_bytes
        || lba_bytes % entry_bytes != 0
    {
        return Err(GptError::EntrySize(entry_bytes));
    }
    let entry_count = le_u32(lba1, 80);
    let array_bytes = entry_count as u64 * entry_bytes as u64;
    if array_bytes == 0 || array_bytes > MAX_ENTRY_ARRAY_BYTES {
        return Err(GptError::EntryArrayTooBig { entries: entry_count, entry_size: entry_bytes });
    }

    let entry_array_lba = le_u64(lba1, 72);
    let array_lbas = array_bytes.div_ceil(lba_bytes as u64);
    let array_end = entry_array_lba
        .checked_add(array_lbas)
        .ok_or(GptError::EntryArrayMisplaced { lba: entry_array_lba, lbas: array_lbas })?;
    // Below the header is the header's own block and the protective MBR;
    // above it is data somebody may be using. Neither is somewhere an array
    // can be, and a table that says otherwise describes a different disk.
    if entry_array_lba < 2 || array_end > first_usable_lba {
        return Err(GptError::EntryArrayMisplaced { lba: entry_array_lba, lbas: array_lbas });
    }

    Ok(Header {
        disk_guid: read_guid(lba1, 56),
        first_usable_lba,
        last_usable_lba,
        entry_array_lba,
        entry_count,
        entry_bytes,
        entry_array_crc: le_u32(lba1, 88),
    })
}

/// The header's CRC is taken over itself with its own CRC field zeroed, so it
/// is computed in three pieces rather than by copying the block to patch it.
fn header_crc(lba1: &[u8], header_bytes: usize) -> u32 {
    let mut crc = Crc32::new();
    crc.update(&lba1[..16]);
    crc.update(&[0; 4]);
    crc.update(&lba1[20..header_bytes]);
    crc.finish()
}

/// Walk the entry array once, checking its CRC as we go, and remember the
/// entry carrying `target`.
///
/// The match is returned only if the CRC holds. Acting on an entry read out of
/// an array whose checksum then failed would make the checksum decorative —
/// which is the shape of every "we validated it" that turns out not to have
/// been load-bearing.
fn scan_entries(
    dev: &mut dyn Sectors,
    header: &Header,
    target: Guid,
    lba_bytes: u32,
) -> Result<(Option<Partition>, u32), GptError> {
    let mut block = [0u8; MAX_LBA_BYTES as usize];
    let block = &mut block[..lba_bytes as usize];

    let entries_per_lba = lba_bytes / header.entry_bytes;
    let mut crc = Crc32::new();
    let mut remaining = header.entry_count as u64 * header.entry_bytes as u64;
    let mut found: Option<Partition> = None;
    let mut used = 0u32;
    let mut index = 0u32;
    let mut lba = header.entry_array_lba;

    while remaining > 0 {
        read(dev, lba, block)?;
        let take = remaining.min(lba_bytes as u64) as usize;
        crc.update(&block[..take]);

        for slot in 0..entries_per_lba {
            if index >= header.entry_count {
                break;
            }
            let at = slot as usize * header.entry_bytes as usize;
            let entry = &block[at..at + header.entry_bytes as usize];
            let type_guid = read_guid(entry, 0);
            if !type_guid.is_zero() {
                used += 1;
                let unique_guid = read_guid(entry, 16);
                if unique_guid == target && found.is_none() {
                    found = Some(Partition {
                        index,
                        type_guid,
                        unique_guid,
                        first_lba: le_u64(entry, 32),
                        last_lba: le_u64(entry, 40),
                    });
                }
            }
            index += 1;
        }

        remaining -= take as u64;
        lba += 1;
    }

    let computed = crc.finish();
    if computed != header.entry_array_crc {
        return Err(GptError::EntryArrayCrc { stored: header.entry_array_crc, computed });
    }
    Ok((found, used))
}

/// Nothing else in the table may claim a block the matched partition claims.
///
/// A second pass rather than bookkeeping in the first, because the entry that
/// overlaps may have been read before the one that matched, and the array is
/// streamed. Thirty-two block reads on a 512-byte device, once per boot.
fn check_no_overlap(
    dev: &mut dyn Sectors,
    header: &Header,
    matched: &Partition,
    lba_bytes: u32,
) -> Result<(), GptError> {
    let mut block = [0u8; MAX_LBA_BYTES as usize];
    let block = &mut block[..lba_bytes as usize];

    let entries_per_lba = lba_bytes / header.entry_bytes;
    let mut remaining = header.entry_count as u64 * header.entry_bytes as u64;
    let mut index = 0u32;
    let mut lba = header.entry_array_lba;

    while remaining > 0 {
        read(dev, lba, block)?;
        for slot in 0..entries_per_lba {
            if index >= header.entry_count {
                break;
            }
            let at = slot as usize * header.entry_bytes as usize;
            let entry = &block[at..at + header.entry_bytes as usize];
            if index != matched.index && !read_guid(entry, 0).is_zero() {
                let first = le_u64(entry, 32);
                let last = le_u64(entry, 40);
                if first <= last && first <= matched.last_lba && matched.first_lba <= last {
                    return Err(GptError::PartitionOverlap { index });
                }
            }
            index += 1;
        }
        remaining -= remaining.min(lba_bytes as u64);
        lba += 1;
    }
    Ok(())
}

fn read_guid(buf: &[u8], at: usize) -> Guid {
    let mut out = [0u8; 16];
    out.copy_from_slice(&buf[at..at + 16]);
    Guid(out)
}

fn le_u32(buf: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[at..at + 4]);
    u32::from_le_bytes(b)
}

fn le_u64(buf: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(b)
}
