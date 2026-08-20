//! A FAT32 volume built here, byte by byte, from the specification.
//!
//! The mutation tests need a volume they can break in exactly one place, and
//! they need to know where that place is. Nothing in this repository can hand
//! them one: `newfs_msdos` is a macOS binary, which is the dependency this
//! crate exists to remove, and `toyos-fat32` is the writer under judgement.
//! So the fixture is written from fatgen103 the same way the checker reads it,
//! and its being clean is asserted first — a mutation from a volume the checker
//! already complains about proves nothing.
//!
//! The layout is the smallest one FAT32 admits: 512-byte sectors, one sector to
//! a cluster, and 65,600 clusters, which is 75 above the 65,525 that separates
//! FAT32 from FAT16. 34 MB of mostly zeroes, allocated per test.

#![allow(dead_code)]

use std::collections::BTreeMap;

pub const BYTES_PER_SECTOR: usize = 512;
pub const SECTORS_PER_CLUSTER: usize = 1;
pub const RESERVED_SECTORS: usize = 32;
pub const NUM_FATS: usize = 2;
/// Just above the 65,525 that makes a volume FAT32 rather than FAT16.
pub const CLUSTERS: u32 = 65_600;
/// `(CLUSTERS + 2) * 4` bytes of entries, rounded up to whole sectors.
pub const FAT_SECTORS: usize = 513;
pub const TOTAL_SECTORS: usize =
    RESERVED_SECTORS + NUM_FATS * FAT_SECTORS + CLUSTERS as usize * SECTORS_PER_CLUSTER;
pub const VOLUME_BYTES: usize = TOTAL_SECTORS * BYTES_PER_SECTOR;
pub const BYTES_PER_CLUSTER: usize = BYTES_PER_SECTOR * SECTORS_PER_CLUSTER;
pub const ROOT_CLUSTER: u32 = 2;
pub const FIRST_DATA_SECTOR: usize = RESERVED_SECTORS + NUM_FATS * FAT_SECTORS;

/// Where the boot sector keeps each field (fatgen103 §3.1, §3.2).
pub const BPB_BYTS_PER_SEC: usize = 11;
pub const BPB_SEC_PER_CLUS: usize = 13;
pub const BPB_RSVD_SEC_CNT: usize = 14;
pub const BPB_NUM_FATS: usize = 16;
pub const BPB_ROOT_ENT_CNT: usize = 17;
pub const BPB_TOT_SEC_16: usize = 19;
pub const BPB_MEDIA: usize = 21;
pub const BPB_FAT_SZ_16: usize = 22;
pub const BPB_TOT_SEC_32: usize = 32;
pub const BPB_FAT_SZ_32: usize = 36;
pub const BPB_EXT_FLAGS: usize = 40;
pub const BPB_FS_VER: usize = 42;
pub const BPB_ROOT_CLUS: usize = 44;
pub const BPB_FS_INFO: usize = 48;
pub const BOOT_SIGNATURE: usize = 510;

/// Where the FSInfo sector keeps each field (fatgen103 §5).
pub const FSINFO_AT: usize = BYTES_PER_SECTOR;
pub const FSI_LEAD_SIG: usize = 0;
pub const FSI_STRUC_SIG: usize = 484;
pub const FSI_FREE_COUNT: usize = 488;
pub const FSI_NXT_FREE: usize = 492;
pub const FSI_TRAIL_SIG: usize = 508;

/// Directory entry field offsets (fatgen103 §6).
pub const DIR_ATTR: usize = 11;
pub const DIR_NT_RES: usize = 12;
pub const DIR_FST_CLUS_HI: usize = 20;
pub const DIR_FST_CLUS_LO: usize = 26;
pub const DIR_FILE_SIZE: usize = 28;
pub const LDIR_ORD: usize = 0;
pub const LDIR_TYPE: usize = 12;
pub const LDIR_CHKSUM: usize = 13;
pub const LDIR_FST_CLUS_LO: usize = 26;

pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LONG_NAME: u8 = 0x0F;
pub const LAST_LONG_ENTRY: u8 = 0x40;

pub const EOC: u32 = 0x0FFF_FFFF;
pub const BAD_CLUSTER: u32 = 0x0FFF_FFF7;

pub fn fat_offset(copy: usize, entry: u32) -> usize {
    (RESERVED_SECTORS + copy * FAT_SECTORS) * BYTES_PER_SECTOR + entry as usize * 4
}

pub fn cluster_offset(cluster: u32) -> usize {
    FIRST_DATA_SECTOR * BYTES_PER_SECTOR + (cluster as usize - 2) * BYTES_PER_CLUSTER
}

/// Where a name's entries ended up, so a test can aim at one byte of one.
pub struct Placed {
    /// The directory's first cluster.
    pub dir: u32,
    /// Byte offset of the short entry.
    pub entry: usize,
    /// Byte offsets of its long-name entries, in the order they sit on disk,
    /// which is the reverse of the order they spell.
    pub longs: Vec<usize>,
    pub first: u32,
    pub size: u32,
}

pub struct Volume {
    pub bytes: Vec<u8>,
    next_cluster: u32,
    placed: BTreeMap<String, Placed>,
}

/// fatgen103 §7.2: the short name's eleven bytes, rotated right and summed.
pub fn checksum(name: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &c in name {
        sum = sum.rotate_right(1);
        sum = sum.wrapping_add(c);
    }
    sum
}

fn short_entry(name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[..11].copy_from_slice(name);
    e[DIR_ATTR] = attr;
    // 2024-06-01 12:34:56 in the format's packed fields, so an entry carries a
    // date rather than the epoch a zeroed one would read as.
    let date = ((2024 - 1980) << 9) | (6 << 5) | 1;
    let time = (12 << 11) | (34 << 5) | 28;
    e[14..16].copy_from_slice(&(time as u16).to_le_bytes());
    e[16..18].copy_from_slice(&(date as u16).to_le_bytes());
    e[18..20].copy_from_slice(&(date as u16).to_le_bytes());
    e[22..24].copy_from_slice(&(time as u16).to_le_bytes());
    e[24..26].copy_from_slice(&(date as u16).to_le_bytes());
    e[DIR_FST_CLUS_HI..DIR_FST_CLUS_HI + 2].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[DIR_FST_CLUS_LO..DIR_FST_CLUS_LO + 2].copy_from_slice(&(cluster as u16).to_le_bytes());
    e[DIR_FILE_SIZE..DIR_FILE_SIZE + 4].copy_from_slice(&size.to_le_bytes());
    e
}

/// The long-name entries for `name`, in the order the format puts them on disk:
/// the last thirteen characters first, carrying `LAST_LONG_ENTRY`, counting
/// down to ordinal 1 immediately before the short entry.
pub fn long_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; 32]> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let groups = units.len().div_ceil(13);
    let sum = checksum(short);
    let mut out = Vec::new();
    for group in (0..groups).rev() {
        let mut e = [0u8; 32];
        e[LDIR_ORD] = (group as u8 + 1) | if group + 1 == groups { LAST_LONG_ENTRY } else { 0 };
        e[DIR_ATTR] = ATTR_LONG_NAME;
        e[LDIR_CHKSUM] = sum;
        let mut slots = [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].into_iter();
        for i in 0..13 {
            let at = slots.next().expect("thirteen slots");
            let index = group * 13 + i;
            let unit = match index.cmp(&units.len()) {
                std::cmp::Ordering::Less => units[index],
                std::cmp::Ordering::Equal => 0x0000,
                std::cmp::Ordering::Greater => 0xFFFF,
            };
            e[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        out.push(e);
    }
    out
}

impl Volume {
    pub fn new() -> Volume {
        let mut v =
            Volume { bytes: vec![0u8; VOLUME_BYTES], next_cluster: 4, placed: BTreeMap::new() };
        v.write_boot_sector(0);
        v.write_boot_sector(6 * BYTES_PER_SECTOR);
        v.write_fs_info(FSINFO_AT);
        v.write_fs_info(7 * BYTES_PER_SECTOR);
        v.set_fat(0, 0x0FFF_FF00 | 0xF8);
        v.set_fat(1, EOC);
        // The root gets two clusters, so the walk crosses a directory chain
        // link on the clean fixture rather than only on a mutated one.
        v.set_fat(2, 3);
        v.set_fat(3, EOC);
        v
    }

    fn write_boot_sector(&mut self, at: usize) {
        let s = &mut self.bytes[at..at + BYTES_PER_SECTOR];
        s[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        s[3..11].copy_from_slice(b"TOYOSCHK");
        s[BPB_BYTS_PER_SEC..][..2].copy_from_slice(&(BYTES_PER_SECTOR as u16).to_le_bytes());
        s[BPB_SEC_PER_CLUS] = SECTORS_PER_CLUSTER as u8;
        s[BPB_RSVD_SEC_CNT..][..2].copy_from_slice(&(RESERVED_SECTORS as u16).to_le_bytes());
        s[BPB_NUM_FATS] = NUM_FATS as u8;
        s[BPB_MEDIA] = 0xF8;
        s[24..26].copy_from_slice(&63u16.to_le_bytes());
        s[26..28].copy_from_slice(&255u16.to_le_bytes());
        s[BPB_TOT_SEC_32..][..4].copy_from_slice(&(TOTAL_SECTORS as u32).to_le_bytes());
        s[BPB_FAT_SZ_32..][..4].copy_from_slice(&(FAT_SECTORS as u32).to_le_bytes());
        s[BPB_ROOT_CLUS..][..4].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
        s[BPB_FS_INFO..][..2].copy_from_slice(&1u16.to_le_bytes());
        s[50..52].copy_from_slice(&6u16.to_le_bytes());
        s[64] = 0x80;
        s[66] = 0x29;
        s[67..71].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        s[71..82].copy_from_slice(b"TOYOSCHK   ");
        s[82..90].copy_from_slice(b"FAT32   ");
        s[BOOT_SIGNATURE..][..2].copy_from_slice(&0xAA55u16.to_le_bytes());
    }

    fn write_fs_info(&mut self, at: usize) {
        let s = &mut self.bytes[at..at + BYTES_PER_SECTOR];
        s[FSI_LEAD_SIG..][..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        s[FSI_STRUC_SIG..][..4].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        s[FSI_FREE_COUNT..][..4].copy_from_slice(&u32::MAX.to_le_bytes());
        s[FSI_NXT_FREE..][..4].copy_from_slice(&u32::MAX.to_le_bytes());
        s[FSI_TRAIL_SIG..][..4].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    }

    pub fn set_fat(&mut self, entry: u32, value: u32) {
        let at = fat_offset(0, entry);
        self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    pub fn fat(&self, entry: u32) -> u32 {
        let at = fat_offset(0, entry);
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("four bytes"))
    }

    /// `n` clusters, linked into one chain ending in an end-of-chain mark.
    pub fn alloc(&mut self, n: usize) -> Vec<u32> {
        let chain: Vec<u32> = (0..n).map(|i| self.next_cluster + i as u32).collect();
        self.next_cluster += n as u32;
        for (i, &c) in chain.iter().enumerate() {
            let next = chain.get(i + 1).copied().unwrap_or(EOC);
            self.set_fat(c, next);
            let at = cluster_offset(c);
            self.bytes[at..at + BYTES_PER_CLUSTER].fill(0);
        }
        chain
    }

    /// The offset of `count` consecutive unused entries in `dir`'s chain.
    fn room(&self, dir: u32, count: usize) -> usize {
        let mut cluster = dir;
        loop {
            let base = cluster_offset(cluster);
            let entries = BYTES_PER_CLUSTER / 32;
            for start in 0..=entries.saturating_sub(count) {
                if (0..count).all(|i| self.bytes[base + (start + i) * 32] == 0x00) {
                    return base + start * 32;
                }
            }
            let next = self.fat(cluster);
            assert!((2..CLUSTERS + 2).contains(&next), "the fixture's directory is full");
            cluster = next;
        }
    }

    fn place(&mut self, dir: u32, key: &str, entries: &[[u8; 32]], first: u32, size: u32) {
        let at = self.room(dir, entries.len());
        for (i, e) in entries.iter().enumerate() {
            self.bytes[at + i * 32..at + (i + 1) * 32].copy_from_slice(e);
        }
        let longs = (0..entries.len() - 1).map(|i| at + i * 32).collect();
        let entry = at + (entries.len() - 1) * 32;
        self.placed.insert(key.to_string(), Placed { dir, entry, longs, first, size });
    }

    pub fn add_file(&mut self, dir: u32, key: &str, short: &[u8; 11], long: Option<&str>, size: u32) {
        let needed = (size as usize).div_ceil(BYTES_PER_CLUSTER);
        let chain = self.alloc(needed);
        let first = chain.first().copied().unwrap_or(0);
        let mut entries = long.map(|l| long_entries(l, short)).unwrap_or_default();
        entries.push(short_entry(short, ATTR_ARCHIVE, first, size));
        self.place(dir, key, &entries, first, size);
    }

    /// A subdirectory, with the `.` and `..` the format requires of one.
    pub fn add_dir(&mut self, parent: u32, key: &str, short: &[u8; 11], parent_link: u32) -> u32 {
        let cluster = self.alloc(1)[0];
        let dot = short_entry(b".          ", ATTR_DIRECTORY, cluster, 0);
        let dot_dot = short_entry(b"..         ", ATTR_DIRECTORY, parent_link, 0);
        let at = cluster_offset(cluster);
        self.bytes[at..at + 32].copy_from_slice(&dot);
        self.bytes[at + 32..at + 64].copy_from_slice(&dot_dot);
        self.place(parent, key, &[short_entry(short, ATTR_DIRECTORY, cluster, 0)], cluster, 0);
        cluster
    }

    pub fn add_label(&mut self, dir: u32, key: &str, label: &[u8; 11]) {
        self.place(dir, key, &[short_entry(label, ATTR_VOLUME_ID, 0, 0)], 0, 0);
    }

    /// Mirror FAT 0 into every copy and record the free count, which is what a
    /// driver does at unmount and what makes the volume clean.
    pub fn finish(&mut self) {
        let used = FAT_SECTORS * BYTES_PER_SECTOR;
        let from = fat_offset(0, 0);
        for copy in 1..NUM_FATS {
            let to = fat_offset(copy, 0);
            self.bytes.copy_within(from..from + used, to);
        }
        let free = (2..CLUSTERS + 2).filter(|&c| self.fat(c) == 0).count() as u32;
        for at in [FSINFO_AT, 7 * BYTES_PER_SECTOR] {
            self.bytes[at + FSI_FREE_COUNT..][..4].copy_from_slice(&free.to_le_bytes());
            self.bytes[at + FSI_NXT_FREE..][..4].copy_from_slice(&self.next_cluster.to_le_bytes());
        }
    }

    pub fn at(&self, key: &str) -> &Placed {
        self.placed.get(key).unwrap_or_else(|| panic!("{key} is not on the fixture"))
    }

    pub fn poke(&mut self, at: usize, bytes: &[u8]) {
        self.bytes[at..at + bytes.len()].copy_from_slice(bytes);
    }

    pub fn poke_u16(&mut self, at: usize, value: u16) {
        self.poke(at, &value.to_le_bytes());
    }

    pub fn poke_u32(&mut self, at: usize, value: u32) {
        self.poke(at, &value.to_le_bytes());
    }
}

/// The volume every mutation starts from: a label, a short-named file, a
/// long-named file spanning several clusters, and two levels of subdirectory.
pub fn fixture() -> Volume {
    let mut v = Volume::new();
    v.add_label(ROOT_CLUSTER, "label", b"TOYOSCHK   ");
    v.add_file(ROOT_CLUSTER, "short.txt", b"SHORT   TXT", None, 100);
    v.add_file(
        ROOT_CLUSTER,
        "long.bin",
        b"ALONGN~1BIN",
        Some("A Long Name For Entries.bin"),
        3000,
    );
    let sub = v.add_dir(ROOT_CLUSTER, "sub", b"SUB        ", 0);
    v.add_file(sub, "sub/inner.dat", b"INNER   DAT", None, 600);
    let deeper = v.add_dir(sub, "sub/deeper", b"DEEPER     ", sub);
    v.add_file(deeper, "sub/deeper/leaf.txt", b"LEAF    TXT", None, 4);
    v.finish();
    v
}
