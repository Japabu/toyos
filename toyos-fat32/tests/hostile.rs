//! A volume is untrusted input, and this is where that is proved rather than
//! claimed.
//!
//! Every test here starts from a real volume — `newfs_msdos` made it, macOS
//! populated it, the checker passed it — and then breaks it on purpose. The
//! assertion in all of them is the same: a typed error, and never a panic, a
//! hang, or an allocation the volume chose the size of. A `#[test]` that
//! returns has already proved the no-panic half, because a panic in a test is
//! a failure; the explicit assertions are about *which* error and about the
//! operation having actually happened.
//!
//! The corpus is held sparsely (see `common::SparseDevice`) because a valid
//! FAT32 volume cannot have fewer than 65,525 clusters, and materialising one
//! per test would be gigabytes of zeroes.

mod common;

use std::sync::OnceLock;

use common::{pattern, Image, SparseDevice};
use toyos_fat32::{BlockAccess, Error, Fat32, FatTime, IoError, MAX_DIR_ENTRIES};

/// Enough of the volume to hold the reserved sectors, both FATs, and the first
/// thousand data clusters. Everything past it reads as zeroes, which is what
/// it is.
const PREFIX_BYTES: usize = 3072 * 512;

fn corpus() -> &'static (Vec<u8>, u64) {
    static ONCE: OnceLock<(Vec<u8>, u64)> = OnceLock::new();
    ONCE.get_or_init(|| {
        let image = Image::new("hostile", 64 * 1024 * 1024, 1);
        image.with_mount(|mount| {
            std::fs::create_dir_all(mount.join("sub/deeper")).expect("mkdir");
            std::fs::write(mount.join("plain.txt"), b"a short file").expect("write");
            std::fs::write(mount.join("A Long Name For Entries.bin"), pattern(20_000, 5)).expect("write");
            std::fs::write(mount.join("sub/inner.dat"), pattern(4000, 6)).expect("write");
            std::fs::write(mount.join("sub/deeper/leaf.txt"), b"leaf").expect("write");
        });
        image.fsck();
        (image.bytes(PREFIX_BYTES), image.size())
    })
}

fn pristine() -> SparseDevice {
    let (prefix, cap) = corpus();
    SparseDevice::from_prefix(prefix, *cap)
}

/// Geometry read straight off the corpus, so the tests below can aim at a FAT
/// entry or a cluster without re-deriving what the crate already computes.
struct Layout {
    bps: u64,
    spc: u64,
    reserved: u64,
    num_fats: u64,
    fat_sectors: u64,
    first_data: u64,
    root: u32,
}

fn layout() -> Layout {
    let dev = pristine();
    let b = dev.peek(0, 512);
    let u16at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
    let u32at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let bps = u16at(11);
    let reserved = u16at(14);
    let num_fats = b[16] as u64;
    let fat_sectors = u32at(36) as u64;
    Layout {
        bps,
        spc: b[13] as u64,
        reserved,
        num_fats,
        fat_sectors,
        first_data: reserved + num_fats * fat_sectors,
        root: u32at(44),
    }
}

impl Layout {
    fn fat_entry(&self, fat: u64, cluster: u32) -> u64 {
        (self.reserved + fat * self.fat_sectors) * self.bps + cluster as u64 * 4
    }

    fn cluster(&self, cluster: u32) -> u64 {
        (self.first_data + (cluster as u64 - 2) * self.spc) * self.bps
    }

    /// Point `cluster`'s entry at `value` in every FAT, so the two copies do
    /// not disagree about what is being tested.
    fn set_chain(&self, dev: &mut SparseDevice, cluster: u32, value: u32) {
        for fat in 0..self.num_fats {
            dev.poke(self.fat_entry(fat, cluster), &value.to_le_bytes());
        }
    }
}

/// Every operation the crate offers, run for effect and not for a result.
///
/// The point is that none of them panics, hangs, or allocates by a number the
/// volume supplied. Errors are expected and ignored; a panic fails the test.
fn exercise(dev: SparseDevice) {
    let Ok(mut fs) = Fat32::mount(dev) else { return };
    let t = FatTime::EPOCH;
    let _ = fs.free_bytes();
    let _ = fs.walk(2048);

    if let Ok(entries) = fs.read_dir("", 2048) {
        for e in entries.iter().take(32) {
            let _ = fs.metadata(&e.name);
            let _ = fs.read_dir(&e.name, 2048);
            let _ = fs.extents(&e.name, 64);
            if let Ok(mut f) = fs.open(&e.name) {
                let mut buf = vec![0u8; 4096];
                let _ = fs.read(&mut f, 0, &mut buf);
                let _ = fs.read(&mut f, 3000, &mut buf);
                let _ = fs.read(&mut f, u32::MAX as u64 - 10, &mut buf);
            }
        }
    }

    if let Ok(mut f) = fs.create("probe.tmp", t) {
        let _ = fs.write(&mut f, 0, &[1, 2, 3, 4]);
        let _ = fs.write(&mut f, 100_000, &[5, 6]);
        let _ = fs.set_len(&mut f, 10);
        let _ = fs.flush_meta(&mut f, t);
    }
    let _ = fs.create_dir("probedir", t);
    let _ = fs.create_dir_all("a/b/c", t);
    let _ = fs.rename("probe.tmp", "probedir/moved.tmp");
    let _ = fs.remove("probedir/moved.tmp");
    let _ = fs.remove_dir("probedir");
    let _ = fs.sync();
}

// ------------------------------------------------------- the corpus itself

/// If this fails, every test below is testing against a fantasy.
#[test]
fn the_corpus_is_a_volume_we_can_read() {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let mut names: Vec<String> = fs.walk(1024).expect("walk").into_iter().map(|(n, _)| n).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "A Long Name For Entries.bin",
            "plain.txt",
            "sub/deeper/leaf.txt",
            "sub/inner.dat",
        ]
    );
    assert_eq!(common::read_all(&mut fs, "plain.txt"), b"a short file");
    assert_eq!(common::read_all(&mut fs, "sub/inner.dat"), pattern(4000, 6));
}

// ------------------------------------------------------------- boot sector

#[test]
fn a_bad_signature_is_not_a_volume() {
    let mut dev = pristine();
    dev.poke(510, &[0x00, 0x00]);
    assert_eq!(common::mount_err(dev), Error::NotFat32);
}

#[test]
fn every_illegal_bpb_field_is_refused() {
    // (offset, bytes, what it breaks)
    let cases: &[(u64, &[u8], &str)] = &[
        (11, &[0, 0], "bytes per sector zero"),
        (11, &[0x09, 0x02], "bytes per sector 521, not a power of two"),
        (11, &[0x00, 0x20], "bytes per sector 8192, above the legal set"),
        (13, &[0], "sectors per cluster zero"),
        (13, &[3], "sectors per cluster not a power of two"),
        (13, &[255], "sectors per cluster above 128"),
        (14, &[0, 0], "no reserved sectors, so the FAT overlaps the boot sector"),
        (16, &[0], "no FATs"),
        (16, &[200], "two hundred FATs"),
        (17, &[16, 0], "a FAT16 root entry count"),
        (19, &[0x10, 0x00], "a FAT16 total sector count"),
        (22, &[0x10, 0x00], "a FAT16 FAT size"),
        (32, &[0, 0, 0, 0], "no sectors at all"),
        (36, &[0, 0, 0, 0], "a zero-length FAT"),
        (40, &[0x8F, 0x00], "an active FAT index past the FAT count"),
        (42, &[1, 0], "a filesystem version this crate does not speak"),
        (44, &[0, 0, 0, 0], "root at cluster 0"),
        (44, &[1, 0, 0, 0], "root at cluster 1"),
        (44, &[0xFF, 0xFF, 0xFF, 0x0F], "root past the last cluster"),
    ];
    for (offset, bytes, why) in cases {
        let mut dev = pristine();
        dev.poke(*offset, bytes);
        assert_eq!(common::mount_err(dev), Error::NotFat32, "accepted {why}");
    }
}

/// The cluster count decides the FAT type. A volume with fewer than 65,525 is
/// FAT16 whatever its boot sector says, and reading its 16-bit FAT entries as
/// 32-bit ones would produce cluster numbers that are plausible and wrong.
#[test]
fn a_volume_too_small_for_fat32_is_refused() {
    let l = layout();
    let mut dev = pristine();
    let sectors = l.first_data + 60_000 * l.spc;
    dev.poke(32, &(sectors as u32).to_le_bytes());
    assert_eq!(common::mount_err(dev), Error::NotFat32);
}

/// A FAT with fewer entries than the volume has clusters would let a chain
/// walk read whatever lies past the FAT region as a cluster number.
#[test]
fn a_fat_too_small_for_its_clusters_is_refused() {
    let l = layout();
    let mut dev = pristine();
    dev.poke(36, &((l.fat_sectors / 4) as u32).to_le_bytes());
    assert_eq!(common::mount_err(dev), Error::NotFat32);
}

#[test]
fn a_volume_larger_than_its_device_is_truncated() {
    let (prefix, cap) = corpus();
    let dev = SparseDevice::from_prefix(prefix, cap / 2);
    assert_eq!(common::mount_err(dev), Error::Truncated);

    // And the boundary: one sector short of what the boot sector declares.
    let dev = SparseDevice::from_prefix(prefix, *cap);
    let total = u32::from_le_bytes(dev.peek(32, 4).try_into().expect("4 bytes")) as u64;
    let l = layout();
    let dev = SparseDevice::from_prefix(prefix, total * l.bps - 1);
    assert_eq!(common::mount_err(dev), Error::Truncated);
    let dev = SparseDevice::from_prefix(prefix, total * l.bps);
    assert!(Fat32::mount(dev).is_ok(), "exactly the declared size must mount");
}

/// A device that stops answering part way through. Nothing may panic on the
/// error it returns, and the error must reach the caller as `Io`.
#[test]
fn a_device_that_fails_mid_read_reports_it() {
    let (prefix, cap) = corpus();
    let mut dev = SparseDevice::from_prefix(prefix, *cap);
    dev.fail_reads_past = Some(256);
    assert_eq!(common::mount_err(dev), Error::Io);

    let l = layout();
    let mut dev = pristine();
    // Enough for the boot sector and FSInfo, not enough for the root cluster.
    dev.fail_reads_past = Some(l.first_data * l.bps);
    let mut fs = Fat32::mount(dev).expect("mount");
    assert_eq!(fs.walk(1024).unwrap_err(), Error::Io);
}

/// **A budget that expired is not a device that failed, and this crate must not
/// flatten the two.**
///
/// `IoError` grew a second variant on 2026-08-22 for one reason: the kernel's
/// implementor bounds an operation with `block::OPERATION`, and reaching that
/// bound is a statement about the caller's clock. Flattening it into
/// `Error::Io` is what made `/bin/logd` end a boot's log for a stick that was
/// answering
/// (`issues/boot-media/fsync-on-log-returns-other-under-a-loaded-host.md`).
/// The two `assert_ne!`s are the point of the test: `Error::Io` is exactly the
/// answer the collapsed version gives.
#[test]
fn a_budget_that_expired_is_not_a_device_that_failed() {
    let (prefix, cap) = corpus();
    let mut dev = SparseDevice::from_prefix(prefix, *cap);
    dev.fail_reads_past = Some(256);
    dev.refusal = IoError::BudgetExpired;
    let refused = common::mount_err(dev);
    assert_eq!(refused, Error::BudgetExpired);
    assert_ne!(refused, Error::Io);

    let l = layout();
    let mut dev = pristine();
    dev.fail_reads_past = Some(l.first_data * l.bps);
    dev.refusal = IoError::BudgetExpired;
    let mut fs = Fat32::mount(dev).expect("mount");
    assert_eq!(fs.walk(1024).unwrap_err(), Error::BudgetExpired);
}

/// The flush is the call `/bin/logd`'s durability claim rests on, so it is the
/// one that has to keep the distinction all the way up.
///
/// Both arms against the same device, so what separates them is the refusal
/// and nothing else.
#[test]
fn a_flush_says_which_of_the_two_refusals_it_was() {
    let mut dev = pristine();
    dev.flush_refuses = true;
    dev.refusal = IoError::Device;
    let mut fs = Fat32::mount(dev).expect("mount");
    assert_eq!(fs.sync().unwrap_err(), Error::Io);

    let mut dev = pristine();
    dev.flush_refuses = true;
    dev.refusal = IoError::BudgetExpired;
    let mut fs = Fat32::mount(dev).expect("mount");
    let refused = fs.sync().unwrap_err();
    assert_eq!(refused, Error::BudgetExpired);
    assert_ne!(refused, Error::Io);

    // And a device that flushes is still `Ok`, so the two arms above are about
    // the refusal rather than about `sync` never succeeding on this fake.
    let mut fs = Fat32::mount(pristine()).expect("mount");
    assert_eq!(fs.sync(), Ok(()));
}

// ----------------------------------------------------------- broken chains

#[test]
fn a_cluster_that_links_to_itself_is_refused() {
    let l = layout();
    let mut dev = pristine();
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let first = fs.extents("A Long Name For Entries.bin", 64).expect("extents")[0].offset;
    let cluster = ((first / l.bps - l.first_data) / l.spc + 2) as u32;
    drop(fs);

    l.set_chain(&mut dev, cluster, cluster);
    let mut fs = Fat32::mount(dev).expect("mount");
    let mut f = fs.open("A Long Name For Entries.bin").expect("open");
    let mut buf = vec![0u8; 20_000];
    assert_eq!(fs.read(&mut f, 0, &mut buf).unwrap_err(), Error::CorruptChain);
}

/// A longer cycle is bounded rather than detected — see `Fat32::advance`. What
/// must hold is that the read terminates and returns no more than the file's
/// declared size, which is the property that keeps it from being a hang.
#[test]
fn a_longer_cycle_is_bounded_rather_than_endless() {
    let l = layout();
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let ext = fs.extents("A Long Name For Entries.bin", 64).expect("extents");
    let first = ((ext[0].offset / l.bps - l.first_data) / l.spc + 2) as u32;
    drop(fs);

    let mut dev = pristine();
    // …→ first+3 → first, a four-cluster ring under a 20,000-byte file.
    l.set_chain(&mut dev, first + 3, first);
    let mut fs = Fat32::mount(dev).expect("mount");
    let mut f = fs.open("A Long Name For Entries.bin").expect("open");
    let mut buf = vec![0u8; 20_000];
    let n = fs.read(&mut f, 0, &mut buf).expect("read");
    assert_eq!(n, 20_000, "the size field still bounds the read");
}

#[test]
fn a_chain_link_outside_the_volume_is_refused() {
    let l = layout();
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let ext = fs.extents("A Long Name For Entries.bin", 64).expect("extents");
    let first = ((ext[0].offset / l.bps - l.first_data) / l.spc + 2) as u32;
    drop(fs);

    for bad in [0u32, 1, 0x0FFF_FFF7, 0x0F00_0000] {
        let mut dev = pristine();
        l.set_chain(&mut dev, first, bad);
        let mut fs = Fat32::mount(dev).expect("mount");
        let mut f = fs.open("A Long Name For Entries.bin").expect("open");
        let mut buf = vec![0u8; 20_000];
        assert_eq!(
            fs.read(&mut f, 0, &mut buf).unwrap_err(),
            Error::CorruptChain,
            "link value {bad:#x} was followed"
        );
    }
}

/// A directory entry whose first cluster is outside the volume must be caught
/// before anything computes an offset from it.
#[test]
fn a_directory_entry_pointing_outside_the_volume_is_refused() {
    let l = layout();
    let mut dev = pristine();
    // `sub` is the third entry written into the root; find it rather than
    // assume where it is.
    let root = l.cluster(l.root);
    let mut found = None;
    for i in 0..64u64 {
        let e = dev.peek(root + i * 32, 32);
        if e[0] != 0 && e[0] != 0xE5 && e[11] & 0x3F != 0x0F && e[11] & 0x10 != 0 {
            found = Some(root + i * 32);
            break;
        }
    }
    let at = found.expect("no subdirectory entry in the root");
    dev.poke(at + 26, &[0xFF, 0xFF]);
    dev.poke(at + 20, &[0xFF, 0x0F]);

    let mut fs = Fat32::mount(dev).expect("mount");
    assert_eq!(fs.walk(1024).unwrap_err(), Error::CorruptDirectory);
}

/// One cluster of a directory, packed with entries and no end marker, so a
/// scan is forced onto the next cluster of the chain.
///
/// Without this the crafted chain is never followed at all: a zero first byte
/// ends a directory by definition, and the corpus's own clusters are mostly
/// zeroes. Two of the tests below were green for exactly that reason before
/// this existed, which is the failure mode the whole file is about.
fn filled_cluster(bytes_per_cluster: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes_per_cluster);
    while out.len() < bytes_per_cluster {
        let mut entry = [0u8; 32];
        entry[..11].copy_from_slice(b"FILLER  BIN");
        entry[11] = 0x20;
        entry[26..28].copy_from_slice(&2u16.to_le_bytes());
        entry[28..32].copy_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&entry);
    }
    out
}

/// A directory whose own chain is cyclic. Unlike a file's, this one has no
/// size field to bound it, so the bound has to come from the entry limit.
#[test]
fn a_cyclic_directory_chain_terminates() {
    let l = layout();
    let mut dev = pristine();
    let filler = filled_cluster((l.bps * l.spc) as usize);
    let root = l.root;
    dev.poke(l.cluster(root), &filler);
    dev.poke(l.cluster(root + 1), &filler);
    l.set_chain(&mut dev, root, root + 1);
    l.set_chain(&mut dev, root + 1, root);

    let mut fs = Fat32::mount(dev).expect("mount");
    let err = fs.walk(1024).unwrap_err();
    assert!(
        matches!(err, Error::CorruptDirectory | Error::CorruptChain | Error::LimitExceeded),
        "unexpected {err:?}"
    );
    // The other way in. Creating an entry both scans the directory for a
    // short-name collision and measures its chain, and either bound is enough
    // to refuse — the scan happens first, so that is the one seen here.
    let err = fs.create("new.txt", FatTime::EPOCH).map(|_| ()).unwrap_err();
    assert!(matches!(err, Error::CorruptDirectory | Error::CorruptChain), "unexpected {err:?}");
}

/// A subdirectory whose entry points back at an ancestor. The tree is a graph,
/// and a walk that follows it never stops.
#[test]
fn a_directory_tree_cycle_terminates() {
    let l = layout();
    let mut dev = pristine();
    let root = l.cluster(l.root);
    for i in 0..64u64 {
        let e = dev.peek(root + i * 32, 32);
        if e[0] != 0 && e[0] != 0xE5 && e[11] & 0x3F != 0x0F && e[11] & 0x10 != 0 {
            // Point the subdirectory at the root, making the tree a loop.
            dev.poke(root + i * 32 + 26, &(l.root as u16).to_le_bytes());
            dev.poke(root + i * 32 + 20, &((l.root >> 16) as u16).to_le_bytes());
            break;
        }
    }
    let mut fs = Fat32::mount(dev).expect("mount");
    // Either answer is sound: the visited set stops the descent, and the
    // budget stops a tree that is merely enormous.
    match fs.walk(1024) {
        Ok(files) => assert!(files.len() <= 1024),
        Err(e) => assert!(matches!(e, Error::LimitExceeded | Error::CorruptDirectory)),
    }
}

// ------------------------------------------------------------ absurd counts

/// A directory chain long enough to hold more entries than the crate will
/// walk. The refusal has to happen before the walk, not after it has built a
/// listing of them.
#[test]
fn a_directory_longer_than_the_entry_bound_is_refused() {
    let l = layout();
    let mut dev = pristine();
    let filler = filled_cluster((l.bps * l.spc) as usize);
    let per_cluster = (l.bps * l.spc / 32) as u32;
    // One cluster more than the bound admits, all of it packed with entries so
    // nothing stops the scan early. Acyclic, so the *count* is what refuses.
    let needed = MAX_DIR_ENTRIES / per_cluster + 4;
    let base = 4000u32;

    dev.poke(l.cluster(l.root), &filler);
    l.set_chain(&mut dev, l.root, base);
    for i in 0..needed {
        dev.poke(l.cluster(base + i), &filler);
        l.set_chain(&mut dev, base + i, base + i + 1);
    }
    dev.poke(l.cluster(base + needed), &filler);
    l.set_chain(&mut dev, base + needed, 0x0FFF_FFFF);

    let mut fs = Fat32::mount(dev).expect("mount");
    let err = fs.read_dir("", 1_000_000).unwrap_err();
    assert_eq!(err, Error::CorruptDirectory);
    // The listing bound is the caller's; this one is the crate's, and it has
    // to hold even when the caller offers a bound larger than it.
    assert_eq!(fs.walk(1_000_000).unwrap_err(), Error::CorruptDirectory);
}

/// The caller's own bound has to hold too, and it has to refuse rather than
/// hand back a listing that is short of the truth.
#[test]
fn the_caller_limit_refuses_rather_than_truncates() {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    assert_eq!(fs.walk(1).unwrap_err(), Error::LimitExceeded);
    assert_eq!(fs.read_dir("", 1).unwrap_err(), Error::LimitExceeded);
    assert_eq!(fs.read_dir("", 0).unwrap_err(), Error::LimitExceeded);
    assert!(fs.walk(64).is_ok());
}

// -------------------------------------------------------------- long names

/// A long-name run whose checksum does not match the short entry it precedes
/// belongs to a file that was deleted and partly overwritten. The short name
/// is still the truth, and the rest of the directory has to stay readable.
#[test]
fn a_long_name_with_a_wrong_checksum_falls_back_to_the_short_one() {
    let l = layout();
    let mut dev = pristine();
    let root = l.cluster(l.root);
    let mut patched = false;
    for i in 0..64u64 {
        let e = dev.peek(root + i * 32, 32);
        if e[0] != 0 && e[0] != 0xE5 && e[11] & 0x3F == 0x0F {
            dev.poke(root + i * 32 + 13, &[e[13] ^ 0xFF]);
            patched = true;
        }
    }
    assert!(patched, "the corpus has no long-name entries to break");

    let mut fs = Fat32::mount(dev).expect("mount");
    let names: Vec<String> = fs.read_dir("", 256).expect("read_dir").into_iter().map(|e| e.name).collect();
    assert!(names.iter().any(|n| n == "plain.txt"), "{names:?}");
    assert!(!names.iter().any(|n| n.contains(' ')), "a broken run was trusted: {names:?}");
    assert!(names.len() >= 3, "entries disappeared: {names:?}");
}

/// Ordinals that do not count down, ordinals of zero, ordinals past the
/// twenty entries the format allows.
#[test]
fn nonsense_long_name_ordinals_do_not_break_the_scan() {
    let l = layout();
    for ord in [0u8, 0x3F, 0x7F, 0xFF, 0x41, 0x05] {
        let mut dev = pristine();
        let root = l.cluster(l.root);
        for i in 0..64u64 {
            let e = dev.peek(root + i * 32, 32);
            if e[0] != 0 && e[0] != 0xE5 && e[11] & 0x3F == 0x0F {
                dev.poke(root + i * 32, &[ord]);
            }
        }
        let mut fs = Fat32::mount(dev).expect("mount");
        let entries = fs.read_dir("", 256).expect("read_dir");
        assert!(entries.iter().any(|e| e.name == "plain.txt"), "ordinal {ord:#x}");
    }
}

/// Every unit of a long name set to a lone surrogate, which is not encodable
/// as UTF-8 and is what a decoder that assumed well-formed UTF-16 would panic
/// on.
#[test]
fn lone_surrogates_in_a_long_name_produce_a_name() {
    let l = layout();
    let mut dev = pristine();
    let root = l.cluster(l.root);
    for i in 0..64u64 {
        let e = dev.peek(root + i * 32, 32);
        if e[0] != 0 && e[0] != 0xE5 && e[11] & 0x3F == 0x0F {
            for &(off, count) in &[(1usize, 5usize), (14, 6), (28, 2)] {
                for k in 0..count {
                    dev.poke(root + i * 32 + (off + k * 2) as u64, &0xD800u16.to_le_bytes());
                }
            }
        }
    }
    let mut fs = Fat32::mount(dev).expect("mount");
    let entries = fs.read_dir("", 256).expect("read_dir");
    assert!(!entries.is_empty());
}

// ------------------------------------------------------------------- fuzz

/// xorshift64*, so a red run is reproducible from its seed and the generator
/// is not the thing under test.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Random damage to the metadata region — boot sector, FSInfo, both FATs, and
/// the first data clusters — followed by every operation the crate offers.
///
/// This is a liveness run and says so: the assertion is that the process is
/// still standing, which covers the panic, the hang and the unbounded
/// allocation together. The typed-error tests above are what say the crate
/// gets the right *answer*; this is what says there is always an answer.
#[test]
fn random_damage_to_the_metadata_never_panics() {
    let l = layout();
    let metadata_bytes = (l.first_data + 64 * l.spc) * l.bps;
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for _ in 0..200 {
        let mut dev = pristine();
        let mutations = 1 + rng.below(24);
        for _ in 0..mutations {
            let at = rng.below(metadata_bytes);
            let byte = (rng.next() >> 33) as u8;
            dev.poke(at, &[byte]);
        }
        exercise(dev);
    }
}

/// Damage aimed only at the boot sector, where a single byte decides how every
/// later offset is computed.
#[test]
fn every_boot_sector_byte_can_be_anything() {
    for offset in 0..512u64 {
        for value in [0x00u8, 0x01, 0x7F, 0x80, 0xFF] {
            let mut dev = pristine();
            dev.poke(offset, &[value]);
            exercise(dev);
        }
    }
}

/// Damage aimed only at directory entries, which is where a name, a size and
/// a cluster number all come from the same 32 bytes.
#[test]
fn random_damage_to_directory_entries_never_panics() {
    let l = layout();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let root = l.cluster(l.root);
    let sub_start = l.cluster(l.root + 1);
    for _ in 0..200 {
        let mut dev = pristine();
        for _ in 0..(1 + rng.below(16)) {
            let base = if rng.next() & 1 == 0 { root } else { sub_start };
            let at = base + rng.below(l.bps * l.spc);
            dev.poke(at, &[(rng.next() >> 33) as u8]);
        }
        exercise(dev);
    }
}

/// Names a caller can ask for that FAT cannot hold. Rejected rather than
/// sanitised, so nobody ends up with a file under a name they did not choose.
#[test]
fn impossible_names_are_refused_not_mangled() {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let t = FatTime::EPOCH;
    let too_long: String = core::iter::repeat_n('x', 256).collect();
    for bad in ["", ".", "..", "a<b.txt", "a>b.txt", "a|b.txt", "a?b.txt", "trailing.", "trailing "] {
        let e = fs.create(bad, t).map(|_| ()).unwrap_err();
        assert!(matches!(e, Error::InvalidName | Error::AlreadyExists), "{bad:?} gave {e:?}");
    }
    assert_eq!(fs.create(&too_long, t).map(|_| ()).unwrap_err(), Error::InvalidName);
    assert_eq!(fs.create("", t).map(|_| ()).unwrap_err(), Error::InvalidName);
}

/// A handle that outlives what it named. Writing through it must not resize
/// whatever took its directory slot.
#[test]
fn a_stale_handle_is_refused() {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let t = FatTime::EPOCH;
    let mut f = fs.open("plain.txt").expect("open");
    fs.remove("plain.txt").expect("remove");
    assert_eq!(fs.flush_meta(&mut f, t).unwrap_err(), Error::NotFound);
}

/// FAT32's size field is 32 bits. A write past it has nowhere to record its
/// length, and must say so before it allocates anything.
#[test]
fn writes_past_the_format_limit_are_refused() {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let t = FatTime::EPOCH;
    let mut f = fs.create("huge.bin", t).expect("create");
    let before = fs.free_bytes().expect("free");
    assert_eq!(fs.write(&mut f, u32::MAX as u64, &[1, 2, 3]).unwrap_err(), Error::TooLarge);
    assert_eq!(fs.write(&mut f, u64::MAX, &[1]).unwrap_err(), Error::TooLarge);
    assert_eq!(fs.set_len(&mut f, 1 << 40).unwrap_err(), Error::TooLarge);
    assert_eq!(fs.free_bytes().expect("free"), before, "a refused write still allocated");
}

// ------------------------------------- the write path, after the 2026-08-01 audit
//
// Each of these was a working reproducer against the tree that audit walked.
// They are grouped because they have one shape: a value that came off the disk
// reached a device operation without the check that gives it meaning, because
// the check sat behind a condition — `is_dir || size > 0`, `steps > 0`, "the
// walk revisits". The `Cluster` type closes the first two; the other two are
// orderings and needed their own fix.

/// Write a raw 32-byte entry into the root's first free slot, and answer with
/// the offset it went to.
fn craft_root_entry(dev: &mut SparseDevice, l: &Layout, entry: &[u8; 32]) -> u64 {
    let root = l.cluster(l.root);
    for i in 0..(l.bps * l.spc / 32) {
        let at = root + i * 32;
        let existing = dev.peek(at, 32);
        if existing[0] == 0x00 || existing[0] == 0xE5 {
            dev.poke(at, entry);
            return at;
        }
    }
    panic!("no free slot in the root directory");
}

/// An entry naming a cluster that is not in the volume, with `size == 0`.
///
/// F1. The validity check ran only when the entry was a directory or had a
/// non-zero size, and `advance(c, 0)` never entered the loop that would have
/// caught it — so `write` computed a byte offset from the crafted number and
/// issued it. On a device larger than the volume (a partition with slack, or
/// an adapter reporting the whole device) the audit landed 18 bytes at
/// 274,877,906,944 of a 64 MiB volume and got `Ok(())` back.
#[test]
fn a_crafted_zero_size_entry_cannot_write_outside_the_volume() {
    let l = layout();
    let (prefix, _) = corpus();
    // Twice the volume, so an out-of-volume offset is inside the device and
    // the device cannot mask the bug by refusing.
    let volume = l.bps * u64::from(u32::from_le_bytes(
        SparseDevice::from_prefix(prefix, 1).peek(32, 4).try_into().expect("4 bytes"),
    ));
    let mut dev = SparseDevice::from_prefix(prefix, volume * 2);
    dev.volume_bytes = Some(volume);

    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(b"CRAFTED BIN");
    entry[11] = 0x20;
    entry[26..28].copy_from_slice(&0x0002u16.to_le_bytes());
    entry[20..22].copy_from_slice(&0x2000u16.to_le_bytes());
    craft_root_entry(&mut dev, &l, &entry);

    let mut fs = Fat32::mount(dev).expect("mount");
    assert_eq!(fs.open("CRAFTED.BIN").map(|_| ()).unwrap_err(), Error::CorruptDirectory);
    assert_eq!(fs.metadata("CRAFTED.BIN").unwrap_err(), Error::CorruptDirectory);
    assert_eq!(fs.extents("CRAFTED.BIN", 16).unwrap_err(), Error::CorruptDirectory);
    // A listing validates only what it follows, so the crafted entry is
    // *reported*: it is on the disk, and saying so computes no offset from it.
    // One bad entry making a whole directory unlistable would be a denial of
    // service on a volume that is otherwise fine.
    assert!(fs.walk(1024).expect("walk").iter().any(|(n, _)| n == "CRAFTED.BIN"));
    assert_eq!(fs.device().out_of_volume, 0, "the crate asked for bytes outside the volume");
}

/// F6, the other half of the same entry: it must still be deletable.
///
/// Refusing to resolve it is right for a read and wrong for a delete — an ESP
/// log rotation that wedges permanently on one crafted entry is a machine that
/// fills up and cannot be fixed in the field.
#[test]
fn a_crafted_entry_can_still_be_deleted() {
    let l = layout();
    let mut dev = pristine();
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(b"CRAFTED BIN");
    entry[11] = 0x20;
    entry[26..28].copy_from_slice(&0xFFFFu16.to_le_bytes());
    entry[20..22].copy_from_slice(&0x0FFFu16.to_le_bytes());
    craft_root_entry(&mut dev, &l, &entry);

    let mut fs = Fat32::mount(dev).expect("mount");
    assert!(fs.read_dir("", 256).expect("read_dir").iter().any(|e| e.name == "CRAFTED.BIN"));
    fs.remove("CRAFTED.BIN").expect("a crafted entry must still be removable");
    assert!(!fs.read_dir("", 256).expect("read_dir").iter().any(|e| e.name == "CRAFTED.BIN"));
    assert_eq!(fs.metadata("CRAFTED.BIN").unwrap_err(), Error::NotFound);
}

/// The head cluster of the corpus file that spans several clusters.
fn multi_cluster_head(l: &Layout) -> u32 {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let ext = fs.extents("A Long Name For Entries.bin", 64).expect("extents");
    ((ext[0].offset / l.bps - l.first_data) / l.spc + 2) as u32
}

/// F3. Truncating a chain that loops back through the cluster being kept.
///
/// `truncate_chain` writes the end-of-chain marker first, and the free walk
/// then arrives at that cluster, reads the marker, and exits normally — having
/// freed the one cluster the file still needs. The audit saw `Ok(())` with
/// every cluster of the truncated file free and the entry still naming the
/// first of them, which the next allocation turns into a cross-link.
#[test]
fn truncating_a_cyclic_chain_does_not_free_the_clusters_it_keeps() {
    let l = layout();
    let head = multi_cluster_head(&l);
    let mut dev = pristine();
    // head → head+1 → head+2 → head, a rho closing above the truncation point.
    l.set_chain(&mut dev, head + 2, head);

    let mut fs = Fat32::mount(dev).expect("mount");
    let mut f = fs.open("A Long Name For Entries.bin").expect("open");
    assert_eq!(fs.set_len(&mut f, 600).unwrap_err(), Error::CorruptChain);

    // The kept cluster must still be allocated. Reading it back through the
    // FAT is the only way to see this: the directory entry looks fine either
    // way, which is what made the original silent.
    let g = *fs.geometry();
    let mut link = [0u8; 4];
    let cluster = g.cluster(head).expect("head is in the volume");
    fs.device().read_at(g.fat_entry_offset(0, cluster), &mut link).expect("read link");
    assert_ne!(
        u32::from_le_bytes(link) & 0x0FFF_FFFF,
        0,
        "truncation freed the cluster it was keeping"
    );
}

/// F4. Deleting a file whose chain is cyclic.
///
/// The free walk detects the cycle, but only after freeing part of it, and the
/// `?` then skipped the erase — leaving a live directory entry naming free
/// clusters. Erasing first makes a failure leak instead, which `fsck`
/// reclaims.
#[test]
fn removing_a_cyclic_chain_never_leaves_an_entry_naming_free_clusters() {
    let l = layout();
    let head = multi_cluster_head(&l);
    let mut dev = pristine();
    l.set_chain(&mut dev, head + 2, head);

    let mut fs = Fat32::mount(dev).expect("mount");
    let result = fs.remove("A Long Name For Entries.bin");
    assert_eq!(
        fs.metadata("A Long Name For Entries.bin").unwrap_err(),
        Error::NotFound,
        "the entry survived a remove that reported {result:?}"
    );
}

/// F2. A handle to a file that was empty when it was opened.
///
/// The guard compared first clusters, and 0 is every unwritten file's first
/// cluster — so a slot freed and refilled by another *empty* file matched, and
/// the stale handle wrote its own size and cluster into the newcomer's entry.
/// `fsck_msdos` called the result clean. The existing
/// `a_stale_handle_is_refused` removes a 12-byte file, so it leaves through
/// the `is_free` branch and never reaches the comparison at all.
#[test]
fn a_stale_handle_over_a_reused_empty_slot_is_refused() {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let t = FatTime::EPOCH;

    let mut stale = fs.create("AAA.TXT", t).expect("create");
    fs.flush_meta(&mut stale, t).expect("flush");
    fs.remove("AAA.TXT").expect("remove");

    let mut newcomer = fs.create("BBB.TXT", t).expect("create");
    fs.flush_meta(&mut newcomer, t).expect("flush");

    assert_eq!(fs.write(&mut stale, 0, &[1u8; 4096]).unwrap_err(), Error::NotFound);
    assert_eq!(fs.flush_meta(&mut stale, t).unwrap_err(), Error::NotFound);
    assert_eq!(fs.metadata("BBB.TXT").expect("metadata").len, 0, "the newcomer was rewritten");
}

/// F5. Writing through a handle whose entry is gone.
///
/// It allocated real clusters, returned `Ok(())`, and nothing ever gave them
/// back — 128 orphaned clusters in the audit, on a volume whose only repair
/// tool is a host `fsck`. `write`'s own contract promises all-or-nothing, and
/// this write did not fail, so no rollback ran.
#[test]
fn a_write_through_a_dead_handle_allocates_nothing() {
    let mut fs = Fat32::mount(pristine()).expect("mount");
    let t = FatTime::EPOCH;
    let mut f = fs.create("DOOMED.BIN", t).expect("create");
    fs.flush_meta(&mut f, t).expect("flush");
    fs.remove("DOOMED.BIN").expect("remove");

    let before = fs.free_bytes().expect("free");
    assert_eq!(fs.write(&mut f, 0, &[7u8; 65_536]).unwrap_err(), Error::NotFound);
    assert_eq!(fs.set_len(&mut f, 65_536).unwrap_err(), Error::NotFound);
    assert_eq!(fs.free_bytes().expect("free"), before, "a dead handle took clusters with it");
}
