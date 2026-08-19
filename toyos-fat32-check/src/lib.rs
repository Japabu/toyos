//! A FAT32 volume checker: the outside judge for every volume this project
//! writes.
//!
//! Written from Microsoft's *FAT32 File System Specification* (fatgen103,
//! version 1.03, 6 December 2000) and depending on neither `toyos-fat32` nor
//! `fatfs` — a checker sharing code with the writer it judges agrees with that
//! writer's bugs. Its dependency list is empty, which is the mechanical half of
//! that claim.
//!
//! It is deliberately stronger than the host `fsck_msdos` it stands in for, in
//! two places where that binary is silent and each of which cost this project a
//! defect every other gate passed (`issues/`): a **stale FAT
//! mirror**, which fsck does not compare and a mount never reads, and
//! **duplicate 8.3 short names**, which neither fsck nor a mount looks at
//! because both use the long names.
//!
//! ```no_run
//! # let volume: &[u8] = &[];
//! let complaints = toyos_fat32_check::check(volume);
//! assert!(complaints.is_empty(), "{}", toyos_fat32_check::describe(&complaints));
//! ```
//!
//! A [`Complaint`] names what is wrong, where it is, and what the format
//! requires, and carries its numbers as fields rather than baked into a
//! sentence — so two complaints of the same kind about different counts are
//! distinguishable without parsing text.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

mod boot;
mod dir;
mod fat;

/// How many complaints a check reports before it stops enumerating.
///
/// Policy, not physics: a volume whose FAT is a page of random bytes has a
/// complaint per cluster, and neither a test failure message nor a person has a
/// use for the four hundred thousandth. What the caller sees when it is hit is
/// [`Complaint::More`], so a truncated report never reads as a complete one.
pub const MAX_COMPLAINTS: usize = 256;

/// How deep the directory tree is walked before the checker stops descending.
///
/// Policy. FAT32 has no depth limit of its own, and the walk is one stack frame
/// per level: the cluster claim already stops it visiting a directory twice, so
/// the deepest a crafted volume could nest is its cluster count. This bounds the
/// recursion and nothing else.
pub const MAX_DEPTH: u32 = 64;

/// Everything wrong with a volume, in the terms the format defines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Complaint {
    /// Fewer bytes than a boot sector.
    NoBootSector { bytes: u64 },
    JmpBoot { got: [u8; 3] },
    BytesPerSector { got: u16 },
    SectorsPerCluster { got: u8 },
    BytesPerCluster { got: u64 },
    ReservedSectors,
    NumFats,
    RootEntryCount { got: u16 },
    TotalSectors16 { got: u16 },
    FatSize16 { got: u16 },
    TotalSectors32,
    FatSize32,
    BootSectorSignature { got: u16 },
    Media { got: u8 },
    FileSystemVersion { got: u16 },
    /// `BPB_FSInfo` outside the reserved region, or naming the boot sector.
    FsInfoSector { got: u16, reserved: u64 },
    /// `BPB_ExtFlags` names a FAT copy the volume does not have.
    ActiveFat { got: u32, num_fats: u64 },
    /// The reserved sectors and the FATs leave no data area.
    NoDataArea { metadata_sectors: u64, total_sectors: u64 },
    /// The boot sector describes more volume than the caller was handed.
    VolumeShorterThanDeclared { declared_bytes: u64, actual_bytes: u64 },
    /// Under 65,525 clusters is not a FAT32 volume by the specification's own
    /// definition of the three formats.
    NotFat32 { clusters: u32 },
    FatTooSmall { fat_bytes: u64, needed_bytes: u64 },
    RootCluster { got: u32, clusters: u32 },

    FsInfoLeadSignature { got: u32 },
    FsInfoStructSignature { got: u32 },
    FsInfoTrailSignature { got: u32 },
    FsInfoFreeCount { declared: u32, counted: u32 },
    /// The one complaint the specification does not require, and the one the
    /// replaced `fsck_msdos` printed as `Free space in FSInfo block is unset`.
    /// fatgen103 §5 permits 0xFFFFFFFF, meaning a driver that does not maintain
    /// the count; every volume this project writes maintains it, and the field
    /// going unknown is that stopping rather than a format the checker met.
    FsInfoFreeCountUnknown,
    FsInfoNextFree { got: u32, clusters: u32 },

    Fat0 { got: u32, want: u32 },
    Fat1 { got: u32 },
    /// `ClnShutBitMask` clear: the volume was not cleanly unmounted.
    VolumeDirty,
    /// `HrdErrBitMask` clear: the volume met a disk error.
    VolumeHardError,
    /// A FAT copy that mirroring says must match FAT 0 and does not.
    FatMirror { fat: u64, entry: u32, got: u32, want: u32 },

    /// A chain link that is neither a cluster of this volume, nor free, nor an
    /// end-of-chain mark.
    ChainOutOfRange { path: String, at: u32, next: u32, clusters: u32 },
    /// A chain link into the bad-cluster mark.
    ChainBadCluster { path: String, at: u32 },
    ChainCycle { path: String, at: u32, back_to: u32 },
    CrossLinked { path: String, at: u32, held_by: String },
    ChainTooShort { path: String, size: u64, held: u64, needed: u64 },
    ChainTooLong { path: String, size: u64, held: u64, needed: u64 },
    /// Clusters marked allocated that no chain reaches.
    LostChain { first: u32, clusters: u32 },

    /// A directory entry naming a first cluster outside the volume.
    FirstCluster { path: String, entry: u32, got: u32, clusters: u32 },
    DirectoryHasNoCluster { path: String },
    DirectorySize { path: String, size: u64 },
    /// A subdirectory whose first two entries are not `.` and `..`.
    DotEntry { path: String, entry: u32, want: &'static str, got: [u8; 11] },
    DotCluster { path: String, got: u32, want: u32 },
    DotDotCluster { path: String, got: u32, want: u32 },
    /// `.` or `..` in the root directory, which has neither.
    DotInRoot { got: [u8; 11] },
    /// `DIR_NTRes` carrying a bit nothing has ever defined.
    ReservedEntryByte { path: String, entry: u32, got: u8 },
    LongNameChecksum { path: String, entry: u32, got: u8, want: u8 },
    LongNameOrdinal { path: String, entry: u32, got: u8, want: u8 },
    /// The ordinal a long-name run opens with, which is the number of entries
    /// in it and cannot be 0 or above 20.
    LongNameRunLength { path: String, entry: u32, got: u8 },
    /// The first entry of a long-name run without `LAST_LONG_ENTRY`.
    LongNameLastFlag { path: String, entry: u32, got: u8 },
    /// A long-name run no short entry follows.
    OrphanLongName { path: String, entry: u32 },
    /// `LDIR_FstClusLO`, which the format requires to be zero.
    LongNameCluster { path: String, entry: u32, got: u16 },
    LongNameType { path: String, entry: u32, got: u8 },
    DuplicateShortName { path: String, name: [u8; 11] },
    VolumeLabelInSubdirectory { path: String, entry: u32 },
    ExtraVolumeLabel { count: u32 },
    /// The tree is nested past [`MAX_DEPTH`] and was not followed further.
    TooDeep { path: String },

    /// [`MAX_COMPLAINTS`] was reached and this many more were not reported.
    More { dropped: usize },
}

/// The complaints one per line, for a caller that is about to fail a test.
pub fn describe(complaints: &[Complaint]) -> String {
    let mut out = String::new();
    for (i, c) in complaints.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let _ = fmt::Write::write_fmt(&mut out, format_args!("{c}"));
    }
    out
}

/// Everything the format has to say about `volume`, which is one whole FAT32
/// volume: a partition's bytes, never a partitioned disk.
///
/// An empty answer is the assertion callers want. The checker reads and never
/// writes, and holds one `u32` per cluster plus the directory it is walking.
pub fn check(volume: &[u8]) -> Vec<Complaint> {
    let mut r = Report::new();
    let Some(geo) = boot::decode(volume, &mut r) else { return r.finish() };
    boot::signatures(volume, &geo, &mut r);
    let Some(table) = fat::read(volume, &geo, &mut r) else { return r.finish() };
    fat::head(&table, &geo, &mut r);
    fat::mirrors(volume, &geo, &mut r);
    let owners = dir::walk(volume, &geo, &table, &mut r);
    fat::lost(&table, &owners, &mut r);
    boot::free_count(volume, &geo, &table, &mut r);
    r.finish()
}

/// The growing complaint list, bounded by [`MAX_COMPLAINTS`].
pub(crate) struct Report {
    out: Vec<Complaint>,
    dropped: usize,
}

impl Report {
    fn new() -> Report {
        Report { out: Vec::new(), dropped: 0 }
    }

    pub(crate) fn say(&mut self, c: Complaint) {
        if self.out.len() < MAX_COMPLAINTS {
            self.out.push(c);
        } else {
            self.dropped += 1;
        }
    }

    fn finish(mut self) -> Vec<Complaint> {
        if self.dropped > 0 {
            self.out.push(Complaint::More { dropped: self.dropped });
        }
        self.out
    }
}

/// An 8.3 name field as the eleven bytes it is, with anything unprintable
/// escaped: a name whose first byte is `0xE5` or whose padding is not spaces is
/// exactly what a duplicate-name complaint is about.
struct Name<'a>(&'a [u8; 11]);

impl fmt::Display for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for &b in self.0 {
            if (0x20..0x7F).contains(&b) {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02X}")?;
            }
        }
        f.write_str("\"")
    }
}

impl fmt::Display for Complaint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Complaint::NoBootSector { bytes } => write!(
                f,
                "the volume is {bytes} bytes, which is not a boot sector; the format's smallest \
                 sector is 512"
            ),
            Complaint::JmpBoot { got } => write!(
                f,
                "boot sector: BS_jmpBoot is {:02X} {:02X} {:02X}; the format requires EB xx 90 or \
                 E9 xx xx",
                got[0], got[1], got[2]
            ),
            Complaint::BytesPerSector { got } => write!(
                f,
                "boot sector: BPB_BytsPerSec is {got}; the format allows 512, 1024, 2048 or 4096"
            ),
            Complaint::SectorsPerCluster { got } => write!(
                f,
                "boot sector: BPB_SecPerClus is {got}; the format requires a power of two from 1 \
                 to 128"
            ),
            Complaint::BytesPerCluster { got } => write!(
                f,
                "boot sector: a cluster is {got} bytes; the format allows at most 32768 and warns \
                 that above that many implementations fail"
            ),
            Complaint::ReservedSectors => write!(
                f,
                "boot sector: BPB_RsvdSecCnt is 0; the format requires at least the boot sector \
                 itself"
            ),
            Complaint::NumFats => {
                write!(f, "boot sector: BPB_NumFATs is 0; the format requires at least one FAT")
            }
            Complaint::RootEntryCount { got } => write!(
                f,
                "boot sector: BPB_RootEntCnt is {got}; FAT32 has no fixed root directory and \
                 requires 0"
            ),
            Complaint::TotalSectors16 { got } => write!(
                f,
                "boot sector: BPB_TotSec16 is {got}; FAT32 counts its sectors in BPB_TotSec32 and \
                 requires 0 here"
            ),
            Complaint::FatSize16 { got } => write!(
                f,
                "boot sector: BPB_FATSz16 is {got}; FAT32 sizes its FAT in BPB_FATSz32 and \
                 requires 0 here"
            ),
            Complaint::TotalSectors32 => write!(
                f,
                "boot sector: BPB_TotSec32 is 0; FAT32 requires the volume's sector count here"
            ),
            Complaint::FatSize32 => {
                write!(f, "boot sector: BPB_FATSz32 is 0; FAT32 requires the FAT's sector count here")
            }
            Complaint::BootSectorSignature { got } => write!(
                f,
                "boot sector: the signature at offset 510 is {got:#06X}; the format requires 0xAA55"
            ),
            Complaint::Media { got } => write!(
                f,
                "boot sector: BPB_Media is {got:#04X}; the format allows 0xF0 and 0xF8 to 0xFF"
            ),
            Complaint::FileSystemVersion { got } => write!(
                f,
                "boot sector: BPB_FSVer is {got:#06X}; this specification defines version 0:0 only"
            ),
            Complaint::FsInfoSector { got, reserved } => write!(
                f,
                "boot sector: BPB_FSInfo names sector {got}; FSInfo lives in the reserved region, \
                 which is sectors 1 to {}",
                reserved.saturating_sub(1)
            ),
            Complaint::ActiveFat { got, num_fats } => write!(
                f,
                "boot sector: BPB_ExtFlags makes FAT {got} the active one and the volume has \
                 {num_fats}"
            ),
            Complaint::NoDataArea { metadata_sectors, total_sectors } => write!(
                f,
                "geometry: the reserved sectors and the FATs come to {metadata_sectors} sectors of \
                 a volume BPB_TotSec32 gives {total_sectors}, so there is no data area"
            ),
            Complaint::VolumeShorterThanDeclared { declared_bytes, actual_bytes } => write!(
                f,
                "geometry: the boot sector describes at least {declared_bytes} bytes of volume and \
                 this one is {actual_bytes}"
            ),
            Complaint::NotFat32 { clusters } => write!(
                f,
                "geometry: the data area holds {clusters} clusters; the format defines a volume \
                 with fewer than 65525 as FAT12 or FAT16, so this is not a FAT32 volume"
            ),
            Complaint::FatTooSmall { fat_bytes, needed_bytes } => write!(
                f,
                "geometry: BPB_FATSz32 gives each FAT {fat_bytes} bytes and its entries need \
                 {needed_bytes}"
            ),
            Complaint::RootCluster { got, clusters } => write!(
                f,
                "boot sector: BPB_RootClus is {got}; the volume's clusters are 2 to {}",
                clusters + 1
            ),
            Complaint::FsInfoLeadSignature { got } => write!(
                f,
                "FSInfo: FSI_LeadSig is {got:#010X}; the format requires 0x41615252"
            ),
            Complaint::FsInfoStructSignature { got } => write!(
                f,
                "FSInfo: FSI_StrucSig is {got:#010X}; the format requires 0x61417272"
            ),
            Complaint::FsInfoTrailSignature { got } => write!(
                f,
                "FSInfo: FSI_TrailSig is {got:#010X}; the format requires 0xAA550000"
            ),
            Complaint::FsInfoFreeCount { declared, counted } => write!(
                f,
                "FSInfo: FSI_Free_Count is {declared} and the FAT has {counted} free clusters; the \
                 format allows 0xFFFFFFFF for unknown and nothing else that is not the count"
            ),
            Complaint::FsInfoFreeCountUnknown => write!(
                f,
                "FSInfo: FSI_Free_Count is 0xFFFFFFFF, which the format defines as unknown; every \
                 host reports this volume's free space from that field without counting, and a \
                 writer that has finished with the volume knows the number"
            ),
            Complaint::FsInfoNextFree { got, clusters } => write!(
                f,
                "FSInfo: FSI_Nxt_Free is {got}; the format allows 0xFFFFFFFF for unknown, and \
                 otherwise a cluster of this volume, which are 2 to {}",
                clusters + 1
            ),
            Complaint::Fat0 { got, want } => write!(
                f,
                "FAT[0] is {got:#010X}; the format requires {want:#010X}, which is BPB_Media in \
                 the low byte and ones above it"
            ),
            Complaint::Fat1 { got } => write!(
                f,
                "FAT[1] is {got:#010X}; the format requires an end-of-chain mark, so every bit \
                 below the two state flags must be set"
            ),
            Complaint::VolumeDirty => write!(
                f,
                "FAT[1] has ClnShutBitMask clear: the volume was not unmounted cleanly and its \
                 metadata may be mid-update"
            ),
            Complaint::VolumeHardError => write!(
                f,
                "FAT[1] has HrdErrBitMask clear: a driver met a read or write error on this \
                 volume and there may be bad sectors"
            ),
            Complaint::FatMirror { fat, entry, got, want } => write!(
                f,
                "FAT {fat} differs from FAT 0 at entry {entry}: {got:#010X} against {want:#010X}. \
                 BPB_ExtFlags has mirroring on, so every copy must carry every update"
            ),
            Complaint::ChainOutOfRange { path, at, next, clusters } => write!(
                f,
                "{path}: cluster {at} links to {next}, which is neither a cluster of this volume \
                 (2 to {}) nor an end-of-chain mark",
                clusters + 1
            ),
            Complaint::ChainBadCluster { path, at } => write!(
                f,
                "{path}: cluster {at} links to the bad-cluster mark 0x0FFFFFF7, which the format \
                 allows only outside a chain"
            ),
            Complaint::ChainCycle { path, at, back_to } => write!(
                f,
                "{path}: cluster {at} links back to {back_to}, which this chain already holds"
            ),
            Complaint::CrossLinked { path, at, held_by } => {
                write!(f, "{path}: cluster {at} is already held by {held_by}")
            }
            Complaint::ChainTooShort { path, size, held, needed } => write!(
                f,
                "{path}: DIR_FileSize is {size} bytes, which needs {needed} clusters, and the \
                 chain holds {held}"
            ),
            Complaint::ChainTooLong { path, size, held, needed } => write!(
                f,
                "{path}: DIR_FileSize is {size} bytes, which needs {needed} clusters, and the \
                 chain holds {held}"
            ),
            Complaint::LostChain { first, clusters } => write!(
                f,
                "{clusters} cluster(s) from {first} are marked allocated and no directory entry \
                 reaches them"
            ),
            Complaint::FirstCluster { path, entry, got, clusters } => write!(
                f,
                "{path}: entry {entry} names first cluster {got}; the volume's clusters are 2 to \
                 {}, and 0 means an empty file",
                clusters + 1
            ),
            Complaint::DirectoryHasNoCluster { path } => write!(
                f,
                "{path}: a directory entry with first cluster 0; a directory always holds at least \
                 its own . and .. entries"
            ),
            Complaint::DirectorySize { path, size } => write!(
                f,
                "{path}: DIR_FileSize is {size}; the format requires 0 on a directory, whose \
                 length is its cluster chain"
            ),
            Complaint::DotEntry { path, entry, want, got } => write!(
                f,
                "{path}: entry {entry} is {}; the format requires a subdirectory's entry {entry} \
                 to be {want}",
                Name(got)
            ),
            Complaint::DotCluster { path, got, want } => write!(
                f,
                "{path}: \".\" names cluster {got}; the format requires the directory's own \
                 cluster {want}"
            ),
            Complaint::DotDotCluster { path, got, want: 0 } => write!(
                f,
                "{path}: \"..\" names cluster {got}; the format requires 0 in a directory whose \
                 parent is the root, whatever BPB_RootClus is"
            ),
            Complaint::DotDotCluster { path, got, want } => write!(
                f,
                "{path}: \"..\" names cluster {got}; the format requires the parent's cluster {want}"
            ),
            Complaint::DotInRoot { got } => write!(
                f,
                "the root directory holds {}; the format gives the root neither a . nor a .. entry",
                Name(got)
            ),
            Complaint::ReservedEntryByte { path, entry, got } => write!(
                f,
                "{path}: entry {entry} has DIR_NTRes {got:#04X}; the format reserves that byte for \
                 Windows NT, which defines only 0x08 and 0x10 for a lowercase base and extension"
            ),
            Complaint::LongNameChecksum { path, entry, got, want } => write!(
                f,
                "{path}: entry {entry} is a long-name entry carrying checksum {got:#04X} of a \
                 short name whose checksum is {want:#04X}"
            ),
            Complaint::LongNameOrdinal { path, entry, got, want } => write!(
                f,
                "{path}: entry {entry} is long-name ordinal {got} where the run requires {want}; \
                 the format numbers them downward to 1"
            ),
            Complaint::LongNameRunLength { path, entry, got } => write!(
                f,
                "{path}: entry {entry} opens a long-name run of {got} entries; a long name is at \
                 most 255 characters and an entry holds 13, so a run is 1 to 20"
            ),
            Complaint::LongNameLastFlag { path, entry, got } => write!(
                f,
                "{path}: entry {entry} begins a long name with LDIR_Ord {got:#04X}; the format \
                 requires LAST_LONG_ENTRY (0x40) on the first entry of a run"
            ),
            Complaint::OrphanLongName { path, entry } => write!(
                f,
                "{path}: the long-name run at entry {entry} is followed by no short entry to name"
            ),
            Complaint::LongNameCluster { path, entry, got } => write!(
                f,
                "{path}: entry {entry} is a long-name entry naming cluster {got}; the format \
                 requires LDIR_FstClusLO to be 0"
            ),
            Complaint::LongNameType { path, entry, got } => write!(
                f,
                "{path}: entry {entry} has LDIR_Type {got:#04X}; the format defines 0 and reserves \
                 the rest"
            ),
            Complaint::DuplicateShortName { path, name } => write!(
                f,
                "{path}: two entries share the 8.3 name {}; the format identifies a file within a \
                 directory by that name, so one of them cannot be opened",
                Name(name)
            ),
            Complaint::VolumeLabelInSubdirectory { path, entry } => write!(
                f,
                "{path}: entry {entry} carries ATTR_VOLUME_ID; the format puts the one volume \
                 label in the root directory"
            ),
            Complaint::ExtraVolumeLabel { count } => write!(
                f,
                "the root directory holds {count} entries with ATTR_VOLUME_ID; the format allows \
                 one"
            ),
            Complaint::TooDeep { path } => write!(
                f,
                "{path}: the tree is nested past {MAX_DEPTH} directories and was not followed \
                 further"
            ),
            Complaint::More { dropped } => write!(
                f,
                "and {dropped} more, past the {MAX_COMPLAINTS} this checker enumerates"
            ),
        }
    }
}
