//! FAT32, read and write, over a byte-addressed volume.
//!
//! UEFI mandates FAT32 for the EFI System Partition, so this is not a legacy
//! exception — it is the current firmware contract, and a UEFI OS that cannot
//! read the filesystem it booted from is unfinished. The immediate use is
//! writing logs on a machine with no serial port.
//!
//! # A volume is untrusted input
//!
//! The bytes come off a USB stick that anything could have written, so every
//! rule that governs a syscall argument governs a directory entry here. No
//! path that touches on-disk bytes may panic: no `unwrap`, no `expect`, no
//! indexing by a disk-derived value, no arithmetic that can overflow. Every
//! such failure is an [`Error`].
//!
//! Three specific hazards, and what closes each:
//!
//! - **Cluster chains can be cyclic.** Every walk in this crate carries an
//!   explicit step bound, and the bound comes from something the chain cannot
//!   influence: a file's own size field for data reads, [`MAX_DIR_ENTRIES`]
//!   for directories, the volume's cluster count for a free. A cycle is
//!   [`Error::CorruptChain`], never a hang. This is why there is no
//!   tortoise-and-hare anywhere — bounding by size is both stricter and
//!   cheaper than doubling every FAT read to detect a cycle we would refuse
//!   anyway.
//! - **Directory trees can be cyclic.** [`Fat32::walk`] is iterative, carries
//!   a visited set of directory clusters, and never follows `.` or `..`.
//! - **Counts can be absurd.** Every allocation derived from the volume is
//!   bounded: listings by the caller's `limit`, names by [`MAX_LFN_CHARS`],
//!   directories by [`MAX_DIR_ENTRIES`], extents by the caller's `max`.
//!
//! # What this crate does not do
//!
//! - **No formatting.** The kernel never formats a disk it was not given, and
//!   the ESP is made by firmware. There is no code here that could write a
//!   BPB, so there is no code here that could destroy one.
//! - **No symlinks.** FAT32 has no representation for one. There is
//!   deliberately no `create_symlink` to call: a VFS adapter must return an
//!   error from its own `create_symlink`, and `read_link` must always answer
//!   `None`. Succeeding silently would leave a caller holding a regular file
//!   it believes is a link.
//! - **No caching.** The kernel's page cache sits directly under
//!   [`BlockAccess`]. A second cache here would be a coherence hazard for no
//!   gain, since every FAT sector this crate re-reads is a hit in that one.
//! - **No FAT12/FAT16.** The cluster count decides the FAT type, per the
//!   specification, and a volume with fewer than [`MIN_FAT32_CLUSTERS`]
//!   clusters is not FAT32 no matter what its boot sector says.
//!
//! # Shape
//!
//! [`Fat32`] owns a [`BlockAccess`] and nothing else that persists. Path-based
//! calls ([`Fat32::create`], [`Fat32::remove`], [`Fat32::metadata`], …) resolve
//! from the root every time; [`Fat32::open`] hands back a [`File`] that caches
//! the directory-entry location and a chain position, so repeated I/O on one
//! file does not re-resolve the path or re-walk the chain from cluster zero.
//! A [`File`] is plain data with no lifetime tie to the volume, so it can go
//! stale — every operation that uses one re-reads the directory entry and
//! checks it still names the same chain, turning staleness into
//! [`Error::NotFound`] instead of a write to whatever file took its place.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod boot;
mod device;
mod dir;
mod error;
mod fat;
mod fs;
mod name;
mod time;

pub use boot::{Geometry, MIN_FAT32_CLUSTERS};
pub use device::{BlockAccess, IoError};
pub use dir::MAX_DIR_ENTRIES;
pub use error::Error;
pub use fs::{DirEntry, Extent, Fat32, File, Metadata};
pub use name::MAX_LFN_CHARS;
pub use time::FatTime;
