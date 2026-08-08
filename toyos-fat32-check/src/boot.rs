//! The boot sector, the BPB, and the FSInfo structure beside it.
//!
//! Field names are the specification's, so a complaint names the same thing a
//! reader of fatgen103 §3.1 and §5 is looking at.

use crate::{Complaint, Report};

/// `BPB_Media`'s legal values (fatgen103 §3.1). A volume that is not removable
/// media uses 0xF8, and the value is repeated in the low byte of `FAT[0]`.
fn media_is_legal(media: u8) -> bool {
    media == 0xF0 || media >= 0xF8
}

/// The one geometry every other question is asked in terms of.
#[derive(Clone, Copy)]
pub(crate) struct Geometry {
    pub bytes_per_sector: u64,
    pub sectors_per_cluster: u64,
    pub reserved_sectors: u64,
    pub num_fats: u64,
    pub fat_sectors: u64,
    pub cluster_count: u32,
    pub root_cluster: u32,
    pub media: u8,
    pub fs_info_sector: Option<u64>,
    /// Which FAT copy a driver reads, per `BPB_ExtFlags`.
    pub active_fat: u64,
    /// `BPB_ExtFlags` bit 7 clear: every write goes to every copy, so the
    /// copies must agree.
    pub mirrored: bool,
}

impl Geometry {
    pub fn bytes_per_cluster(&self) -> u64 {
        self.bytes_per_sector * self.sectors_per_cluster
    }

    pub fn fat_offset(&self, copy: u64) -> u64 {
        (self.reserved_sectors + copy * self.fat_sectors) * self.bytes_per_sector
    }

    pub fn fat_bytes(&self) -> u64 {
        self.fat_sectors * self.bytes_per_sector
    }

    pub fn data_offset(&self) -> u64 {
        (self.reserved_sectors + self.num_fats * self.fat_sectors) * self.bytes_per_sector
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        self.data_offset() + (u64::from(cluster) - 2) * self.bytes_per_cluster()
    }

    pub fn holds(&self, cluster: u32) -> bool {
        cluster >= 2 && cluster <= self.cluster_count + 1
    }
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Read the BPB and derive the geometry, or say why the volume has none.
///
/// Everything checkable about the boot sector is reported before the derivation
/// is attempted, so a volume with three broken fields names three rather than
/// the first.
pub(crate) fn decode(vol: &[u8], r: &mut Report) -> Option<Geometry> {
    if vol.len() < 512 {
        r.say(Complaint::NoBootSector { bytes: vol.len() as u64 });
        return None;
    }

    let jmp = [vol[0], vol[1], vol[2]];
    if !((jmp[0] == 0xEB && jmp[2] == 0x90) || jmp[0] == 0xE9) {
        r.say(Complaint::JmpBoot { got: jmp });
    }

    let signature = u16_at(vol, 510);
    if signature != 0xAA55 {
        r.say(Complaint::BootSectorSignature { got: signature });
    }

    let bytes_per_sector = u16_at(vol, 11);
    let sectors_per_cluster = vol[13];
    let reserved_sectors = u16_at(vol, 14);
    let num_fats = vol[16];
    let root_entries = u16_at(vol, 17);
    let total_sectors_16 = u16_at(vol, 19);
    let media = vol[21];
    let fat_size_16 = u16_at(vol, 22);
    let total_sectors_32 = u32_at(vol, 32);
    let fat_size_32 = u32_at(vol, 36);
    let ext_flags = u16_at(vol, 40);
    let fs_version = u16_at(vol, 42);
    let root_cluster = u32_at(vol, 44);
    let fs_info = u16_at(vol, 48);

    let sector_legal = matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096);
    if !sector_legal {
        r.say(Complaint::BytesPerSector { got: bytes_per_sector });
    }
    let cluster_legal = sectors_per_cluster.is_power_of_two();
    if !cluster_legal {
        r.say(Complaint::SectorsPerCluster { got: sectors_per_cluster });
    }
    if sector_legal && cluster_legal {
        let bytes = u64::from(bytes_per_sector) * u64::from(sectors_per_cluster);
        if bytes > 32 * 1024 {
            r.say(Complaint::BytesPerCluster { got: bytes });
        }
    }
    if reserved_sectors == 0 {
        r.say(Complaint::ReservedSectors);
    }
    if num_fats == 0 {
        r.say(Complaint::NumFats);
    }
    if root_entries != 0 {
        r.say(Complaint::RootEntryCount { got: root_entries });
    }
    if total_sectors_16 != 0 {
        r.say(Complaint::TotalSectors16 { got: total_sectors_16 });
    }
    if fat_size_16 != 0 {
        r.say(Complaint::FatSize16 { got: fat_size_16 });
    }
    if total_sectors_32 == 0 {
        r.say(Complaint::TotalSectors32);
    }
    if fat_size_32 == 0 {
        r.say(Complaint::FatSize32);
    }
    if !media_is_legal(media) {
        r.say(Complaint::Media { got: media });
    }
    if fs_version != 0 {
        r.say(Complaint::FileSystemVersion { got: fs_version });
    }

    if !sector_legal || !cluster_legal || reserved_sectors == 0 || num_fats == 0
        || total_sectors_32 == 0 || fat_size_32 == 0
    {
        return None;
    }

    let bytes_per_sector = u64::from(bytes_per_sector);
    let sectors_per_cluster = u64::from(sectors_per_cluster);
    let reserved_sectors = u64::from(reserved_sectors);
    let num_fats = u64::from(num_fats);
    let fat_sectors = u64::from(fat_size_32);
    let total_sectors = u64::from(total_sectors_32);

    let declared_bytes = total_sectors * bytes_per_sector;
    if declared_bytes > vol.len() as u64 {
        r.say(Complaint::VolumeShorterThanDeclared {
            declared_bytes,
            actual_bytes: vol.len() as u64,
        });
        return None;
    }

    let metadata_sectors = reserved_sectors + num_fats * fat_sectors;
    if metadata_sectors >= total_sectors {
        r.say(Complaint::NoDataArea { metadata_sectors, total_sectors });
        return None;
    }
    // fatgen103 §3.5: the count of clusters is the data area divided by the
    // cluster size, and the format *is* whichever of the three that count
    // falls into. Under 65525 the same bytes are a FAT16 volume, whatever the
    // boot sector calls itself.
    let clusters = (total_sectors - metadata_sectors) / sectors_per_cluster;
    let cluster_count = u32::try_from(clusters).unwrap_or(u32::MAX);
    if cluster_count < 65525 {
        r.say(Complaint::NotFat32 { clusters: cluster_count });
        return None;
    }

    let needed_bytes = (u64::from(cluster_count) + 2) * 4;
    let fat_bytes = fat_sectors * bytes_per_sector;
    if fat_bytes < needed_bytes {
        r.say(Complaint::FatTooSmall { fat_bytes, needed_bytes });
        return None;
    }

    if !(2..=cluster_count + 1).contains(&root_cluster) {
        r.say(Complaint::RootCluster { got: root_cluster, clusters: cluster_count });
        return None;
    }

    // fatgen103 §3.5.1: bit 7 clear means every FAT is written; set means only
    // the copy bits 0..3 name is.
    let mirrored = ext_flags & 0x0080 == 0;
    let active_fat = if mirrored { 0 } else { u64::from(ext_flags & 0x000F) };
    if active_fat >= num_fats {
        r.say(Complaint::ActiveFat { got: active_fat as u32, num_fats });
        return None;
    }

    let fs_info_sector = if fs_info >= 1 && u64::from(fs_info) < reserved_sectors {
        Some(u64::from(fs_info))
    } else {
        r.say(Complaint::FsInfoSector { got: fs_info, reserved: reserved_sectors });
        None
    };

    Some(Geometry {
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        num_fats,
        fat_sectors,
        cluster_count,
        root_cluster,
        media,
        fs_info_sector,
        active_fat,
        mirrored,
    })
}

/// The FSInfo sector as bytes, when the BPB gave it a place to be.
fn fs_info<'a>(vol: &'a [u8], geo: &Geometry) -> Option<&'a [u8]> {
    let at = usize::try_from(geo.fs_info_sector? * geo.bytes_per_sector).ok()?;
    vol.get(at..at + 512)
}

/// The three signatures that say the sector is an FSInfo at all, and the
/// hint fields' ranges.
pub(crate) fn signatures(vol: &[u8], geo: &Geometry, r: &mut Report) {
    let Some(fsi) = fs_info(vol, geo) else { return };

    let lead = u32_at(fsi, 0);
    if lead != 0x4161_5252 {
        r.say(Complaint::FsInfoLeadSignature { got: lead });
    }
    let struc = u32_at(fsi, 484);
    if struc != 0x6141_7272 {
        r.say(Complaint::FsInfoStructSignature { got: struc });
    }
    let trail = u32_at(fsi, 508);
    if trail != 0xAA55_0000 {
        r.say(Complaint::FsInfoTrailSignature { got: trail });
    }

    let next_free = u32_at(fsi, 492);
    if next_free != u32::MAX && !geo.holds(next_free) {
        r.say(Complaint::FsInfoNextFree { got: next_free, clusters: geo.cluster_count });
    }
}

/// `FSI_Free_Count` against the FAT it claims to summarise.
///
/// A stale number is the failure mode: every host that mounts the volume
/// reports free space from here without counting. fatgen103 §5 also allows
/// 0xFFFFFFFF, meaning the writer does not maintain the field — see
/// [`Complaint::FsInfoFreeCountUnknown`] for why that is a complaint here and
/// the only one in this crate that is not the format's own.
pub(crate) fn free_count(vol: &[u8], geo: &Geometry, table: &[u32], r: &mut Report) {
    let Some(fsi) = fs_info(vol, geo) else { return };
    let declared = u32_at(fsi, 488);
    if declared == u32::MAX {
        r.say(Complaint::FsInfoFreeCountUnknown);
        return;
    }
    let counted = table[2..].iter().filter(|&&e| e == 0).count() as u32;
    if declared != counted {
        r.say(Complaint::FsInfoFreeCount { declared, counted });
    }
}
