//! The parser against tables somebody else wrote.
//!
//! Every test here starts from one valid image and breaks exactly one thing,
//! so a failure names the field. The point is not that the happy path works —
//! a QEMU boot covers that against a real firmware and a real disk — but that
//! none of the broken ones panics, allocates by a number the disk chose, or
//! returns a partition anyway.

use toyos_gpt::{crc32, GptError, Guid, Located, Sectors};

const LBA: u32 = 512;
const ENTRY: u32 = 128;
const ARRAY_LBA: u64 = 2;
/// 128 entries of 128 bytes is 32 blocks of 512, which is why every GPT on
/// earth has its first usable block at 34.
const FIRST_USABLE: u64 = 34;
const DISK_LBAS: u64 = 2048;

const TYPE_ESP: Guid = Guid::EFI_SYSTEM;
const TYPE_OTHER: Guid = Guid([0x0F, 0xC6, 0x3D, 0xAF, 0x84, 0x83, 0x47, 0x72, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4]);

fn guid(n: u8) -> Guid {
    let mut b = [n; 16];
    b[0] = n;
    b[15] = n ^ 0xFF;
    Guid(b)
}

#[derive(Clone, Copy)]
struct Entry {
    type_guid: Guid,
    unique: Guid,
    first: u64,
    last: u64,
}

impl Entry {
    fn new(type_guid: Guid, unique: Guid, first: u64, last: u64) -> Self {
        Self { type_guid, unique, first, last }
    }
}

struct Builder {
    lba_bytes: u32,
    lba_count: u64,
    entry_count: u32,
    entry_bytes: u32,
    entry_array_lba: u64,
    first_usable: u64,
    last_usable: u64,
    revision: u32,
    header_bytes: u32,
    reserved: u32,
    my_lba: u64,
    entries: Vec<Entry>,
    hybrid_mbr: bool,
    no_mbr_signature: bool,
    /// Write a second, independent copy of the header and the array at the
    /// top of the device — LBA `lba_count - 1` and just below it — the way a
    /// real disk carries one. `false` by default and unchanged by it: every
    /// test above this field was written against a disk with no backup at
    /// all, and adding one silently would let a fallback this crate does not
    /// yet have paper over a primary this suite meant to break.
    backup: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            lba_bytes: LBA,
            lba_count: DISK_LBAS,
            entry_count: 128,
            entry_bytes: ENTRY,
            entry_array_lba: ARRAY_LBA,
            first_usable: FIRST_USABLE,
            last_usable: DISK_LBAS - FIRST_USABLE,
            revision: 0x0001_0000,
            header_bytes: 92,
            reserved: 0,
            my_lba: 1,
            entries: vec![
                // Two ESP-typed decoys before the real one, and the real one
                // is neither first nor an obvious pick: a matcher that keys on
                // the type GUID, or takes the first used entry, or takes the
                // biggest, gets a different answer than the one asserted.
                Entry::new(TYPE_ESP, guid(0xA1), 40, 99),
                Entry::new(TYPE_OTHER, guid(0xB2), 100, 199),
                Entry::new(TYPE_ESP, guid(0xC3), 200, 299),
                Entry::new(TYPE_ESP, guid(0xD4), 300, 1999),
            ],
            hybrid_mbr: false,
            no_mbr_signature: false,
            backup: false,
        }
    }
}

impl Builder {
    fn build(&self) -> Image {
        let lba = self.lba_bytes as usize;
        let mut disk = vec![0u8; lba * self.lba_count as usize];

        if !self.no_mbr_signature {
            disk[510] = 0x55;
            disk[511] = 0xAA;
        }
        disk[446 + 4] = 0xEE;
        disk[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
        disk[446 + 12..446 + 16].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        if self.hybrid_mbr {
            disk[446 + 16 + 4] = 0x83;
        }

        // A header may name an array LBA this disk does not have; the parser
        // is what has to notice, so the builder just writes no entries.
        let array_at = (self.entry_array_lba as usize).saturating_mul(lba).min(disk.len());
        for (i, e) in self.entries.iter().enumerate() {
            let at = array_at + i * self.entry_bytes as usize;
            if at + 128 > disk.len() {
                break;
            }
            disk[at..at + 16].copy_from_slice(&e.type_guid.0);
            disk[at + 16..at + 32].copy_from_slice(&e.unique.0);
            disk[at + 32..at + 40].copy_from_slice(&e.first.to_le_bytes());
            disk[at + 40..at + 48].copy_from_slice(&e.last.to_le_bytes());
        }

        let array_bytes = (self.entry_count as usize).saturating_mul(self.entry_bytes as usize);
        let array_crc = if array_at.saturating_add(array_bytes) <= disk.len() {
            crc32(&disk[array_at..array_at + array_bytes])
        } else {
            0
        };

        let mut h = vec![0u8; lba];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&self.revision.to_le_bytes());
        h[12..16].copy_from_slice(&self.header_bytes.to_le_bytes());
        h[20..24].copy_from_slice(&self.reserved.to_le_bytes());
        h[24..32].copy_from_slice(&self.my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&(self.lba_count - 1).to_le_bytes());
        h[40..48].copy_from_slice(&self.first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&self.last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&guid(0x5D).0);
        h[72..80].copy_from_slice(&self.entry_array_lba.to_le_bytes());
        h[80..84].copy_from_slice(&self.entry_count.to_le_bytes());
        h[84..88].copy_from_slice(&self.entry_bytes.to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let size = (self.header_bytes as usize).min(lba).max(16);
        let crc = crc32(&h[..size]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        disk[lba..lba * 2].copy_from_slice(&h);

        if self.backup {
            // The backup array sits directly below the backup header, at the
            // top of the device — the mirror of the primary's layout, where
            // the array follows the header. Same entries, same CRC: this
            // builds an honest mirror, not an independent second table, because the
            // point of `backup` is a torn front recovering from an intact
            // back, not two disks disagreeing.
            let array_lbas = (self.entry_count as u64 * self.entry_bytes as u64).div_ceil(lba as u64);
            let backup_header_lba = self.lba_count - 1;
            let backup_array_lba = backup_header_lba - array_lbas;
            let backup_array_at = (backup_array_lba as usize).saturating_mul(lba);
            for (i, e) in self.entries.iter().enumerate() {
                let at = backup_array_at + i * self.entry_bytes as usize;
                if at + 128 > disk.len() {
                    break;
                }
                disk[at..at + 16].copy_from_slice(&e.type_guid.0);
                disk[at + 16..at + 32].copy_from_slice(&e.unique.0);
                disk[at + 32..at + 40].copy_from_slice(&e.first.to_le_bytes());
                disk[at + 40..at + 48].copy_from_slice(&e.last.to_le_bytes());
            }
            let backup_array_crc = crc32(&disk[backup_array_at..backup_array_at + array_bytes]);

            let mut hb = vec![0u8; lba];
            hb[..8].copy_from_slice(b"EFI PART");
            hb[8..12].copy_from_slice(&self.revision.to_le_bytes());
            hb[12..16].copy_from_slice(&self.header_bytes.to_le_bytes());
            hb[20..24].copy_from_slice(&self.reserved.to_le_bytes());
            hb[24..32].copy_from_slice(&backup_header_lba.to_le_bytes());
            hb[32..40].copy_from_slice(&1u64.to_le_bytes()); // AlternateLBA: the primary, at LBA 1.
            hb[40..48].copy_from_slice(&self.first_usable.to_le_bytes());
            hb[48..56].copy_from_slice(&self.last_usable.to_le_bytes());
            hb[56..72].copy_from_slice(&guid(0x5D).0);
            hb[72..80].copy_from_slice(&backup_array_lba.to_le_bytes());
            hb[80..84].copy_from_slice(&self.entry_count.to_le_bytes());
            hb[84..88].copy_from_slice(&self.entry_bytes.to_le_bytes());
            hb[88..92].copy_from_slice(&backup_array_crc.to_le_bytes());
            let hb_crc = crc32(&hb[..size]);
            hb[16..20].copy_from_slice(&hb_crc.to_le_bytes());
            let backup_header_at = backup_header_lba as usize * lba;
            disk[backup_header_at..backup_header_at + lba].copy_from_slice(&hb);
        }

        Image { lba_bytes: self.lba_bytes, lba_count: self.lba_count, bytes: disk, fail_at: None }
    }
}

struct Image {
    lba_bytes: u32,
    lba_count: u64,
    bytes: Vec<u8>,
    fail_at: Option<u64>,
}

impl Image {
    fn at(&mut self, lba: u64, off: usize) -> &mut u8 {
        &mut self.bytes[lba as usize * self.lba_bytes as usize + off]
    }
    fn locate(&mut self, target: Guid) -> Result<Located, GptError> {
        toyos_gpt::locate(self, target)
    }
}

impl Sectors for Image {
    fn lba_bytes(&self) -> u32 {
        self.lba_bytes
    }
    fn lba_count(&self) -> u64 {
        self.lba_count
    }
    fn read_lba(&mut self, lba: u64, buf: &mut [u8]) -> bool {
        if self.fail_at == Some(lba) {
            return false;
        }
        let at = lba as usize * self.lba_bytes as usize;
        let Some(src) = self.bytes.get(at..at + buf.len()) else {
            return false;
        };
        buf.copy_from_slice(src);
        true
    }
}

#[test]
fn finds_the_partition_by_unique_guid() {
    let mut img = Builder::default().build();
    let found = img.locate(guid(0xC3)).expect("the table has this GUID");
    assert_eq!(found.partition.index, 2);
    assert_eq!(found.partition.first_lba, 200);
    assert_eq!(found.partition.last_lba, 299);
    assert_eq!(found.partition.lba_count(), 100);
    assert_eq!(found.used_entries, 4);
    assert!(found.partition.is_efi_system());
    assert_eq!(found.disk_guid, guid(0x5D));
}

/// The one that matters: three of the four entries are ESPs, so anything
/// selecting on the type GUID picks the wrong disk region. Each of the four
/// must come back as itself.
#[test]
fn each_guid_finds_its_own_entry() {
    let want = [
        (guid(0xA1), 0u32, 40u64, 99u64),
        (guid(0xB2), 1, 100, 199),
        (guid(0xC3), 2, 200, 299),
        (guid(0xD4), 3, 300, 1999),
    ];
    for (g, index, first, last) in want {
        let mut img = Builder::default().build();
        let found = img.locate(g).expect("present");
        assert_eq!((found.partition.index, found.partition.first_lba, found.partition.last_lba), (index, first, last));
    }
}

#[test]
fn absent_guid_is_not_found_and_says_how_many_there_were() {
    let mut img = Builder::default().build();
    assert_eq!(img.locate(guid(0xEE)), Err(GptError::NotFound { used_entries: 4 }));
}

#[test]
fn a_zero_guid_matches_nothing() {
    let mut img = Builder::default().build();
    assert_eq!(img.locate(Guid::ZERO), Err(GptError::NotFound { used_entries: 4 }));
}

#[test]
fn no_protective_mbr() {
    let mut img = Builder { no_mbr_signature: true, ..Default::default() }.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::NoProtectiveMbr));

    let mut img = Builder::default().build();
    *img.at(0, 446 + 4) = 0x07;
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::NoProtectiveMbr));
}

/// A protective record next to a real one means two tables describe this disk.
#[test]
fn hybrid_mbr_is_refused() {
    let mut img = Builder { hybrid_mbr: true, ..Default::default() }.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::NoProtectiveMbr));
}

#[test]
fn header_signature() {
    let mut img = Builder::default().build();
    *img.at(1, 0) = b'X';
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::NoHeader));
}

#[test]
fn header_revision() {
    let mut img = Builder { revision: 0x0002_0000, ..Default::default() }.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::UnsupportedRevision(0x0002_0000)));
}

#[test]
fn header_size_bounds() {
    for bad in [0u32, 91, 513, u32::MAX] {
        let mut img = Builder { header_bytes: bad, ..Default::default() }.build();
        assert_eq!(img.locate(guid(0xC3)), Err(GptError::HeaderSize(bad)), "header_size {bad}");
    }
}

#[test]
fn header_reserved_word() {
    let mut img = Builder { reserved: 1, ..Default::default() }.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::HeaderReserved(1)));
}

#[test]
fn header_must_claim_lba_one() {
    let mut img = Builder { my_lba: 2, ..Default::default() }.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::HeaderMisplaced(2)));
}

#[test]
fn one_flipped_bit_in_the_header() {
    let mut img = Builder::default().build();
    *img.at(1, 80) ^= 0x01;
    match img.locate(guid(0xC3)) {
        Err(GptError::HeaderCrc { .. }) => {}
        other => panic!("a corrupt header parsed as {other:?}"),
    }
}

#[test]
fn one_flipped_bit_in_the_entry_array() {
    let mut img = Builder::default().build();
    *img.at(ARRAY_LBA, 32) ^= 0x01;
    match img.locate(guid(0xC3)) {
        Err(GptError::EntryArrayCrc { .. }) => {}
        other => panic!("a corrupt entry array parsed as {other:?}"),
    }
}

/// The CRC covers `entry_count * entry_bytes` bytes and not a byte more, so a
/// change past the end of the array must not be read as corruption — and must
/// not be read as an entry either.
#[test]
fn the_array_ends_where_the_header_says() {
    let mut img = Builder { entry_count: 3, ..Default::default() }.build();
    // Entry 3 is now outside the array. It is still on the disk.
    assert_eq!(img.locate(guid(0xD4)), Err(GptError::NotFound { used_entries: 3 }));
    *img.at(ARRAY_LBA, 3 * 128 + 1) ^= 0xFF;
    let found = img.locate(guid(0xC3)).expect("still parses");
    assert_eq!(found.used_entries, 3);
}

#[test]
fn entry_size_must_be_a_power_of_two_multiple_of_128_that_fits_a_block() {
    for bad in [0u32, 1, 64, 127, 192, 1024, u32::MAX] {
        let mut img = Builder { entry_bytes: bad, ..Default::default() }.build();
        assert_eq!(img.locate(guid(0xC3)), Err(GptError::EntrySize(bad)), "entry size {bad}");
    }
}

#[test]
fn a_billion_entries_is_refused_not_read() {
    for bad in [u32::MAX, 1_000_000_000, 1025] {
        let mut img = Builder { entry_count: bad, ..Default::default() }.build();
        assert_eq!(
            img.locate(guid(0xC3)),
            Err(GptError::EntryArrayTooBig { entries: bad, entry_size: ENTRY }),
            "entry count {bad}"
        );
    }
}

/// 128 KiB exactly is the ceiling, and it is a ceiling on the array, not on
/// this disk: the array would have to fit before the first usable block too.
#[test]
fn the_array_ceiling_is_where_it_says_it_is() {
    let at_ceiling = (toyos_gpt::MAX_ENTRY_ARRAY_BYTES / ENTRY as u64) as u32;
    let over = Builder { entry_count: at_ceiling + 1, ..Default::default() }.build();
    let mut over = over;
    assert!(matches!(over.locate(guid(0xC3)), Err(GptError::EntryArrayTooBig { .. })));

    let mut ok = Builder {
        entry_count: at_ceiling,
        first_usable: 2 + toyos_gpt::MAX_ENTRY_ARRAY_BYTES / LBA as u64,
        ..Default::default()
    }
    .build();
    // Not TooBig: it is refused, if at all, for a different reason.
    assert!(!matches!(ok.locate(guid(0xC3)), Err(GptError::EntryArrayTooBig { .. })));
}

#[test]
fn zero_entries_is_refused() {
    let mut img = Builder { entry_count: 0, ..Default::default() }.build();
    assert_eq!(
        img.locate(guid(0xC3)),
        Err(GptError::EntryArrayTooBig { entries: 0, entry_size: ENTRY })
    );
}

#[test]
fn the_array_may_not_sit_on_the_header_or_past_the_usable_range() {
    for bad_lba in [0u64, 1] {
        let mut img = Builder { entry_array_lba: bad_lba, ..Default::default() }.build();
        assert!(
            matches!(img.locate(guid(0xC3)), Err(GptError::EntryArrayMisplaced { .. })),
            "array at LBA {bad_lba}"
        );
    }
    // Starts legally, ends past the first usable block.
    let mut img = Builder { first_usable: 20, ..Default::default() }.build();
    assert!(matches!(img.locate(guid(0xC3)), Err(GptError::EntryArrayMisplaced { .. })));
}

#[test]
fn an_array_lba_near_the_top_of_the_range_does_not_wrap() {
    let mut img = Builder { entry_array_lba: u64::MAX - 1, ..Default::default() }.build();
    assert!(matches!(img.locate(guid(0xC3)), Err(GptError::EntryArrayMisplaced { .. })));
}

#[test]
fn usable_range_must_be_a_range_inside_the_device() {
    let mut inverted = Builder { first_usable: 500, last_usable: 100, ..Default::default() }.build();
    assert_eq!(
        inverted.locate(guid(0xC3)),
        Err(GptError::UsableRange { first: 500, last: 100 })
    );

    let mut past_end = Builder { last_usable: DISK_LBAS, ..Default::default() }.build();
    assert_eq!(
        past_end.locate(guid(0xC3)),
        Err(GptError::UsableRange { first: FIRST_USABLE, last: DISK_LBAS })
    );

    let mut over_the_table = Builder { first_usable: 1, ..Default::default() }.build();
    assert_eq!(
        over_the_table.locate(guid(0xC3)),
        Err(GptError::UsableRange { first: 1, last: DISK_LBAS - FIRST_USABLE })
    );
}

#[test]
fn a_partition_outside_the_disk_is_refused() {
    let mut b = Builder::default();
    b.entries[2] = Entry::new(TYPE_ESP, guid(0xC3), 200, u64::MAX);
    let mut img = b.build();
    assert_eq!(
        img.locate(guid(0xC3)),
        Err(GptError::PartitionRange { first: 200, last: u64::MAX })
    );
}

#[test]
fn a_backwards_partition_is_refused() {
    let mut b = Builder::default();
    b.entries[2] = Entry::new(TYPE_ESP, guid(0xC3), 900, 800);
    let mut img = b.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::PartitionRange { first: 900, last: 800 }));
}

/// A partition sitting on top of the entry array is the interesting shape:
/// the caller's next act is to write to it.
#[test]
fn a_partition_over_the_table_is_refused() {
    let mut b = Builder::default();
    b.entries[2] = Entry::new(TYPE_ESP, guid(0xC3), 3, 299);
    let mut img = b.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::PartitionRange { first: 3, last: 299 }));
}

#[test]
fn an_overlapping_neighbour_is_refused() {
    let mut b = Builder::default();
    b.entries[3] = Entry::new(TYPE_OTHER, guid(0xD4), 250, 400);
    let mut img = b.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::PartitionOverlap { index: 3 }));

    // And the overlap is found when it comes *before* the match too, which is
    // the case a single streaming pass would miss.
    let mut b = Builder::default();
    b.entries[0] = Entry::new(TYPE_OTHER, guid(0xA1), 40, 250);
    let mut img = b.build();
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::PartitionOverlap { index: 0 }));
}

/// UEFI puts the second copy at the end of the device precisely so a torn
/// write to the front is recoverable. A primary whose signature is gone
/// never became a checked table, so `locate` must retry the backup rather
/// than refuse a disk that is otherwise fine.
#[test]
fn a_damaged_primary_falls_back_to_a_good_backup() {
    let mut img = Builder { backup: true, ..Default::default() }.build();
    *img.at(1, 0) = b'X';
    let found = img.locate(guid(0xC3)).expect("the backup carries this GUID");
    assert_eq!(found.partition.index, 2);
    assert_eq!(found.partition.first_lba, 200);
    assert_eq!(found.partition.last_lba, 299);
    assert_eq!(found.used_entries, 4);
    assert_eq!(found.disk_guid, guid(0x5D));
}

/// Both copies gone must be a named refusal, not a panic and not a made-up
/// answer. The fallback's own failure is discarded in favour of the
/// primary's, so the caller learns why the primary — the copy that matters —
/// was unreadable.
#[test]
fn both_copies_damaged_is_refused_by_name() {
    let mut img = Builder { backup: true, ..Default::default() }.build();
    *img.at(1, 0) = b'X';
    *img.at(DISK_LBAS - 1, 0) = b'X';
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::NoHeader));
}

/// A primary that parsed cleanly and simply does not contain the target is
/// never compared against the backup — a CRC-verified table is trusted
/// alone, so a `NotFound` is not in the set of errors this falls back on.
#[test]
fn a_valid_primary_that_lacks_the_guid_is_not_retried_against_the_backup() {
    let mut img = Builder { backup: true, ..Default::default() }.build();
    assert_eq!(img.locate(guid(0xEE)), Err(GptError::NotFound { used_entries: 4 }));
}

#[test]
fn a_read_that_does_not_happen_is_an_error() {
    for lba in [0u64, 1, ARRAY_LBA] {
        let mut img = Builder::default().build();
        img.fail_at = Some(lba);
        assert_eq!(img.locate(guid(0xC3)), Err(GptError::ReadFailed(lba)));
    }
}

#[test]
fn block_sizes_outside_the_supported_range() {
    for bad in [0u32, 128, 500, 8192, u32::MAX] {
        let mut img = Builder::default().build();
        img.lba_bytes = bad;
        assert_eq!(img.locate(guid(0xC3)), Err(GptError::UnsupportedLbaSize(bad)));
    }
}

#[test]
fn a_four_kibibyte_block_device_parses() {
    let mut img = Builder {
        lba_bytes: 4096,
        lba_count: 512,
        entry_array_lba: 2,
        first_usable: 6,
        last_usable: 500,
        entries: vec![
            Entry::new(TYPE_OTHER, guid(0x11), 10, 20),
            Entry::new(TYPE_ESP, guid(0x22), 21, 400),
        ],
        ..Default::default()
    }
    .build();
    let found = img.locate(guid(0x22)).expect("present");
    assert_eq!((found.partition.index, found.partition.first_lba), (1, 21));
    assert_eq!(found.used_entries, 2);
}

#[test]
fn a_device_with_no_room_for_a_table() {
    let mut img = Builder::default().build();
    img.lba_count = 2;
    assert_eq!(img.locate(guid(0xC3)), Err(GptError::DeviceTooSmall(2)));
}

/// Nothing on any path may panic, index out of bounds, or overflow, whatever
/// the disk says. One flipped byte at a time over every byte the parser can
/// reach in the header and the table — 17,920 parses, each of which must
/// simply *return*.
#[test]
fn no_byte_of_the_table_can_panic_the_parser() {
    let mut img = Builder::default().build();
    let reach = (ARRAY_LBA as usize + 32) * LBA as usize;
    let mut located = 0;
    for at in 0..reach {
        for mask in [0x01u8, 0xFF] {
            img.bytes[at] ^= mask;
            if img.locate(guid(0xC3)).is_ok() {
                located += 1;
            }
            img.bytes[at] ^= mask;
        }
    }
    // The sweep has to be able to fail, and a sweep that refused everything
    // would prove nothing about the parser: bytes the table does not read
    // (padding inside entries, the unused tail of the array block) leave a
    // valid table behind, so some of these must still find the partition.
    assert!(located > 0, "every single-byte change broke the table");
    assert!(img.locate(guid(0xC3)).is_ok(), "the sweep did not put the table back");
}
