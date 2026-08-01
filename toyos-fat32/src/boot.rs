use crate::device::BlockAccess;
use crate::error::Error;

/// Below this cluster count the volume is FAT12 or FAT16, whatever its boot
/// sector claims. The FAT specification defines the type by the cluster count
/// and nothing else, so accepting a smaller volume as FAT32 would mean reading
/// 16-bit FAT entries as 32-bit ones and getting plausible garbage.
pub const MIN_FAT32_CLUSTERS: u32 = 65_525;

/// Above this, a cluster number collides with the reserved end-of-chain and
/// bad-cluster values. Validating it here is what lets [`next_cluster`] treat
/// "out of range" and "reserved marker" as one check.
///
/// [`next_cluster`]: crate::Fat32
const MAX_FAT32_CLUSTERS: u32 = 0x0FFF_FFF4;

/// The only sector sizes a FAT volume may declare.
const LEGAL_SECTOR_SIZES: [u32; 4] = [512, 1024, 2048, 4096];

/// The most FATs this crate will mirror a write to. The specification allows
/// any number; every volume in existence has one or two. The bound exists
/// because `num_fats` multiplies the FAT region's size, and an absurd value
/// would otherwise be caught only by the arithmetic it overflows.
const MAX_NUM_FATS: u32 = 8;

/// A cluster number that has been checked against the volume it came from.
///
/// [`Geometry::cluster`] is the only way to make one, and the field is private
/// to this module, so a number that came off the stick cannot become a byte
/// offset without passing the check. That is the whole point: the check
/// existed before, at nine sites, and the one place it was written with a
/// condition in front of it — a directory entry's cluster was validated only
/// when the entry was a directory or had a non-zero size — was a write of 256
/// GiB outside the volume, reported as success. A precondition in a doc
/// comment is a precondition nobody enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cluster(u32);

impl Cluster {
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Everything about the volume's layout, derived once and then trusted.
///
/// The point of computing this at mount is that afterwards nothing has to
/// re-validate: every field here has been checked against every other, so the
/// byte offsets this struct computes are inside the volume by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    /// Sectors in one FAT.
    pub fat_sectors: u32,
    pub total_sectors: u32,
    pub root_cluster: u32,
    /// Sector holding the FSInfo structure, if the volume declares one.
    pub fsinfo_sector: Option<u32>,
    /// `Some(i)` when the volume disabled mirroring and only FAT `i` is live;
    /// `None` when writes go to every FAT.
    pub active_fat: Option<u32>,
    /// Data clusters, numbered 2 ..= `cluster_count + 1`.
    pub cluster_count: u32,
    pub first_data_sector: u32,
}

fn u16_at(buf: &[u8], off: usize) -> u16 {
    match buf.get(off..off + 2) {
        Some(&[a, b]) => u16::from_le_bytes([a, b]),
        _ => 0,
    }
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    match buf.get(off..off + 4) {
        Some(&[a, b, c, d]) => u32::from_le_bytes([a, b, c, d]),
        _ => 0,
    }
}

impl Geometry {
    /// Parse and validate a boot sector.
    ///
    /// `capacity` is the device's size, checked against the volume the boot
    /// sector describes. Every other check is internal: a field outside its
    /// legal set, or a combination that does not add up, is
    /// [`Error::NotFat32`]. Nothing here can panic on any 512 bytes.
    pub fn parse(buf: &[u8], capacity: u64) -> Result<Geometry, Error> {
        if buf.len() < 512 {
            return Err(Error::NotFat32);
        }
        if u16_at(buf, 510) != 0xAA55 {
            return Err(Error::NotFat32);
        }

        let bytes_per_sector = u16_at(buf, 11) as u32;
        if !LEGAL_SECTOR_SIZES.contains(&bytes_per_sector) {
            return Err(Error::NotFat32);
        }

        let sectors_per_cluster = buf.get(13).copied().unwrap_or(0) as u32;
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() || sectors_per_cluster > 128 {
            return Err(Error::NotFat32);
        }

        let reserved_sectors = u16_at(buf, 14) as u32;
        if reserved_sectors == 0 {
            return Err(Error::NotFat32);
        }

        let num_fats = buf.get(16).copied().unwrap_or(0) as u32;
        if num_fats == 0 || num_fats > MAX_NUM_FATS {
            return Err(Error::NotFat32);
        }

        // The three fields that are zero exactly when the volume is FAT32.
        // A non-zero root entry count means a FAT12/FAT16 fixed root
        // directory, which lives where this crate expects data clusters.
        if u16_at(buf, 17) != 0 || u16_at(buf, 19) != 0 || u16_at(buf, 22) != 0 {
            return Err(Error::NotFat32);
        }

        let total_sectors = u32_at(buf, 32);
        let fat_sectors = u32_at(buf, 36);
        if total_sectors == 0 || fat_sectors == 0 {
            return Err(Error::NotFat32);
        }

        if u16_at(buf, 42) != 0 {
            return Err(Error::NotFat32);
        }

        let ext_flags = u16_at(buf, 40);
        let active_fat = if ext_flags & 0x0080 != 0 {
            let idx = (ext_flags & 0x000F) as u32;
            if idx >= num_fats {
                return Err(Error::NotFat32);
            }
            Some(idx)
        } else {
            None
        };

        // u64 throughout: `reserved_sectors` and `fat_sectors` are each at
        // most u32::MAX and `num_fats` at most 8, so the sum is bounded by
        // 2^35 and cannot wrap. Its comparison against `total_sectors` is what
        // keeps every later sector number inside a u32.
        let fat_region = fat_sectors as u64 * num_fats as u64;
        let first_data_sector = reserved_sectors as u64 + fat_region;
        if first_data_sector >= total_sectors as u64 {
            return Err(Error::NotFat32);
        }
        let data_sectors = total_sectors as u64 - first_data_sector;
        let cluster_count = data_sectors / sectors_per_cluster as u64;

        if cluster_count < MIN_FAT32_CLUSTERS as u64 || cluster_count > MAX_FAT32_CLUSTERS as u64 {
            return Err(Error::NotFat32);
        }
        let cluster_count = cluster_count as u32;

        // The FAT must have an entry for every cluster it describes, plus the
        // two reserved ones. Without this a volume could claim more clusters
        // than its FAT can address, and a chain walk would read whatever lies
        // past the FAT region as if it were a cluster number.
        let fat_entries = fat_sectors as u64 * bytes_per_sector as u64 / 4;
        if fat_entries < cluster_count as u64 + 2 {
            return Err(Error::NotFat32);
        }

        let root_cluster = u32_at(buf, 44);
        if root_cluster < 2 || root_cluster > cluster_count + 1 {
            return Err(Error::NotFat32);
        }

        let fsinfo_raw = u16_at(buf, 48);
        let fsinfo_sector = if fsinfo_raw != 0 && (fsinfo_raw as u32) < reserved_sectors {
            Some(fsinfo_raw as u32)
        } else {
            None
        };

        let volume_bytes = total_sectors as u64 * bytes_per_sector as u64;
        if volume_bytes > capacity {
            return Err(Error::Truncated);
        }

        Ok(Geometry {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            fat_sectors,
            total_sectors,
            root_cluster,
            fsinfo_sector,
            active_fat,
            cluster_count,
            first_data_sector: first_data_sector as u32,
        })
    }

    pub fn bytes_per_cluster(&self) -> u32 {
        self.bytes_per_sector * self.sectors_per_cluster
    }

    /// The largest legal cluster number. Anything above it in a FAT entry is a
    /// reserved marker or corruption, and this one comparison separates both
    /// from a real link.
    pub fn max_cluster(&self) -> u32 {
        self.cluster_count + 1
    }

    pub fn sector_offset(&self, sector: u32) -> u64 {
        sector as u64 * self.bytes_per_sector as u64
    }

    /// The only constructor for a [`Cluster`]. `None` for anything that is not
    /// a data cluster of this volume — including 0, which a directory entry
    /// uses to mean "no data at all".
    pub fn cluster(&self, raw: u32) -> Option<Cluster> {
        (raw >= 2 && raw <= self.max_cluster()).then_some(Cluster(raw))
    }

    /// The root directory's first cluster. Always valid: [`Self::parse`]
    /// refuses a volume whose root cluster is outside it.
    pub fn root(&self) -> Cluster {
        Cluster(self.root_cluster)
    }

    pub fn cluster_offset(&self, cluster: Cluster) -> u64 {
        let sector = self.first_data_sector as u64
            + (cluster.0 as u64 - 2) * self.sectors_per_cluster as u64;
        sector * self.bytes_per_sector as u64
    }

    pub fn fat_entry_offset(&self, fat: u32, cluster: Cluster) -> u64 {
        self.fat_base_offset(fat) + cluster.0 as u64 * 4
    }

    /// Where FAT number `fat` starts. The scanning loops work in whole FAT
    /// sectors and have no cluster to offset from.
    pub fn fat_base_offset(&self, fat: u32) -> u64 {
        let fat_start = self.reserved_sectors as u64 + fat as u64 * self.fat_sectors as u64;
        fat_start * self.bytes_per_sector as u64
    }

    /// Which FATs a write must reach. One when the volume disabled mirroring,
    /// otherwise all of them — a stale mirror is a volume that reads
    /// differently after a firmware update decides to use the other copy.
    pub fn fat_mirrors(&self) -> core::ops::Range<u32> {
        match self.active_fat {
            Some(i) => i..i + 1,
            None => 0..self.num_fats,
        }
    }
}

/// FSInfo's free-cluster hints.
///
/// Hints, not truth: the specification says so, and this crate treats them
/// that way. Allocation scans the FAT and never consults `free_count`, so a
/// volume claiming a billion free clusters cannot make an allocation succeed
/// that should have failed. `next_free` only chooses where the scan starts,
/// and is range-checked before use.
#[derive(Debug, Clone, Copy)]
pub struct FsInfo {
    pub free_count: Option<u32>,
    pub next_free: Option<Cluster>,
    pub dirty: bool,
}

impl FsInfo {
    pub const UNKNOWN: FsInfo = FsInfo { free_count: None, next_free: None, dirty: false };

    /// Read FSInfo, or return [`Self::UNKNOWN`] if it is absent or malformed.
    ///
    /// A bad FSInfo does not fail the mount. It carries no information the FAT
    /// does not already hold, so refusing to mount over it would turn a
    /// cosmetic inconsistency into an unbootable machine.
    pub fn read<D: BlockAccess>(dev: &mut D, geom: &Geometry, buf: &mut [u8]) -> FsInfo {
        let Some(sector) = geom.fsinfo_sector else { return FsInfo::UNKNOWN };
        let Some(buf) = buf.get_mut(..geom.bytes_per_sector as usize) else { return FsInfo::UNKNOWN };
        if dev.read_at(geom.sector_offset(sector), buf).is_err() {
            return FsInfo::UNKNOWN;
        }
        if u32_at(buf, 0) != 0x4161_5252
            || u32_at(buf, 484) != 0x6141_7272
            || u32_at(buf, 508) != 0xAA55_0000
        {
            return FsInfo::UNKNOWN;
        }
        let free_raw = u32_at(buf, 488);
        FsInfo {
            free_count: if free_raw <= geom.cluster_count { Some(free_raw) } else { None },
            next_free: geom.cluster(u32_at(buf, 492)),
            dirty: false,
        }
    }

    /// Write the hints back into an FSInfo sector that already exists,
    /// preserving every other byte.
    ///
    /// Read-modify-write rather than synthesising the sector: the signatures
    /// and the reserved regions belong to whoever formatted the volume, and
    /// rewriting them from our own idea of the layout is how a driver corrupts
    /// a filesystem it only meant to update a counter in.
    pub fn write<D: BlockAccess>(&self, dev: &mut D, geom: &Geometry, buf: &mut [u8]) -> Result<(), Error> {
        let Some(sector) = geom.fsinfo_sector else { return Ok(()) };
        let Some(buf) = buf.get_mut(..geom.bytes_per_sector as usize) else { return Ok(()) };
        let offset = geom.sector_offset(sector);
        dev.read_at(offset, buf)?;
        if u32_at(buf, 0) != 0x4161_5252 || u32_at(buf, 484) != 0x6141_7272 {
            return Ok(());
        }
        let put = |buf: &mut [u8], off: usize, v: u32| {
            if let Some(slot) = buf.get_mut(off..off + 4) {
                slot.copy_from_slice(&v.to_le_bytes());
            }
        };
        put(buf, 488, self.free_count.unwrap_or(0xFFFF_FFFF));
        put(buf, 492, self.next_free.map_or(0xFFFF_FFFF, Cluster::raw));
        dev.write_at(offset, buf)?;
        Ok(())
    }
}
