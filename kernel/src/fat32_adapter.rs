//! The two partitions this kernel was given, as mounted filesystems.
//!
//! UEFI mandates FAT32 on the partition firmware loads a bootloader from, so
//! this is the one filesystem a ToyOS machine is guaranteed to have before it
//! has any other. The log partition beside it is FAT32 for a different reason —
//! it is the format every desktop OS mounts off a stick without being asked.
//! `toyos-fat32` reads and writes both; this file is the two things that crate
//! deliberately does not know about — the kernel's 4 KiB [`BlockDevice`], and
//! [`vfs::FileSystem`].
//!
//! # Why neither can become "some disk we found"
//!
//! Three independent gates, and a volume is untouched unless all three pass:
//!
//! 1. **Which partition.** [`gpt::boot_volume`] and [`gpt::log_volume`] answer,
//!    and they answer only for unique partition GUIDs the kernel was handed
//!    through `KernelArgs` — the boot partition's from firmware, cross-checked
//!    against the table's own extent and accepted only when exactly one device
//!    carries it, and the log partition's from a file on that volume, looked
//!    for only on the device the boot partition was found on. Nothing here
//!    scans for a FAT signature and nothing here looks at a partition *type*.
//!    In particular the log partition is not "the other FAT32 on the stick",
//!    which would have needed no handoff at all and is the defect the handoff
//!    exists to make unrepresentable.
//! 2. **Which bytes.** [`FatDevice`] clamps every read and every write to the
//!    volume before it reaches the device, so a filesystem that computed a wild
//!    offset gets [`IoError`] rather than a neighbour's blocks. This is the
//!    adapter's invariant and not the filesystem's to be trusted about:
//!    `BlockAccess`'s own documentation claims the crate "never asks for bytes
//!    it has not already bounded against the volume", and the storage-stack
//!    audit reproduced a crafted directory entry driving a write 256 GiB past
//!    the end of one. A driver escaping its partition is how a boot stick's
//!    other partitions get destroyed — and with two mounted at once, the
//!    neighbour it would escape into is the other one.
//!
//!    The bound is the *volume*, tighter than the partition: [`Fat32::probe`]
//!    reads the boot sector without mounting, and the sector count in it is
//!    what the filesystem may legitimately address. Slack between the volume
//!    and the end of the partition is then unreachable too. A partition's first
//!    byte need not be 4 KiB-aligned in general, so a write to it is a
//!    read-modify-write of a device block it shares with whatever is next to
//!    it — which preserves those bytes rather than authoring them.
//! 3. **Whether it is already ours.** `toyos-fat32` contains no code that can
//!    write a BPB. A volume that does not parse as FAT32 makes [`mount`] return
//!    `None` after nothing but reads, and there is no path from there to a
//!    format, because no such path exists to take.
//!
//! # What these mounts are not
//!
//! Not a general FAT32 mount service. Two volumes, both named by the handoff,
//! mounted once at boot. A third FAT32 partition on the same disk is not
//! reachable from here and should not become reachable without the same three
//! gates.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hashbrown::HashMap;

use toyos_abi::syscall::SyscallError;
use toyos_fat32::{BlockAccess, Error, Extent, Fat32, FatTime, IoError};

use crate::block::BlockDevice;
use crate::drivers::{usb_storage, xhci};
use crate::file_backing::FileBacking;
use crate::file_cache::{self, FileId};
use crate::gpt;
use crate::sync::Lock;
use crate::vfs::FileSystem;

/// The only transfer unit [`BlockDevice`] has.
const BLOCK: u64 = 4096;

/// Which of the two partitions a mount is.
///
/// A mount is named for its role and never for its format: `/esp` would say
/// what the filesystem is, and selecting a volume by what it looks like is the
/// mistake [`gpt`] exists to make unrepresentable. Both of these are FAT32 and
/// neither is mounted for being FAT32.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// The partition firmware loaded the bootloader from.
    Boot,
    /// The partition that volume names, which the kernel's log goes on. Its own
    /// partition because macOS never auto-mounts an EFI-typed one, so a log on
    /// the ESP is unreadable on the machine the owner would read it from.
    Log,
}

impl Role {
    /// The VFS mount name, which is also the top-level directory.
    pub fn mount(self) -> &'static str {
        match self {
            Role::Boot => "boot",
            Role::Log => "log",
        }
    }

    fn slot(self) -> usize {
        match self {
            Role::Boot => 0,
            Role::Log => 1,
        }
    }

    fn volume(self) -> Option<gpt::Volume> {
        match self {
            Role::Boot => gpt::boot_volume(),
            Role::Log => gpt::log_volume(),
        }
    }
}

impl core::fmt::Display for Role {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.mount())
    }
}

/// Extents one file's data may be split into before [`Fat32::extents`] refuses.
///
/// Derived, not picked. An [`Extent`] is two `u64`s, and the `Vec` holding them
/// must stay under `mm::MAX_HEAP_ALLOC` (2_093_056) including the transient
/// request `Vec`'s growth-by-doubling makes: 65_536 is a power of two, so the
/// last allocation is exactly `65_536 * 16 = 1_048_576` and the one before it
/// half that. A file needing more than this is one whose every cluster is
/// discontiguous from the last — at the 4 KiB clusters an ESP of a few hundred
/// megabytes gets, that is a 256 MiB file, and refusing to open it is better
/// than a `Vec` the allocator would refuse anyway.
const MAX_EXTENTS: usize = 65_536;

const _: () = assert!(core::mem::size_of::<Extent>() == 16);

/// One partition, seen as a byte range, over a device that only does whole
/// 4 KiB blocks.
///
/// Offsets are relative to the partition. Nothing above this struct can name a
/// byte outside it, which is the property that makes a filesystem bug on this
/// volume a filesystem bug rather than damage to the disk it sits on.
///
/// # Why blocks stay resident
///
/// This boundary is where read amplification is *created*, so it is where it
/// has to be paid off. `toyos-fat32` reasons in the volume's own units — a FAT
/// entry is four bytes — while the device's only transfer unit is 4096, so a
/// chain walk cost one USB transfer per cluster and the volumes this project
/// builds have 512-byte clusters. One device block covers 1024 FAT entries;
/// re-reading it per entry is the whole cost.
///
/// # Why keeping copies is sound here
///
/// Nothing is ever held back: a write reaches the device before this returns,
/// and the copy is updated (partial block) or dropped (whole blocks) as part
/// of issuing it. So the resident set can be stale only if something else
/// writes these blocks, and after [`mount`] nothing can. There is one of these
/// per [`Role`] and no other handle to either partition: `probe_boot_disks`
/// opens its own and only reads, before the mounts; `usb_gate` writes only a
/// disk whose block 0 carries the designation stamp, which this one does not.
///
/// The two roles' resident sets cannot alias each other either, and that is a
/// property of the *image* rather than of this code: `create_gpt_disk` aligns
/// both partitions to 1 MiB and asserts that no 4 KiB device block belongs to
/// both. Unaligned, the ESP ended a quarter of the way into a block the log
/// partition began in, and each mount's copy of it would have gone stale on the
/// other's write with nothing here able to see it.
struct FatDevice {
    dev: Box<dyn BlockDevice>,
    /// Where the partition starts, in bytes from the start of the device.
    start: u64,
    /// How many bytes it has.
    len: u64,
    /// One device block, for the partial-block ends of a request. On the heap
    /// because the deepest caller of this is the idle loop, whose stack is
    /// 16 KiB and has no guard page.
    scratch: Vec<u8>,
    /// [`RESIDENT_BLOCKS`] blocks, each tagged with the block it holds.
    resident: Vec<u8>,
    tags: [Option<u64>; RESIDENT_BLOCKS],
    /// Round-robin, because the access pattern this exists for touches every
    /// resident block on every append. Recency cannot rank blocks that are all
    /// used once per operation, so anything cleverer would cost a counter to
    /// arrive at the same eviction.
    next_victim: usize,
}

/// Device blocks [`FatDevice`] keeps a copy of.
///
/// Sized to hold what one append touches at once, which is what makes the
/// difference between one device read per FAT entry and one per operation:
/// the active FAT's block for the file's clusters, the mirror FAT's block at
/// the same index (`set_fat_entry` writes every FAT and a 4-byte write is a
/// read-modify-write), the directory block carrying the entry, the FSInfo
/// block, and the data block being appended to. That is five, or seven when
/// the file's chain straddles a FAT block boundary.
const RESIDENT_BLOCKS: usize = 8;

impl FatDevice {
    /// The device byte offset `offset` names, or [`IoError`] if the request
    /// leaves the partition. Every read and write goes through here.
    fn locate(&self, offset: u64, len: usize) -> Result<u64, IoError> {
        let end = offset.checked_add(len as u64).ok_or(IoError)?;
        if end > self.len {
            return Err(IoError);
        }
        Ok(self.start + offset)
    }

    fn slot_of(&self, block: u64) -> Option<usize> {
        self.tags.iter().position(|&t| t == Some(block))
    }

    /// Leave `block` in `scratch`, reading it only if it is not already here.
    fn load(&mut self, block: u64) -> Result<(), IoError> {
        if let Some(slot) = self.slot_of(block) {
            let at = slot * BLOCK as usize;
            self.scratch.copy_from_slice(&self.resident[at..at + BLOCK as usize]);
            return Ok(());
        }
        let Self { dev, scratch, .. } = self;
        dev.read_blocks(block, 1, scratch).map_err(|_| IoError)?;
        self.retain(block);
        Ok(())
    }

    /// Record `scratch` as this device's `block`. Only ever called where the
    /// device holds those bytes already — after reading them, or after writing
    /// them — so nothing here is a copy the disk is waiting for.
    fn retain(&mut self, block: u64) {
        let slot = self.slot_of(block).unwrap_or_else(|| {
            let s = self.next_victim;
            self.next_victim = (s + 1) % RESIDENT_BLOCKS;
            s
        });
        let at = slot * BLOCK as usize;
        self.resident[at..at + BLOCK as usize].copy_from_slice(&self.scratch);
        self.tags[slot] = Some(block);
    }

    fn forget(&mut self, first: u64, count: u64) {
        for tag in &mut self.tags {
            if tag.is_some_and(|b| b >= first && b < first + count) {
                *tag = None;
            }
        }
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let base = self.locate(offset, buf.len())?;
        let mut done = 0usize;
        while done < buf.len() {
            let at = base + done as u64;
            let block = at / BLOCK;
            let within = (at % BLOCK) as usize;
            let left = buf.len() - done;
            if within == 0 && left >= BLOCK as usize {
                let count = left / BLOCK as usize;
                let end = done + count * BLOCK as usize;
                self.dev
                    .read_blocks(block, count as u32, &mut buf[done..end])
                    .map_err(|_| IoError)?;
                done = end;
            } else {
                let n = (BLOCK as usize - within).min(left);
                self.load(block)?;
                buf[done..done + n].copy_from_slice(&self.scratch[within..within + n]);
                done += n;
            }
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        let base = self.locate(offset, buf.len())?;
        let mut done = 0usize;
        while done < buf.len() {
            let at = base + done as u64;
            let block = at / BLOCK;
            let within = (at % BLOCK) as usize;
            let left = buf.len() - done;
            if within == 0 && left >= BLOCK as usize {
                let count = left / BLOCK as usize;
                let end = done + count * BLOCK as usize;
                self.forget(block, count as u64);
                self.dev
                    .write_blocks(block, count as u32, &buf[done..end])
                    .map_err(|_| IoError)?;
                done = end;
            } else {
                // The bytes this request does not cover belong to whoever wrote
                // them — another file, or the partition table itself when the
                // partition does not start on a 4 KiB boundary.
                let n = (BLOCK as usize - within).min(left);
                self.load(block)?;
                self.scratch[within..within + n].copy_from_slice(&buf[done..done + n]);
                let Self { dev, scratch, .. } = self;
                dev.write_blocks(block, 1, scratch).map_err(|_| IoError)?;
                self.retain(block);
                done += n;
            }
        }
        Ok(())
    }
}

/// Each mounted partition's device, reachable without the VFS lock.
///
/// Statics for the same reason `page_cache`'s device is one: a [`FileBacking`]
/// serves a page-fault miss with `&self` and no filesystem in hand, so the
/// device cannot live inside the `Box<dyn FileSystem>` the VFS owns. Lock
/// order is VFS → here → `XHCI`; nothing takes them the other way, and no path
/// holds two of these at once — [`Role`] indexes one of them and every caller
/// has exactly one role in hand.
static VOLUMES: [Lock<Option<FatDevice>>; 2] = [Lock::new(None), Lock::new(None)];

fn device(role: Role) -> &'static Lock<Option<FatDevice>> {
    &VOLUMES[role.slot()]
}

/// One [`VOLUMES`] entry in the shape `toyos-fat32` asks for.
///
/// Zero state beyond the role and the capacity, so the filesystem holding one
/// of these can be moved into the VFS while the device stays put.
pub struct FatVolume {
    role: Role,
    bytes: u64,
}

impl BlockAccess for FatVolume {
    fn capacity(&self) -> u64 {
        self.bytes
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let mut guard = device(self.role).lock();
        guard.as_mut().ok_or(IoError)?.read_at(offset, buf)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError> {
        let mut guard = device(self.role).lock();
        guard.as_mut().ok_or(IoError)?.write_at(offset, buf)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        let mut guard = device(self.role).lock();
        guard.as_mut().ok_or(IoError)?.dev.flush().map_err(|_| IoError)
    }
}

/// Whether a page re-read of a file on either of these volumes is allowed to
/// succeed.
///
/// A kernel feature because nothing on the host side can stage it: both
/// partitions are on the disk the guest is running from, so every QEMU-side way
/// to make its reads fail — `readonly=on` is writes only, detaching the device,
/// a smaller image — takes away the mount the failure has to be observed
/// through, or the kernel the machine is running. `werror`/`rerror` act on the
/// whole drive and would break the boot before there is a log to write.
///
/// The read is still *issued*, and only its verdict is replaced. Overriding it
/// before the call would make this the mistake `FLUSH_SENSE` makes — an
/// injection that also hides a broken transport, so the gate goes green on a
/// boot where the device was never asked. Same reason `xhci-one-slot` and
/// `i8042-fault` exist.
const FAT_BACKING_READS: bool = !cfg!(feature = "fat-backing-read-fails");

/// A file on one of these volumes, as byte ranges the page-fault path can read
/// without going back through the filesystem.
struct FatBacking {
    role: Role,
    extents: Vec<Extent>,
    size: u64,
}

impl FileBacking for FatBacking {
    fn read_page(&self, file_offset: u64, buf: &mut [u8; 4096]) -> crate::block::BlockResult {
        buf.fill(0);
        if file_offset >= self.size {
            return Ok(());
        }
        let valid = (4096u64).min(self.size - file_offset) as usize;
        let mut done = 0usize;
        // Where the extent under consideration starts, in file bytes. A page
        // can span two of them whenever the volume's cluster is smaller than
        // 4096, which a volume a few tens of megabytes across usually is.
        let mut base = 0u64;
        for extent in &self.extents {
            if done >= valid {
                return Ok(());
            }
            let want = file_offset + done as u64;
            if want >= base + extent.len {
                base += extent.len;
                continue;
            }
            let within = want - base;
            let n = ((extent.len - within) as usize).min(valid - done);
            let mut guard = device(self.role).lock();
            let Some(dev) = guard.as_mut() else {
                // Only reachable if a mount failed after installing the device
                // — but silence here is what the other two backings do not do,
                // and `serving zeros` is the string a triage greps for.
                log!("{}-volume: not mounted; serving zeros", self.role);
                return Err(crate::block::BlockError);
            };
            let served = dev.read_at(extent.offset + within, &mut buf[done..done + n]);
            if !FAT_BACKING_READS {
                // The read above was issued and is the shipped one, so a
                // transport that really broke is still what `served` says —
                // the injection cannot hide it. What it does replace is the
                // whole outcome of a failed read, verdict *and* buffer: a real
                // failure returns here with these bytes still holding the zeros
                // from `buf.fill(0)`, and a caller that got the data anyway
                // would make every assertion about the consequences vacuous.
                buf[done..done + n].fill(0);
            }
            if served.is_err() || !FAT_BACKING_READS {
                log!("{}-volume: read of {n} B at volume offset {} failed; serving zeros",
                    self.role, extent.offset + within);
                return Err(crate::block::BlockError);
            }
            drop(guard);
            done += n;
            base += extent.len;
        }
        Ok(())
    }

    fn file_size(&self) -> u64 {
        self.size
    }
}

/// Per-open-file state: the path it was opened by, and the crate's own handle,
/// which caches the directory-entry location and a chain position.
///
/// Keeping the handle is what makes an append cost one FAT read instead of a
/// walk from cluster zero — `write_page` is called once per dirty page and
/// re-resolving the path each time would be quadratic in the file's length.
struct OpenFile {
    name: String,
    file: toyos_fat32::File,
}

/// VFS adapter for one of the two partitions.
///
/// A file's identity here is its **path**, which is what [`by_name`] keys on.
/// The trait requires the same [`FileId`] for the same file across opens, and
/// the two other candidates cannot give that: the directory entry's location
/// is reused the moment an entry is erased, so a location-keyed id would let a
/// new file inherit a deleted one's cached pages, and the crate's stale-handle
/// fingerprint (8.3 name plus creation timestamp) cannot tell a
/// delete-and-recreate within the same two-second timestamp from the original,
/// so it is not a generation counter and is not used as one.
///
/// Path identity makes delete-and-recreate produce a *new* `FileId`, because
/// `delete` drops the name; and it survives a rename, because `rename`
/// re-keys. What it cannot survive is an fd held across an unlink — see
/// [`FileSystem::delete`].
///
/// [`by_name`]: FatFs::by_name
pub struct FatFs {
    role: Role,
    fs: Fat32<FatVolume>,
    open: HashMap<FileId, OpenFile>,
    by_name: HashMap<String, FileId>,
}

/// What to stamp on an entry this adapter is writing.
///
/// FAT stores wall-clock time and the VFS's `mtime` is nanoseconds since boot,
/// so the number the trait hands this adapter is not a time of day and cannot
/// go on an entry. It comes from `clock` instead, which read the RTC once for
/// the whole machine — this used to be a per-volume reading, so a machine with
/// both volumes mounted had two answers to what time it booted.
///
/// Local and not UTC: FAT stores local time by specification. A machine with no
/// wall clock stamps [`FatTime::EPOCH`], which is what the format itself uses
/// for an entry nobody dated.
fn now() -> FatTime {
    crate::clock::local_secs().map_or(FatTime::EPOCH, FatTime::from_unix_secs)
}

impl FatFs {
    fn new(role: Role, fs: Fat32<FatVolume>) -> Self {
        Self { role, fs, open: HashMap::new(), by_name: HashMap::new() }
    }

    fn backing(&mut self, name: &str) -> Option<Arc<dyn FileBacking>> {
        let size = self.fs.metadata(name).ok()?.len;
        match self.fs.extents(name, MAX_EXTENTS) {
            Ok(extents) => Some(Arc::new(FatBacking { role: self.role, extents, size })),
            Err(e) => {
                log!("{}-volume: {name} has no readable extent list: {e}", self.role);
                None
            }
        }
    }

    /// Make sure every directory on the way to `name` exists.
    ///
    /// The VFS has no per-mount `mkdir` — `Vfs::create_dir` records a name in
    /// its own set and tells no filesystem — so a `create` of `a/b/c.txt` is
    /// the only notice this mount ever gets that `a/b` was wanted. Every other
    /// mount is a flat namespace where the question does not arise.
    fn ensure_parent(&mut self, name: &str, time: FatTime) -> Result<(), Error> {
        let Some((parent, _)) = name.rsplit_once('/') else { return Ok(()) };
        self.fs.create_dir_all(parent, time)
    }
}

impl FileSystem for FatFs {
    /// The bound is honoured before the allocation, not after it.
    ///
    /// `Fat32::walk` checks `limit` against the count it has *before* each
    /// push, for files and for directories alike, and abandons the listing
    /// rather than truncating it. So this is the second implementation of this
    /// trait that can meet its stated contract; the two bcachefs adapters
    /// still cannot.
    fn list(&mut self, limit: usize) -> Result<Vec<(String, u64)>, SyscallError> {
        match self.fs.walk(limit) {
            Ok(names) => Ok(names),
            Err(Error::LimitExceeded) => Err(SyscallError::ResourceExhausted),
            Err(e) => {
                log!("{}-volume: cannot list it: {e}", self.role);
                Err(SyscallError::NotFound)
            }
        }
    }

    fn file_size(&mut self, name: &str) -> Option<u64> {
        let meta = self.fs.metadata(name).ok()?;
        (!meta.is_dir).then_some(meta.len)
    }

    fn file_mtime(&mut self, name: &str) -> u64 {
        self.fs.metadata(name).map_or(0, |m| m.modified_unix)
    }

    /// Always `None`. FAT32 has no representation for a symbolic link, and
    /// answering anything else would hand the caller a regular file it
    /// believes is a link.
    fn read_link(&mut self, _name: &str) -> Option<String> {
        None
    }

    fn open_file(&mut self, name: &str) -> Option<(FileId, Option<Arc<dyn FileBacking>>)> {
        if let Some(&file_id) = self.by_name.get(name) {
            file_cache::open(file_id);
            return Some((file_id, self.backing(name)));
        }
        let file = self.fs.open(name).ok()?;
        let size = file.len();
        let backing = self.backing(name)?;

        let file_id = file_cache::create_file(true);
        file_cache::set_size(file_id, size);
        self.by_name.insert(String::from(name), file_id);
        self.open.insert(file_id, OpenFile { name: String::from(name), file });
        Some((file_id, Some(backing)))
    }

    fn create(&mut self, name: &str, _mtime: u64) -> Result<FileId, &'static str> {
        if let Some(&file_id) = self.by_name.get(name) {
            return Ok(file_id);
        }
        let time = now();
        self.ensure_parent(name, time).map_err(|e| e.as_str())?;
        let file = match self.fs.create(name, time) {
            Ok(file) => file,
            // `vfs::create_file` is also how an existing file is reopened for
            // writing, so an existing name is not an error here — but it is in
            // the crate, deliberately, because a create that silently opened
            // somebody else's file is how a caller comes to believe it owns
            // bytes it does not.
            Err(Error::AlreadyExists) => self.fs.open(name).map_err(|e| e.as_str())?,
            Err(e) => return Err(e.as_str()),
        };
        let file_id = file_cache::create_file(true);
        file_cache::set_size(file_id, file.len());
        self.by_name.insert(String::from(name), file_id);
        self.open.insert(file_id, OpenFile { name: String::from(name), file });
        Ok(file_id)
    }

    fn close_file(&mut self, file_id: FileId) {
        if file_cache::ref_count(file_id) == 0 {
            if let Some(info) = self.open.remove(&file_id) {
                self.by_name.remove(&info.name);
            }
        }
    }

    /// Unlink, and drop the write handle whether or not an fd still holds the
    /// file.
    ///
    /// Unconditionally, unlike the bcachefs adapters, and that is the point.
    /// `remove` below frees the chain and erases the entry, so the cached
    /// `toyos_fat32::File` now names clusters the allocator is free to hand to
    /// the next file — and a later `write_page` through it would put one
    /// process's bytes inside another's. Dropping it turns that into
    /// `"file not open"` from `write_page`, which `fd::close` discards, so an
    /// fd held across an unlink can no longer write the file back. That is the
    /// right answer: the file it would write back does not exist.
    ///
    /// The read side is not closed here — the `FatBacking` an open fd already
    /// holds still names those byte ranges, which is the same live
    /// cross-process leak known issues records for `/home`. This closes the
    /// destructive half only.
    fn delete(&mut self, name: &str) -> bool {
        if let Some(file_id) = self.by_name.remove(name) {
            file_cache::mark_deleted(file_id);
            self.open.remove(&file_id);
        }
        match self.fs.remove(name) {
            Ok(()) => true,
            Err(Error::NotFound) => false,
            Err(e) => {
                log!("{}-volume: cannot delete {name}: {e}", self.role);
                false
            }
        }
    }

    fn delete_prefix(&mut self, prefix: &str) {
        let Ok(names) = self.fs.walk(crate::vfs::MAX_LIST_ENTRIES) else {
            log!("{}-volume: cannot enumerate {prefix} to delete it", self.role);
            return;
        };
        let doomed: Vec<String> =
            names.into_iter().map(|(n, _)| n).filter(|n| n.starts_with(prefix)).collect();
        for name in doomed {
            self.delete(&name);
        }
    }

    /// Rename, deleting the destination first when one exists.
    ///
    /// FAT has no atomic replacement, so `Fat32::rename` refuses a destination
    /// that exists rather than opening a window in which neither name
    /// resolves. The VFS's callers want POSIX overwrite, so the window is
    /// opened here, where it is visible: between the delete and the rename
    /// below, neither name names the old file's data.
    fn rename(&mut self, old: &str, new: &str) -> Result<(), &'static str> {
        if self.fs.exists(new).map_err(|e| e.as_str())? {
            self.delete(new);
        }
        self.fs.rename(old, new).map_err(|e| e.as_str())?;
        if let Some(file_id) = self.by_name.remove(old) {
            self.by_name.insert(String::from(new), file_id);
            if let Some(info) = self.open.get_mut(&file_id) {
                info.name = String::from(new);
            }
        }
        Ok(())
    }

    fn write_page(
        &mut self,
        file_id: FileId,
        page_idx: u32,
        data: &[u8; 4096],
    ) -> Result<(), &'static str> {
        let Self { fs, open, .. } = self;
        let info = open.get_mut(&file_id).ok_or("file not open")?;
        fs.write(&mut info.file, page_idx as u64 * 4096, data).map_err(|e| e.as_str())
    }

    /// Record the file's real length and stamp it, then re-derive its backing.
    ///
    /// The length matters because `write_page` writes whole pages: the last
    /// one carries the cache's zero padding, so without this the entry would
    /// claim a page-rounded size. The backing matters because the pages the
    /// flush just wrote are now evictable, and the extents captured when the
    /// file was opened do not cover the clusters this write allocated —
    /// evicting one of those pages against a stale extent list reads back
    /// zeroes.
    fn update_metadata(
        &mut self,
        file_id: FileId,
        size: u64,
        _mtime: u64,
    ) -> Result<(), &'static str> {
        let time = now();
        let name = {
            let Self { fs, open, .. } = self;
            let info = open.get_mut(&file_id).ok_or("file not open")?;
            if info.file.len() != size {
                fs.set_len(&mut info.file, size).map_err(|e| e.as_str())?;
            }
            fs.flush_meta(&mut info.file, time).map_err(|e| e.as_str())?;
            info.name.clone()
        };
        if let Some(backing) = self.backing(&name) {
            file_cache::set_backing(file_id, backing);
        }
        Ok(())
    }

    /// Always an error. See [`FileSystem::read_link`] above and the crate's
    /// own documentation: there is deliberately nothing here to call.
    fn create_symlink(&mut self, _name: &str, _target: &str) -> Result<(), &'static str> {
        Err("FAT32 has no symlinks")
    }

    /// The error is returned rather than logged, and that is the whole point of
    /// the signature: the log mount is where the kernel's own log lives, so a
    /// line written here is pending ring content, which is the next flush,
    /// which is the next sync. Swallowing it made a device that declines to
    /// flush into a permanent write loop from the idle loop.
    ///
    /// A `&'static str` and so not role-tagged: the caller names the file it was
    /// syncing, which says which volume it was on.
    fn sync(&mut self) -> Result<(), &'static str> {
        self.fs.sync().map_err(|e| match e {
            Error::Io => "the volume's device refused the sync",
            _ => "the volume would not sync",
        })
    }

    fn open_backing(&mut self, name: &str) -> Option<Arc<dyn FileBacking>> {
        self.backing(name)
    }
}

/// Ask every USB disk whether it carries the partitions this kernel was given,
/// and do not stop asking while the machine is still missing the one it was
/// booted from.
///
/// Read-only, and the missing half of the GPT work: `gpt::probe` ran for NVMe
/// only, so on a machine that boots off a stick — which is every machine this
/// project boots — `gpt::boot_volume()` answered `None` and no mount could
/// ever ask it anything.
///
/// # Why this is a loop and not a pass
///
/// It ran once, at a fixed point in the peripheral phase, against a disk set
/// that is **not final at that point**. `xhci::await_connect_settle` returns as
/// soon as the root hub's connect set has held still for the USB debounce and is
/// non-empty, so a machine whose *other* devices are up scans without whatever
/// is still arriving — and the T14 has four internal USB devices beside the
/// stick it boots from. That machine reached a working compositor desktop with
/// neither `/boot` nor `/log` on one boot and mounted both on the next, off the
/// same stick and the same image, which is a race and not a defect in anything
/// downstream of here.
///
/// The asymmetry is the same one `xhci::EMPTY_BUS_NS` is written around, and it
/// is what keeps this free: a machine whose boot volume has already been
/// resolved — every QEMU boot, every machine that boots off NVMe, and the T14 on
/// a good boot — leaves after one pass, because `gpt::boot_volume()` answers.
/// Only a machine that would otherwise report no boot volume at all pays
/// anything, and that is the outcome this exists to prevent.
///
/// # Why every refusal here has a line
///
/// The end of this function is a machine with no `/boot` and no `/log`, on
/// hardware whose only other diagnostic channel *is* `/log`. Skipping a disk
/// silently — which is what a bare `continue` did, three times over — spends the
/// one chance anybody has to find out why.
pub fn probe_boot_disks() {
    let deadline = crate::clock::nanos_since_boot() + xhci::PORT_SETTLE_CEILING_NS;
    let mut probed = 0;
    loop {
        probed = probe_announced(probed);
        // Nothing to wait for. Three ways, and the third is the one worth
        // stating: firmware named no partition, so there is no question; or a
        // device carries it; or two devices do, which no further device can
        // repair — `gpt:` has already named that machine as having no boot
        // volume, and waiting the ceiling out would be waiting for an event
        // that cannot occur.
        if gpt::boot_partition().is_none() || !gpt::boot_volume_still_possible() {
            return;
        }
        if crate::clock::nanos_since_boot() >= deadline {
            log!(
                "usb-storage: {probed} disk(s) on this machine and none carries the boot \
                 partition after {} ms of looking — this boot has no /boot and no /log",
                xhci::PORT_SETTLE_CEILING_NS / 1_000_000
            );
            return;
        }
        // Paced rather than spun, at the cadence the connect settle already
        // reads port registers on: each pass is one MMIO read per port under
        // the controller lock, and the thing being waited for is physical.
        let next = crate::clock::nanos_since_boot() + xhci::PORT_POLL_NS;
        while crate::clock::nanos_since_boot() < next {
            core::hint::spin_loop();
        }
        xhci::recheck_ports();
    }
}

/// Probe every disk announced since the last call, and return the new count.
///
/// Indices are stable and dense — a disk keeps the index it was bound under —
/// so "since" is a count, and a disk is never probed twice.
fn probe_announced(mut probed: usize) -> usize {
    let count = usb_storage::count();
    while probed < count {
        let index = probed;
        probed += 1;
        // One question with one answer. This used to ask twice — `open` for the
        // handle and `storage_geometry` for the block size — which is two `None`
        // branches for one fact, and the disk carried the block size all along.
        let Some(mut disk) = usb_storage::open(index) else {
            log!(
                "usb-storage: disk {index} was announced and was gone again before its partition \
                 table could be read — not probed"
            );
            continue;
        };
        let lba_bytes = disk.logical_block_bytes();
        gpt::probe(&mut disk, lba_bytes);
    }
    probed
}

/// The bound disk carrying `id`, or `None` when no driver here serves it.
///
/// Only USB today. A machine that boots off an internal disk lands in the
/// `None` arm and gets neither mount, because the NVMe device is owned by the
/// page cache from the moment storage comes up and there is no second handle
/// to it — see the report in known issues rather than a workaround here.
fn device_carrying(id: crate::block::DeviceId) -> Option<Box<dyn BlockDevice>> {
    (0..usb_storage::count())
        .filter_map(usb_storage::open)
        .find(|disk| disk.device_id() == id)
        .map(|disk| Box::new(disk) as Box<dyn BlockDevice>)
}

/// Open the partition `role` names, if it can be found and if it carries a
/// filesystem we recognise.
///
/// `None` is an ordinary outcome and never a reason to write anything: no
/// handoff, no device carrying that GUID, two devices carrying it, a device
/// this kernel has no driver for, or a volume that is not FAT32. The caller
/// simply has no mount for that role.
pub fn mount(role: Role) -> Option<FatFs> {
    let volume = role.volume()?;

    let Some(dev) = device_carrying(volume.device) else {
        log!(
            "{role}-volume: the partition is on device {} and no driver here can open it",
            volume.device
        );
        return None;
    };

    let lba = volume.lba_bytes as u64;
    let start = volume.start_lba.checked_mul(lba)?;
    let len = volume.blocks.checked_mul(lba)?;
    let device_bytes = dev.block_count().checked_mul(BLOCK)?;
    if start.checked_add(len)? > device_bytes {
        log!(
            "{role}-volume: the table puts the partition at {start}+{len} on a device of \
             {device_bytes} bytes — refusing to mount past the end of it"
        );
        return None;
    }

    *device(role).lock() = Some(FatDevice {
        dev,
        start,
        len,
        scratch: vec![0u8; BLOCK as usize],
        resident: vec![0u8; RESIDENT_BLOCKS * BLOCK as usize],
        tags: [None; RESIDENT_BLOCKS],
        next_victim: 0,
    });

    // `probe` is a total read and takes no ownership, which is what lets the
    // bound be tightened from the partition to the volume before anything can
    // write. A boot sector describing more than the partition holds is already
    // `Error::Truncated`, so this only ever shrinks.
    let mut volume = FatVolume { role, bytes: len };
    let geom = match Fat32::probe(&mut volume) {
        Ok(geom) => geom,
        Err(e) => {
            log!("{role}-volume: the partition holds no FAT32 this kernel can mount: {e}");
            *device(role).lock() = None;
            return None;
        }
    };
    let volume_bytes = geom.total_sectors as u64 * geom.bytes_per_sector as u64;
    volume.bytes = volume_bytes;
    if let Some(mounted) = device(role).lock().as_mut() {
        mounted.len = volume_bytes;
    }

    match Fat32::mount(volume) {
        Ok(fs) => {
            log!(
                "{role}-volume: partition mounted, {volume_bytes} bytes of a {len}-byte partition \
                 at device offset {start}, {}-byte sectors, {}-byte clusters, {} clusters",
                geom.bytes_per_sector,
                geom.bytes_per_cluster(),
                geom.cluster_count
            );
            Some(FatFs::new(role, fs))
        }
        Err(e) => {
            log!("{role}-volume: the partition holds no FAT32 this kernel can mount: {e}");
            *device(role).lock() = None;
            None
        }
    }
}
