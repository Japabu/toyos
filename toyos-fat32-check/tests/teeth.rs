//! One test per invariant, each breaking a clean volume in exactly that way.
//!
//! A checker fed only clean volumes is decoration: it is silent whether or not
//! it looks. So every complaint this crate can make has a test here that
//! constructs the state it is about and asserts the complaint comes back, and
//! [`the_fixture_is_clean`] is what makes each of those a *change* rather than
//! a volume that was already broken.

mod common;

use common::*;
use toyos_fat32_check::{check, describe, Complaint, MAX_COMPLAINTS};

/// The volume is broken in one place and the checker says so by name.
macro_rules! complains {
    ($volume:expr, $want:pat if $guard:expr) => {{
        let v: &Volume = &$volume;
        let got = check(&v.bytes);
        assert!(
            got.iter().any(|c| matches!(c, $want if $guard)),
            "wanted {} if {}, and the checker said:\n{}",
            stringify!($want),
            stringify!($guard),
            if got.is_empty() { "nothing at all".to_string() } else { describe(&got) }
        );
        got
    }};
    ($volume:expr, $want:pat) => {{
        let v: &Volume = &$volume;
        let got = check(&v.bytes);
        assert!(
            got.iter().any(|c| matches!(c, $want)),
            "wanted {}, and the checker said:\n{}",
            stringify!($want),
            if got.is_empty() { "nothing at all".to_string() } else { describe(&got) }
        );
        got
    }};
}

#[test]
fn the_fixture_is_clean() {
    let v = fixture();
    let got = check(&v.bytes);
    assert!(got.is_empty(), "the volume every mutation starts from is not clean:\n{}", describe(&got));
}

/// A volume built and then *not* finished: the mirror never copied, the free
/// count never written. Both halves of `finish` earn their place, so a fixture
/// that skipped it could not be the baseline.
#[test]
fn an_unfinished_fixture_is_not_clean() {
    let mut v = Volume::new();
    v.add_file(ROOT_CLUSTER, "f", b"F       BIN", None, 4096);
    let got = check(&v.bytes);
    assert!(
        got.iter().any(|c| matches!(c, Complaint::FatMirror { .. }))
            && got.iter().any(|c| matches!(c, Complaint::FsInfoFreeCountUnknown)),
        "wanted the mirror and the free count, and the checker said:\n{}",
        describe(&got)
    );
}

// ------------------------------------------------------------ boot sector

#[test]
fn a_boot_sector_that_does_not_jump() {
    let mut v = fixture();
    v.poke(0, &[0x00, 0x00, 0x00]);
    complains!(v, Complaint::JmpBoot { got: [0, 0, 0] });
}

#[test]
fn a_sector_size_the_format_does_not_define() {
    let mut v = fixture();
    v.poke_u16(BPB_BYTS_PER_SEC, 777);
    complains!(v, Complaint::BytesPerSector { got: 777 });
}

#[test]
fn a_cluster_that_is_not_a_power_of_two_sectors() {
    let mut v = fixture();
    v.bytes[BPB_SEC_PER_CLUS] = 3;
    complains!(v, Complaint::SectorsPerCluster { got: 3 });
}

#[test]
fn a_cluster_past_the_formats_ceiling() {
    let mut v = fixture();
    v.bytes[BPB_SEC_PER_CLUS] = 128;
    complains!(v, Complaint::BytesPerCluster { got: 65536 });
}

#[test]
fn no_reserved_sectors_at_all() {
    let mut v = fixture();
    v.poke_u16(BPB_RSVD_SEC_CNT, 0);
    complains!(v, Complaint::ReservedSectors);
}

#[test]
fn no_fat_at_all() {
    let mut v = fixture();
    v.bytes[BPB_NUM_FATS] = 0;
    complains!(v, Complaint::NumFats);
}

#[test]
fn a_fat32_volume_claiming_a_fixed_root_directory() {
    let mut v = fixture();
    v.poke_u16(BPB_ROOT_ENT_CNT, 512);
    complains!(v, Complaint::RootEntryCount { got: 512 });
}

#[test]
fn a_fat32_volume_counting_sectors_in_the_sixteen_bit_field() {
    let mut v = fixture();
    v.poke_u16(BPB_TOT_SEC_16, 1234);
    complains!(v, Complaint::TotalSectors16 { got: 1234 });
}

#[test]
fn a_fat32_volume_sizing_its_fat_in_the_sixteen_bit_field() {
    let mut v = fixture();
    v.poke_u16(BPB_FAT_SZ_16, 200);
    complains!(v, Complaint::FatSize16 { got: 200 });
}

#[test]
fn a_volume_of_no_sectors() {
    let mut v = fixture();
    v.poke_u32(BPB_TOT_SEC_32, 0);
    complains!(v, Complaint::TotalSectors32);
}

#[test]
fn a_fat_of_no_sectors() {
    let mut v = fixture();
    v.poke_u32(BPB_FAT_SZ_32, 0);
    complains!(v, Complaint::FatSize32);
}

#[test]
fn a_boot_sector_without_its_signature() {
    let mut v = fixture();
    v.poke_u16(BOOT_SIGNATURE, 0x1234);
    complains!(v, Complaint::BootSectorSignature { got: 0x1234 });
}

#[test]
fn a_media_byte_the_format_does_not_define() {
    let mut v = fixture();
    v.bytes[BPB_MEDIA] = 0x42;
    complains!(v, Complaint::Media { got: 0x42 });
}

#[test]
fn a_file_system_version_this_specification_does_not_define() {
    let mut v = fixture();
    v.poke_u16(BPB_FS_VER, 0x0100);
    complains!(v, Complaint::FileSystemVersion { got: 0x0100 });
}

#[test]
fn a_root_cluster_that_is_not_a_cluster() {
    let mut v = fixture();
    v.poke_u32(BPB_ROOT_CLUS, 1);
    complains!(v, Complaint::RootCluster { got: 1, .. });
}

#[test]
fn an_fsinfo_sector_outside_the_reserved_region() {
    let mut v = fixture();
    v.poke_u16(BPB_FS_INFO, RESERVED_SECTORS as u16);
    complains!(v, Complaint::FsInfoSector { .. });
}

#[test]
fn an_active_fat_the_volume_does_not_have() {
    let mut v = fixture();
    // Bit 7 turns mirroring off, and bits 0..3 then name the one live copy.
    v.poke_u16(BPB_EXT_FLAGS, 0x0085);
    complains!(v, Complaint::ActiveFat { got: 5, num_fats: 2 });
}

#[test]
fn a_volume_shorter_than_its_boot_sector_says() {
    let mut v = fixture();
    v.bytes.truncate(VOLUME_BYTES - BYTES_PER_SECTOR);
    complains!(v, Complaint::VolumeShorterThanDeclared { .. });
}

#[test]
fn a_volume_with_too_few_clusters_to_be_fat32() {
    let mut v = fixture();
    // 65,524 clusters is a FAT16 volume however the boot sector describes
    // itself, so the same bytes read differently under a correct driver.
    let shrunk = RESERVED_SECTORS + NUM_FATS * FAT_SECTORS + 65_524;
    v.poke_u32(BPB_TOT_SEC_32, shrunk as u32);
    complains!(v, Complaint::NotFat32 { clusters: 65_524 });
}

#[test]
fn a_fat_too_small_for_the_clusters_it_must_describe() {
    let mut v = fixture();
    v.poke_u32(BPB_FAT_SZ_32, 100);
    complains!(v, Complaint::FatTooSmall { .. });
}

#[test]
fn a_volume_that_is_all_metadata() {
    let mut v = fixture();
    v.poke_u32(BPB_TOT_SEC_32, (RESERVED_SECTORS + NUM_FATS * FAT_SECTORS) as u32);
    complains!(v, Complaint::NoDataArea { .. });
}

#[test]
fn fewer_bytes_than_a_boot_sector() {
    let got = check(&[0u8; 100]);
    assert!(
        got.iter().any(|c| matches!(c, Complaint::NoBootSector { bytes: 100 })),
        "{}",
        describe(&got)
    );
}

// ---------------------------------------------------------------- FSInfo

#[test]
fn an_fsinfo_without_its_lead_signature() {
    let mut v = fixture();
    v.poke_u32(FSINFO_AT + FSI_LEAD_SIG, 0xDEAD_BEEF);
    complains!(v, Complaint::FsInfoLeadSignature { got: 0xDEAD_BEEF });
}

#[test]
fn an_fsinfo_without_its_struct_signature() {
    let mut v = fixture();
    v.poke_u32(FSINFO_AT + FSI_STRUC_SIG, 0);
    complains!(v, Complaint::FsInfoStructSignature { got: 0 });
}

#[test]
fn an_fsinfo_without_its_trail_signature() {
    let mut v = fixture();
    v.poke_u32(FSINFO_AT + FSI_TRAIL_SIG, 0);
    complains!(v, Complaint::FsInfoTrailSignature { got: 0 });
}

#[test]
fn a_free_count_that_is_not_the_free_count() {
    let mut v = fixture();
    let at = FSINFO_AT + FSI_FREE_COUNT;
    let was = u32::from_le_bytes(v.bytes[at..at + 4].try_into().expect("four bytes"));
    v.poke_u32(at, was + 7);
    complains!(v, Complaint::FsInfoFreeCount { .. });
}

#[test]
fn a_free_count_the_writer_left_unknown() {
    let mut v = fixture();
    v.poke_u32(FSINFO_AT + FSI_FREE_COUNT, u32::MAX);
    complains!(v, Complaint::FsInfoFreeCountUnknown);
}

#[test]
fn a_next_free_hint_that_is_not_a_cluster() {
    let mut v = fixture();
    v.poke_u32(FSINFO_AT + FSI_NXT_FREE, CLUSTERS + 900);
    complains!(v, Complaint::FsInfoNextFree { .. });
}

// ------------------------------------------------------------------- FAT

#[test]
fn a_fat_zero_that_does_not_carry_the_media_byte() {
    let mut v = fixture();
    v.poke_u32(fat_offset(0, 0), 0x0FFF_FFF0);
    v.poke_u32(fat_offset(1, 0), 0x0FFF_FFF0);
    complains!(v, Complaint::Fat0 { got: 0x0FFF_FFF0, want: 0x0FFF_FFF8 });
}

#[test]
fn a_fat_one_that_is_not_an_end_of_chain_mark() {
    let mut v = fixture();
    v.poke_u32(fat_offset(0, 1), 0x0FFF_FFFD);
    v.poke_u32(fat_offset(1, 1), 0x0FFF_FFFD);
    complains!(v, Complaint::Fat1 { got: 0x0FFF_FFFD });
}

#[test]
fn a_volume_flagged_as_not_cleanly_unmounted() {
    let mut v = fixture();
    v.poke_u32(fat_offset(0, 1), EOC & !0x0800_0000);
    v.poke_u32(fat_offset(1, 1), EOC & !0x0800_0000);
    complains!(v, Complaint::VolumeDirty);
}

#[test]
fn a_volume_flagged_as_having_met_a_disk_error() {
    let mut v = fixture();
    v.poke_u32(fat_offset(0, 1), EOC & !0x0400_0000);
    v.poke_u32(fat_offset(1, 1), EOC & !0x0400_0000);
    complains!(v, Complaint::VolumeHardError);
}

/// The first of the two things `fsck_msdos` never checked: a mount reads the
/// active copy only, so this volume reads back perfectly until something
/// consults the mirror — a repair tool, or a driver whose `BPB_ExtFlags` says
/// otherwise.
#[test]
fn a_fat_copy_left_behind_by_a_write() {
    let mut v = fixture();
    let first = v.at("short.txt").first;
    v.poke_u32(fat_offset(1, first), 0);
    complains!(v, Complaint::FatMirror { fat: 1, .. });
}

// ---------------------------------------------------------------- chains

#[test]
fn a_chain_link_that_is_not_a_cluster() {
    let mut v = fixture();
    let first = v.at("long.bin").first;
    v.set_fat(first, 999_999);
    v.finish();
    complains!(v, Complaint::ChainOutOfRange { next: 999_999, .. });
}

#[test]
fn a_chain_link_into_the_bad_cluster_mark() {
    let mut v = fixture();
    let first = v.at("long.bin").first;
    v.set_fat(first, BAD_CLUSTER);
    v.finish();
    complains!(v, Complaint::ChainBadCluster { .. });
}

#[test]
fn a_chain_that_links_back_into_itself() {
    let mut v = fixture();
    let first = v.at("long.bin").first;
    v.set_fat(first + 2, first);
    v.finish();
    complains!(v, Complaint::ChainCycle { back_to, .. } if *back_to == first);
}

#[test]
fn two_files_holding_one_cluster() {
    let mut v = fixture();
    let shared = v.at("short.txt").first;
    let entry = v.at("sub/inner.dat").entry;
    v.poke_u16(entry + DIR_FST_CLUS_LO, shared as u16);
    v.poke_u16(entry + DIR_FST_CLUS_HI, (shared >> 16) as u16);
    complains!(v, Complaint::CrossLinked { at, .. } if *at == shared);
}

#[test]
fn a_file_whose_size_needs_more_clusters_than_it_holds() {
    let mut v = fixture();
    let entry = v.at("short.txt").entry;
    v.poke_u32(entry + DIR_FILE_SIZE, 40_000);
    complains!(v, Complaint::ChainTooShort { needed: 79, held: 1, .. });
}

#[test]
fn a_file_holding_more_clusters_than_its_size_needs() {
    let mut v = fixture();
    let entry = v.at("long.bin").entry;
    v.poke_u32(entry + DIR_FILE_SIZE, 10);
    complains!(v, Complaint::ChainTooLong { needed: 1, held: 6, .. });
}

#[test]
fn clusters_allocated_and_reachable_from_nothing() {
    let mut v = fixture();
    let orphan = CLUSTERS - 10;
    v.set_fat(orphan, orphan + 1);
    v.set_fat(orphan + 1, EOC);
    v.finish();
    complains!(v, Complaint::LostChain { first, clusters: 2 } if *first == orphan);
}

/// A leak with no head to start the walk at: every cluster of it is pointed at
/// by another, so the sweep for one is not enough.
#[test]
fn an_orphaned_ring_of_clusters() {
    let mut v = fixture();
    let ring = CLUSTERS - 20;
    v.set_fat(ring, ring + 1);
    v.set_fat(ring + 1, ring);
    v.finish();
    complains!(v, Complaint::LostChain { clusters: 2, .. });
}

// ----------------------------------------------------------- directories

#[test]
fn a_subdirectory_that_does_not_begin_with_dot() {
    let mut v = fixture();
    let sub = v.at("sub").first;
    v.poke(cluster_offset(sub), b"NOTDOT  BIN");
    complains!(v, Complaint::DotEntry { entry: 0, .. });
}

#[test]
fn a_dot_entry_naming_someone_elses_cluster() {
    let mut v = fixture();
    let sub = v.at("sub").first;
    v.poke_u16(cluster_offset(sub) + DIR_FST_CLUS_LO, (sub + 1) as u16);
    complains!(v, Complaint::DotCluster { .. });
}

/// The defect `fatfs`'s `create_dir` had, which cost every image this project
/// built twelve complaints: `..` in a directory whose parent is the root must
/// be 0, and it wrote the root's own cluster number.
#[test]
fn a_dot_dot_naming_the_root_cluster_instead_of_zero() {
    let mut v = fixture();
    let sub = v.at("sub").first;
    v.poke_u16(cluster_offset(sub) + 32 + DIR_FST_CLUS_LO, ROOT_CLUSTER as u16);
    complains!(v, Complaint::DotDotCluster { got: 2, want: 0, .. });
}

/// `fatfs`'s other one: a long-name entry written ahead of `.`, which puts the
/// dot entries somewhere other than the two places the format defines.
#[test]
fn a_long_name_entry_ahead_of_the_dot_entries() {
    let mut v = fixture();
    let sub = v.at("sub").first;
    let at = cluster_offset(sub);
    let mut moved = [0u8; 64];
    moved.copy_from_slice(&v.bytes[at..at + 64]);
    v.poke(at + 32, &moved[..32]);
    v.poke(at + 64, &moved[32..]);
    let lfn = long_entries(".", b".          ");
    v.poke(at, &lfn[0]);
    complains!(v, Complaint::DotEntry { entry: 0, .. });
}

#[test]
fn a_directory_entry_carrying_a_size() {
    let mut v = fixture();
    let entry = v.at("sub").entry;
    v.poke_u32(entry + DIR_FILE_SIZE, 512);
    complains!(v, Complaint::DirectorySize { size: 512, .. });
}

#[test]
fn a_directory_entry_naming_no_cluster() {
    let mut v = fixture();
    let entry = v.at("sub").entry;
    v.poke_u16(entry + DIR_FST_CLUS_LO, 0);
    v.poke_u16(entry + DIR_FST_CLUS_HI, 0);
    complains!(v, Complaint::DirectoryHasNoCluster { .. });
}

#[test]
fn an_entry_naming_a_cluster_outside_the_volume() {
    let mut v = fixture();
    let entry = v.at("short.txt").entry;
    v.poke_u16(entry + DIR_FST_CLUS_HI, 0x00FF);
    complains!(v, Complaint::FirstCluster { .. });
}

#[test]
fn a_reserved_entry_byte_carrying_something_nothing_defines() {
    let mut v = fixture();
    let entry = v.at("short.txt").entry;
    v.bytes[entry + DIR_NT_RES] = 0x20;
    complains!(v, Complaint::ReservedEntryByte { got: 0x20, .. });
}

/// The two bits Windows NT defined and everything writes: not a complaint, or
/// the checker reds on every volume macOS has ever touched.
#[test]
fn the_lowercase_name_bits_are_not_a_complaint() {
    let mut v = fixture();
    let (base, ext) = (v.at("short.txt").entry, v.at("sub/inner.dat").entry);
    v.bytes[base + DIR_NT_RES] = 0x18;
    v.bytes[ext + DIR_NT_RES] = 0x08;
    let got = check(&v.bytes);
    assert!(got.is_empty(), "the lowercase-name bits are a complaint:\n{}", describe(&got));
}

// ---------------------------------------------------------- long names

#[test]
fn a_long_name_run_whose_checksum_is_not_its_short_names() {
    let mut v = fixture();
    let at = v.at("long.bin").longs[1];
    v.bytes[at + LDIR_CHKSUM] ^= 0xFF;
    complains!(v, Complaint::LongNameChecksum { .. });
}

#[test]
fn a_long_name_run_whose_ordinals_do_not_count_down() {
    let mut v = fixture();
    let at = v.at("long.bin").longs[1];
    v.bytes[at + LDIR_ORD] = 7;
    complains!(v, Complaint::LongNameOrdinal { got: 7, .. });
}

#[test]
fn a_long_name_run_that_does_not_reach_ordinal_one() {
    let mut v = fixture();
    let longs = &v.at("long.bin").longs;
    let (first, last) = (longs[0], longs[longs.len() - 1]);
    // Drop the final entry by making the run one shorter and turning the last
    // into a repeat of its predecessor's ordinal.
    v.bytes[first + LDIR_ORD] = 2 | LAST_LONG_ENTRY;
    let checksum = v.bytes[last + LDIR_CHKSUM];
    v.poke(last, &[0xE5]);
    v.bytes[last + LDIR_CHKSUM] = checksum;
    complains!(v, Complaint::LongNameOrdinal { want: 1, .. });
}

#[test]
fn a_long_name_run_that_does_not_begin_with_the_last_entry_flag() {
    let mut v = fixture();
    let at = v.at("long.bin").longs[0];
    v.bytes[at + LDIR_ORD] &= !LAST_LONG_ENTRY;
    complains!(v, Complaint::LongNameLastFlag { .. });
}

#[test]
fn a_long_name_run_of_more_entries_than_a_long_name_has() {
    let mut v = fixture();
    let at = v.at("long.bin").longs[0];
    v.bytes[at + LDIR_ORD] = 25 | LAST_LONG_ENTRY;
    complains!(v, Complaint::LongNameRunLength { got: 25, .. });
}

#[test]
fn a_long_name_run_no_short_entry_follows() {
    let mut v = fixture();
    let entry = v.at("long.bin").entry;
    v.poke(entry, &[0xE5]);
    complains!(v, Complaint::OrphanLongName { .. });
}

#[test]
fn a_long_name_entry_naming_a_cluster() {
    let mut v = fixture();
    let at = v.at("long.bin").longs[2];
    v.poke_u16(at + LDIR_FST_CLUS_LO, 9);
    complains!(v, Complaint::LongNameCluster { got: 9, .. });
}

#[test]
fn a_long_name_entry_with_a_type_the_format_reserves() {
    let mut v = fixture();
    let at = v.at("long.bin").longs[0];
    v.bytes[at + LDIR_TYPE] = 1;
    complains!(v, Complaint::LongNameType { got: 1, .. });
}

// --------------------------------------------------------- names, labels

/// The second thing `fsck_msdos` never checked. Both it and a mount read the
/// long names, so a writer that stops uniquifying short names leaves two
/// entries that a short-name reader — every FAT driver's fallback — cannot tell
/// apart, and every test in sight stays green.
#[test]
fn two_entries_in_one_directory_with_the_same_short_name() {
    let mut v = fixture();
    let entry = v.at("long.bin").entry;
    v.poke(entry, b"SHORT   TXT");
    // The long-name run in front of it now names a different short name, which
    // is its own complaint; the duplicate is the one under test.
    let sum = checksum(b"SHORT   TXT");
    for at in v.at("long.bin").longs.clone() {
        v.bytes[at + LDIR_CHKSUM] = sum;
    }
    complains!(v, Complaint::DuplicateShortName { .. });
}

#[test]
fn a_volume_label_outside_the_root_directory() {
    let mut v = fixture();
    let sub = v.at("sub").first;
    v.add_label(sub, "stray", b"STRAYLABEL ");
    v.finish();
    complains!(v, Complaint::VolumeLabelInSubdirectory { .. });
}

#[test]
fn a_second_volume_label_in_the_root() {
    let mut v = fixture();
    v.add_label(ROOT_CLUSTER, "second", b"SECOND     ");
    v.finish();
    complains!(v, Complaint::ExtraVolumeLabel { count: 2 });
}

#[test]
fn a_dot_entry_in_the_root_directory() {
    let mut v = fixture();
    let entry = v.at("short.txt").entry;
    v.poke(entry, b".          ");
    complains!(v, Complaint::DotInRoot { .. });
}

// ------------------------------------------------------------- the bound

/// Past [`MAX_COMPLAINTS`] the report says so rather than reading as complete.
#[test]
fn a_report_that_stops_enumerating_says_it_stopped() {
    let mut v = fixture();
    for orphan in 1000..1000 + MAX_COMPLAINTS as u32 + 20 {
        v.set_fat(orphan, EOC);
    }
    v.finish();
    let got = check(&v.bytes);
    assert_eq!(got.len(), MAX_COMPLAINTS + 1, "{}", describe(&got));
    assert!(
        matches!(got[MAX_COMPLAINTS], Complaint::More { .. }),
        "the last line of a truncated report is not the truncation:\n{}",
        describe(&got)
    );
}

/// A tree deeper than the walk follows is reported and not followed, so a
/// crafted volume cannot make the checker expensive.
#[test]
fn a_tree_deeper_than_the_walk_follows() {
    let mut v = Volume::new();
    v.add_label(ROOT_CLUSTER, "label", b"TOYOSCHK   ");
    let mut parent = ROOT_CLUSTER;
    let mut link = 0;
    for depth in 0..70 {
        let key = format!("d{depth}");
        let mut name = *b"D000       ";
        name[1..4].copy_from_slice(format!("{depth:03}").as_bytes());
        let child = v.add_dir(parent, &key, &name, link);
        link = parent;
        parent = child;
    }
    v.finish();
    complains!(v, Complaint::TooDeep { .. });
}
