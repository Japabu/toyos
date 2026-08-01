use alloc::string::String;

use crate::device::BlockAccess;
use crate::error::Error;
use crate::fs::Fat32;
use crate::name::{
    self, ShortName, MAX_LFN_ENTRIES, MAX_SHORT_NAME_CANDIDATES, UNITS_PER_LFN_ENTRY,
};
use crate::time::FatTime;

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;
const ATTR_LONG_NAME_MASK: u8 = ATTR_LONG_NAME | ATTR_DIRECTORY | ATTR_ARCHIVE;

const ENTRY_SIZE: u32 = 32;
const FREE: u8 = 0xE5;
const END: u8 = 0x00;

/// Entries this crate will walk in one directory before calling it corrupt.
///
/// 2 MiB of directory data. A policy bound, not a format one: FAT32 puts no
/// limit on a directory's chain, so without this a crafted chain is a scan
/// that runs for as long as the volume is large. It bounds *time* rather than
/// memory — a directory scan allocates nothing — and at roughly three entries
/// per file it still admits some twenty thousand files in one directory, which
/// is more than an ESP or a log directory will hold.
pub const MAX_DIR_ENTRIES: u32 = 65_536;

/// Units a reassembled long name can occupy: 20 entries of 13.
const LFN_UNIT_CAPACITY: usize = MAX_LFN_ENTRIES * UNITS_PER_LFN_ENTRY;

/// One 32-byte directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawEntry(pub [u8; 32]);

impl RawEntry {
    pub fn zeroed() -> RawEntry {
        RawEntry([0u8; 32])
    }

    pub fn short(&self) -> ShortName {
        let mut out = [0u8; 11];
        out.copy_from_slice(&self.0[..11]);
        out
    }

    pub fn set_short(&mut self, short: &ShortName) {
        self.0[..11].copy_from_slice(short);
    }

    pub fn attr(&self) -> u8 {
        self.0[11]
    }

    pub fn set_attr(&mut self, attr: u8) {
        self.0[11] = attr;
    }

    pub fn nt_flags(&self) -> u8 {
        self.0[12]
    }

    pub fn first_cluster(&self) -> u32 {
        let lo = u16::from_le_bytes([self.0[26], self.0[27]]) as u32;
        let hi = u16::from_le_bytes([self.0[20], self.0[21]]) as u32;
        (hi << 16) | lo
    }

    pub fn set_first_cluster(&mut self, cluster: u32) {
        self.0[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        self.0[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    }

    pub fn size(&self) -> u32 {
        u32::from_le_bytes([self.0[28], self.0[29], self.0[30], self.0[31]])
    }

    pub fn set_size(&mut self, size: u32) {
        self.0[28..32].copy_from_slice(&size.to_le_bytes());
    }

    pub fn write_time(&self) -> FatTime {
        FatTime::from_raw(
            u16::from_le_bytes([self.0[24], self.0[25]]),
            u16::from_le_bytes([self.0[22], self.0[23]]),
            0,
        )
    }

    pub fn set_write_time(&mut self, t: FatTime) {
        let (date, time, _) = t.raw();
        self.0[22..24].copy_from_slice(&time.to_le_bytes());
        self.0[24..26].copy_from_slice(&date.to_le_bytes());
        self.0[18..20].copy_from_slice(&date.to_le_bytes());
    }

    pub fn set_create_time(&mut self, t: FatTime) {
        let (date, time, tenths) = t.raw();
        self.0[13] = tenths;
        self.0[14..16].copy_from_slice(&time.to_le_bytes());
        self.0[16..18].copy_from_slice(&date.to_le_bytes());
    }

    pub fn is_dir(&self) -> bool {
        self.attr() & ATTR_DIRECTORY != 0 && self.attr() & ATTR_VOLUME_ID == 0
    }

    pub fn is_volume_label(&self) -> bool {
        !self.is_lfn() && self.attr() & ATTR_VOLUME_ID != 0
    }

    pub fn is_lfn(&self) -> bool {
        self.attr() & ATTR_LONG_NAME_MASK == ATTR_LONG_NAME
    }

    pub fn is_free(&self) -> bool {
        self.0[0] == FREE || self.0[0] == END
    }

    pub fn is_end(&self) -> bool {
        self.0[0] == END
    }

    /// The `.` and `..` entries, which name a directory this crate must never
    /// follow: `..` climbs, and following either turns any tree walk into a
    /// cycle.
    pub fn is_dot(&self) -> bool {
        self.0[0] == b'.'
    }
}

/// A position in a directory's entry stream, remembered so repeated access
/// does not re-walk the cluster chain from the start.
///
/// Advances forward only. Every caller here walks a directory in order, and a
/// cursor that could go backwards would need either a second walk or a cache
/// of the chain, both of which cost more than they save.
pub struct EntryCursor {
    dir_start: u32,
    cluster: u32,
    cluster_index: u32,
}

impl EntryCursor {
    pub fn new(dir_start: u32) -> EntryCursor {
        EntryCursor { dir_start, cluster: dir_start, cluster_index: 0 }
    }

    /// Byte offset of entry `index`, or `None` when the chain ends before it.
    pub fn offset_of<D: BlockAccess>(
        &mut self,
        fs: &mut Fat32<D>,
        index: u32,
    ) -> Result<Option<u64>, Error> {
        let geom = *fs.geometry();
        let per_cluster = geom.bytes_per_cluster() / ENTRY_SIZE;
        let want_cluster = index / per_cluster;

        if want_cluster < self.cluster_index {
            self.cluster = self.dir_start;
            self.cluster_index = 0;
        }
        while self.cluster_index < want_cluster {
            match fs.next_cluster(self.cluster)? {
                Some(next) => {
                    self.cluster = next;
                    self.cluster_index += 1;
                }
                None => return Ok(None),
            }
        }
        let within = (index % per_cluster) as u64 * ENTRY_SIZE as u64;
        Ok(Some(geom.cluster_offset(self.cluster) + within))
    }
}

/// A directory entry found by a scan, together with where its run begins so it
/// can be deleted whole.
pub struct Located {
    pub raw: RawEntry,
    /// Index of the short entry.
    pub index: u32,
    /// Index of the first entry of the run — the first long-name entry, or the
    /// short entry when there is no long name.
    pub first_index: u32,
    /// Length of the reassembled long name in UTF-16 units, or 0 when the
    /// entry has none and its 8.3 name is its name.
    pub long_len: usize,
}

/// A forward scan over one directory's entries, reassembling long names.
///
/// Written as a cursor the caller drives rather than an iterator or a callback
/// because every step needs `&mut Fat32`, and both alternatives would hold
/// that borrow across the caller's own work.
pub struct DirScan {
    cursor: EntryCursor,
    index: u32,
    units: [u16; LFN_UNIT_CAPACITY],
    /// Nominal unit count of the run in progress: `13 * highest ordinal`.
    run_units: usize,
    run_checksum: u8,
    /// Ordinal the next long-name entry must carry, counting down to 1.
    expect_ord: u8,
    run_start: u32,
    run_valid: bool,
    done: bool,
}

impl DirScan {
    pub fn new(dir_start: u32) -> DirScan {
        DirScan {
            cursor: EntryCursor::new(dir_start),
            index: 0,
            units: [0; LFN_UNIT_CAPACITY],
            run_units: 0,
            run_checksum: 0,
            expect_ord: 0,
            run_start: 0,
            run_valid: false,
            done: false,
        }
    }

    fn drop_run(&mut self) {
        self.run_valid = false;
        self.run_units = 0;
        self.expect_ord = 0;
    }

    /// The next real entry, skipping free slots, long-name runs, volume
    /// labels and the dot entries.
    pub fn next<D: BlockAccess>(&mut self, fs: &mut Fat32<D>) -> Result<Option<Located>, Error> {
        if self.done {
            return Ok(None);
        }
        loop {
            if self.index >= MAX_DIR_ENTRIES {
                self.done = true;
                return Err(Error::CorruptDirectory);
            }
            let Some(offset) = self.cursor.offset_of(fs, self.index)? else {
                self.done = true;
                return Ok(None);
            };
            let raw = fs.read_entry_at(offset)?;
            let index = self.index;
            self.index += 1;

            if raw.is_end() {
                self.done = true;
                return Ok(None);
            }
            if raw.0[0] == FREE {
                self.drop_run();
                continue;
            }
            if raw.is_lfn() {
                self.absorb_lfn(&raw, index);
                continue;
            }
            if raw.is_volume_label() || raw.is_dot() {
                self.drop_run();
                continue;
            }

            // A run is this entry's name only if it is complete and its
            // checksum ties it to this short name. Anything else is the
            // wreckage of a deleted file whose long entries were not all
            // overwritten, and the short name is still authoritative.
            let long_len = if self.run_valid
                && self.expect_ord == 0
                && self.run_checksum == name::lfn_checksum(&raw.short())
            {
                self.run_length()
            } else {
                0
            };
            let first_index = if long_len > 0 { self.run_start } else { index };
            self.drop_run();
            return Ok(Some(Located { raw, index, first_index, long_len }));
        }
    }

    fn absorb_lfn(&mut self, raw: &RawEntry, index: u32) {
        let ord = raw.0[0];
        let seq = (ord & 0x3F) as usize;
        if seq == 0 || seq > MAX_LFN_ENTRIES {
            self.drop_run();
            return;
        }
        if ord & 0x40 != 0 {
            self.run_units = seq * UNITS_PER_LFN_ENTRY;
            self.run_checksum = raw.0[13];
            self.run_start = index;
            self.run_valid = true;
            self.units = [0xFFFF; LFN_UNIT_CAPACITY];
        } else if !self.run_valid || seq as u8 != self.expect_ord || raw.0[13] != self.run_checksum {
            self.drop_run();
            return;
        }
        // The 13 units of an entry are split across three non-adjacent runs of
        // its 32 bytes, which is the format's doing and not worth hiding.
        let base = (seq - 1) * UNITS_PER_LFN_ENTRY;
        let mut n = 0;
        for &(off, count) in &[(1usize, 5usize), (14, 6), (28, 2)] {
            for i in 0..count {
                let at = off + i * 2;
                let unit = u16::from_le_bytes([raw.0[at], raw.0[at + 1]]);
                if let Some(slot) = self.units.get_mut(base + n) {
                    *slot = unit;
                }
                n += 1;
            }
        }
        self.expect_ord = (seq - 1) as u8;
    }

    fn run_length(&self) -> usize {
        let nominal = self.run_units.min(LFN_UNIT_CAPACITY);
        for i in 0..nominal {
            match self.units.get(i) {
                Some(&u) if u == 0x0000 || u == 0xFFFF => return i,
                Some(_) => {}
                None => return i,
            }
        }
        nominal
    }

    /// The long name of the entry `next` just returned.
    pub fn units(&self, loc: &Located) -> &[u16] {
        self.units.get(..loc.long_len).unwrap_or(&[])
    }

    pub fn name_eq(&self, loc: &Located, query: &str) -> bool {
        if loc.long_len > 0 {
            name::long_name_eq(self.units(loc), query)
        } else {
            name::short_name_eq(&loc.raw.short(), query)
        }
    }

    pub fn name_string(&self, loc: &Located) -> String {
        if loc.long_len > 0 {
            name::units_to_string(self.units(loc))
        } else {
            name::short_name_to_string(&loc.raw.short(), loc.raw.nt_flags())
        }
    }
}

impl<D: BlockAccess> Fat32<D> {
    pub(crate) fn read_entry_at(&mut self, offset: u64) -> Result<RawEntry, Error> {
        let bps = self.geometry().bytes_per_sector as u64;
        let sector = offset / bps * bps;
        let within = (offset - sector) as usize;
        self.load_sector(sector)?;
        let bytes = self
            .sector()
            .get(within..within + ENTRY_SIZE as usize)
            .ok_or(Error::CorruptDirectory)?;
        let mut raw = [0u8; 32];
        raw.copy_from_slice(bytes);
        Ok(RawEntry(raw))
    }

    pub(crate) fn write_entry_at(&mut self, offset: u64, entry: &RawEntry) -> Result<(), Error> {
        self.invalidate_sector();
        self.device().write_at(offset, &entry.0)?;
        Ok(())
    }

    /// Entries the directory's currently allocated clusters can hold.
    fn dir_capacity(&mut self, dir_start: u32) -> Result<u32, Error> {
        let per_cluster = self.geometry().bytes_per_cluster() / ENTRY_SIZE;
        let max_clusters = (MAX_DIR_ENTRIES / per_cluster).max(1) as u64;
        let clusters = self.chain_len(dir_start, max_clusters)?;
        Ok((clusters as u32).saturating_mul(per_cluster).min(MAX_DIR_ENTRIES))
    }

    /// The index of the first run of `count` consecutive free slots, growing
    /// the directory by a cluster if there is no such run.
    fn find_free_run(&mut self, dir_start: u32, count: u32) -> Result<u32, Error> {
        let mut capacity = self.dir_capacity(dir_start)?;
        loop {
            if let Some(index) = self.scan_free_run(dir_start, count, capacity)? {
                return Ok(index);
            }
            let per_cluster = self.geometry().bytes_per_cluster() / ENTRY_SIZE;
            if capacity.saturating_add(per_cluster) > MAX_DIR_ENTRIES {
                return Err(Error::NoSpace);
            }
            let max_clusters = (MAX_DIR_ENTRIES / per_cluster).max(1) as u64;
            let last = self.chain_last(dir_start, max_clusters)?;
            let new = self.alloc_zeroed_cluster()?;
            self.set_fat_entry(last, new)?;
            capacity += per_cluster;
        }
    }

    fn scan_free_run(
        &mut self,
        dir_start: u32,
        count: u32,
        capacity: u32,
    ) -> Result<Option<u32>, Error> {
        let mut cursor = EntryCursor::new(dir_start);
        let mut run_start: Option<u32> = None;
        let mut index = 0;
        while index < capacity {
            let Some(offset) = cursor.offset_of(self, index)? else { break };
            let raw = self.read_entry_at(offset)?;
            if raw.is_free() {
                let start = *run_start.get_or_insert(index);
                if raw.is_end() {
                    // Every slot from here to the end of the allocated
                    // clusters is free: an end marker means nothing follows.
                    return Ok(if capacity - start >= count { Some(start) } else { None });
                }
                if index + 1 - start >= count {
                    return Ok(Some(start));
                }
            } else {
                run_start = None;
            }
            index += 1;
        }
        Ok(None)
    }

    fn short_name_taken(&mut self, dir_start: u32, short: &ShortName) -> Result<bool, Error> {
        let mut scan = DirScan::new(dir_start);
        while let Some(loc) = scan.next(self)? {
            if &loc.raw.short() == short {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Create a directory entry for `name`, carrying the cluster, size and
    /// attributes of `template`. Returns the index of its short entry.
    pub(crate) fn insert_entry(
        &mut self,
        dir_start: u32,
        name: &str,
        template: &RawEntry,
    ) -> Result<u32, Error> {
        name::validate_component(name)?;
        let basis = name::basis_name(name);
        let short_only = name::fits_short(name, &basis);

        let mut short = basis.short;
        let mut found = false;
        for n in 0..MAX_SHORT_NAME_CANDIDATES {
            let candidate = name::candidate(&basis, name, n);
            if !self.short_name_taken(dir_start, &candidate)? {
                short = candidate;
                found = true;
                break;
            }
        }
        if !found {
            return Err(Error::NoSpace);
        }

        let (groups, units) = if short_only {
            (0, [[0u16; UNITS_PER_LFN_ENTRY]; MAX_LFN_ENTRIES])
        } else {
            name::lfn_groups(name)?
        };
        let total = groups as u32 + 1;
        let start = self.find_free_run(dir_start, total)?;

        let checksum = name::lfn_checksum(&short);
        let mut cursor = EntryCursor::new(dir_start);
        for g in (0..groups).rev() {
            let mut raw = RawEntry::zeroed();
            let ord = (g + 1) as u8;
            raw.0[0] = if g + 1 == groups { ord | 0x40 } else { ord };
            raw.0[11] = ATTR_LONG_NAME;
            raw.0[13] = checksum;
            let mut n = 0;
            for &(off, count) in &[(1usize, 5usize), (14, 6), (28, 2)] {
                for i in 0..count {
                    let at = off + i * 2;
                    raw.0[at..at + 2].copy_from_slice(&units[g][n].to_le_bytes());
                    n += 1;
                }
            }
            let index = start + (groups - 1 - g) as u32;
            let offset = cursor.offset_of(self, index)?.ok_or(Error::NoSpace)?;
            self.write_entry_at(offset, &raw)?;
        }

        let mut entry = *template;
        entry.set_short(&short);
        entry.0[12] = 0;
        let index = start + groups as u32;
        let offset = cursor.offset_of(self, index)?.ok_or(Error::NoSpace)?;
        self.write_entry_at(offset, &entry)?;
        Ok(index)
    }

    /// Mark every entry of a run free. Does not touch the cluster chain — a
    /// rename moves an entry and must not free what it still points at.
    pub(crate) fn erase_entries(
        &mut self,
        dir_start: u32,
        first_index: u32,
        last_index: u32,
    ) -> Result<(), Error> {
        let mut cursor = EntryCursor::new(dir_start);
        for index in first_index..=last_index {
            let Some(offset) = cursor.offset_of(self, index)? else { break };
            let mut raw = self.read_entry_at(offset)?;
            raw.0[0] = FREE;
            self.write_entry_at(offset, &raw)?;
        }
        Ok(())
    }

    /// Whether a directory holds nothing but `.`, `..` and free slots.
    pub(crate) fn dir_is_empty(&mut self, dir_start: u32) -> Result<bool, Error> {
        let mut scan = DirScan::new(dir_start);
        Ok(scan.next(self)?.is_none())
    }

    /// Write the `.` and `..` pair a new directory must begin with.
    ///
    /// `parent` is 0 when the parent is the root, which is what the format
    /// requires and what `fsck` checks: the root has no cluster number a `..`
    /// entry may name.
    pub(crate) fn init_dot_entries(
        &mut self,
        cluster: u32,
        parent: u32,
        time: FatTime,
    ) -> Result<(), Error> {
        let root = self.geometry().root_cluster;
        let parent = if parent == root { 0 } else { parent };
        let mut cursor = EntryCursor::new(cluster);
        for (index, (short, target)) in
            [(b".          ", cluster), (b"..         ", parent)].into_iter().enumerate()
        {
            let mut raw = RawEntry::zeroed();
            raw.set_short(short);
            raw.set_attr(ATTR_DIRECTORY);
            raw.set_first_cluster(target);
            raw.set_create_time(time);
            raw.set_write_time(time);
            let offset = cursor.offset_of(self, index as u32)?.ok_or(Error::NoSpace)?;
            self.write_entry_at(offset, &raw)?;
        }
        Ok(())
    }

    /// Repoint a directory's `..` entry after it moves to a new parent.
    pub(crate) fn set_dot_dot(&mut self, cluster: u32, parent: u32) -> Result<(), Error> {
        let root = self.geometry().root_cluster;
        let parent = if parent == root { 0 } else { parent };
        let mut cursor = EntryCursor::new(cluster);
        let Some(offset) = cursor.offset_of(self, 1)? else { return Err(Error::CorruptDirectory) };
        let mut raw = self.read_entry_at(offset)?;
        if raw.0[..2] != *b".." {
            return Err(Error::CorruptDirectory);
        }
        raw.set_first_cluster(parent);
        self.write_entry_at(offset, &raw)
    }
}
