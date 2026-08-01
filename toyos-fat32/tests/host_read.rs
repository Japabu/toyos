//! Direction one of the gate: macOS writes the volume, this crate reads it.
//!
//! Everything asserted here has ground truth outside this repository — the
//! bytes came from `newfs_msdos` and the macOS `msdosfs` driver, so a reader
//! that agrees with itself cannot pass.

mod common;

use std::fs;

use common::{pattern, read_all, sorted_walk, Image};
use toyos_fat32::{Error, Fat32};

/// One cluster on the 512-byte-cluster image, so the sizes below are stated in
/// clusters rather than in a number that means nothing.
const CLUSTER: usize = 512;

struct Fixture {
    image: Image,
    files: Vec<(String, Vec<u8>)>,
}

/// A volume with one of everything that has ever broken a FAT reader.
fn populated(sectors_per_cluster: u32, cluster_bytes: usize) -> Fixture {
    let files: Vec<(String, Vec<u8>)> = vec![
        ("UPPER.TXT".into(), b"a pure 8.3 name needs no long entry".to_vec()),
        ("hello.txt".into(), b"lowercase, which 8.3 cannot hold".to_vec()),
        ("A very long file name that needs several LFN entries.txt".into(), b"lfn".to_vec()),
        ("\u{fc}n\u{ef}c\u{f6}d\u{e9}.txt".into(), "non-ascii".as_bytes().to_vec()),
        ("empty.bin".into(), Vec::new()),
        ("exactly-one-cluster.bin".into(), pattern(cluster_bytes, 1)),
        ("one-byte-over.bin".into(), pattern(cluster_bytes + 1, 2)),
        ("sub/one.txt".into(), b"in a subdirectory".to_vec()),
        ("sub/nested/deep.bin".into(), pattern(300 * 1024, 3)),
        ("sub/nested/A Second Long Name.dat".into(), pattern(3 * cluster_bytes, 4)),
    ];

    let image = Image::new("read", 64 * 1024 * 1024, sectors_per_cluster);
    image.with_mount(|mount| {
        for (name, data) in &files {
            let path = mount.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(&path, data).expect("write");
        }
    });
    Fixture { image, files }
}

#[test]
fn walk_sees_exactly_what_the_host_wrote() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");

    let mut expected: Vec<(String, u64)> =
        fx.files.iter().map(|(n, d)| (n.clone(), d.len() as u64)).collect();
    expected.sort();
    assert_eq!(sorted_walk(&mut fs), expected);
}

/// The one macOS writes as a short entry with the NT lowercase bits set.
/// Ignoring those bits reports `HELLO.TXT`, which is a different file name.
#[test]
fn lowercase_short_names_survive() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");
    let names: Vec<String> = fs.read_dir("", 64).expect("read_dir").into_iter().map(|e| e.name).collect();
    assert!(names.contains(&String::from("hello.txt")), "{names:?}");
    assert!(names.contains(&String::from("UPPER.TXT")), "{names:?}");
}

#[test]
fn every_file_reads_back_byte_for_byte() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");
    for (name, data) in &fx.files {
        assert_eq!(&read_all(&mut fs, name), data, "contents of {name}");
        assert_eq!(fs.metadata(name).expect("metadata").len, data.len() as u64, "size of {name}");
    }
}

#[test]
fn reads_land_correctly_at_every_offset_and_length() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");
    let name = "sub/nested/deep.bin";
    let truth = &fx.files.iter().find(|(n, _)| n == name).expect("fixture").1;
    let mut f = fs.open(name).expect("open");

    // Offsets and lengths chosen around the cluster boundary, which is where
    // a read that computes its run wrong stops being wrong by a whole cluster
    // and starts being wrong by a byte.
    for &offset in &[0u64, 1, 511, 512, 513, 1023, 1024, 65_536, 300 * 1024 - 1] {
        for &len in &[1usize, 7, 511, 512, 513, 4096, 100_000] {
            let mut buf = vec![0u8; len];
            let n = fs.read(&mut f, offset, &mut buf).expect("read");
            let want = &truth[(offset as usize).min(truth.len())..]
                [..n.min(truth.len() - (offset as usize).min(truth.len()))];
            assert_eq!(&buf[..n], want, "offset {offset} len {len}");
            let remaining = truth.len().saturating_sub(offset as usize);
            assert_eq!(n, len.min(remaining), "short read at offset {offset} len {len}");
        }
    }
}

#[test]
fn subdirectories_list_independently() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");

    let mut top: Vec<String> = fs.read_dir("sub", 64).expect("read_dir").into_iter().map(|e| e.name).collect();
    top.sort();
    assert_eq!(top, vec![String::from("nested"), String::from("one.txt")]);

    let nested = fs.read_dir("sub/nested", 64).expect("read_dir");
    assert_eq!(nested.len(), 2);
    assert!(nested.iter().any(|e| e.name == "deep.bin" && e.len == 300 * 1024));

    assert!(fs.metadata("sub").expect("metadata").is_dir);
    assert!(!fs.metadata("sub/one.txt").expect("metadata").is_dir);
}

/// The volume label lives in the root directory as an entry with a name field
/// and no file behind it. A reader that treats it as a file invents one.
#[test]
fn the_volume_label_is_not_a_file() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");
    let names: Vec<String> = fs.read_dir("", 64).expect("read_dir").into_iter().map(|e| e.name).collect();
    assert!(!names.iter().any(|n| n.starts_with("TF")), "label leaked into the listing: {names:?}");
}

#[test]
fn missing_paths_are_not_found_rather_than_anything_else() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");
    assert_eq!(fs.open("nope.txt").unwrap_err(), Error::NotFound);
    assert_eq!(fs.open("sub/nope.txt").unwrap_err(), Error::NotFound);
    assert_eq!(fs.metadata("sub/one.txt/deeper").unwrap_err(), Error::NotADirectory);
    assert_eq!(fs.open("sub").unwrap_err(), Error::IsADirectory);
    assert!(!fs.exists("nope.txt").expect("exists"));
    assert!(fs.exists("sub/one.txt").expect("exists"));
}

/// Names are matched case-insensitively, which is what firmware does when it
/// looks for `EFI/BOOT/BOOTX64.EFI`.
#[test]
fn lookup_ignores_case() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");
    assert!(fs.exists("HELLO.TXT").expect("exists"));
    assert!(fs.exists("upper.txt").expect("exists"));
    assert!(fs.exists("SUB/NESTED/DEEP.BIN").expect("exists"));
    assert!(fs.exists("a VERY long file name that needs several LFN entries.TXT").expect("exists"));
}

#[test]
fn extents_cover_the_file_and_point_at_its_bytes() {
    let fx = populated(1, CLUSTER);
    let mut fs = Fat32::mount(fx.image.device()).expect("mount");
    let name = "sub/nested/deep.bin";
    let truth = &fx.files.iter().find(|(n, _)| n == name).expect("fixture").1;

    let extents = fs.extents(name, 4096).expect("extents");
    assert_eq!(extents.iter().map(|e| e.len).sum::<u64>(), truth.len() as u64);
    // macOS lays a fresh file down contiguously, so this is one extent. That
    // is worth recording: the coalescing is doing something, not returning a
    // run per cluster.
    assert_eq!(extents.len(), 1, "{extents:?}");

    // Read the extents straight off the device and reassemble the file, which
    // is what a demand-paging backing does.
    let mut assembled = Vec::new();
    for e in &extents {
        let mut buf = vec![0u8; e.len as usize];
        use toyos_fat32::BlockAccess;
        fs.device().read_at(e.offset, &mut buf).expect("device read");
        assembled.extend_from_slice(&buf);
    }
    assert_eq!(&assembled, truth);

    assert_eq!(fs.extents(name, 0).unwrap_err(), Error::LimitExceeded);
    assert!(fs.extents("empty.bin", 4096).expect("extents").is_empty());
}

/// A 4 KiB cluster volume is a different code path only in arithmetic, which
/// is exactly the kind of difference that hides a bug.
#[test]
fn a_four_kib_cluster_volume_reads_the_same() {
    let files: Vec<(String, Vec<u8>)> = vec![
        ("boot.cfg".into(), b"small".to_vec()),
        ("A Long Name Here.bin".into(), pattern(40_000, 7)),
        ("dir/deep/file.dat".into(), pattern(4096, 8)),
    ];
    // 65_525 clusters of 4 KiB is the floor for FAT32, so this volume cannot
    // be smaller. It is sparse, so it costs what is written to it.
    let image = Image::new("read4k", 300 * 1024 * 1024, 8);
    image.with_mount(|mount| {
        for (name, data) in &files {
            let path = mount.join(name);
            if let Some(p) = path.parent() {
                fs::create_dir_all(p).expect("mkdir");
            }
            fs::write(&path, data).expect("write");
        }
    });

    let mut fs = Fat32::mount(image.device()).expect("mount");
    assert_eq!(fs.geometry().bytes_per_cluster(), 4096);
    for (name, data) in &files {
        assert_eq!(&read_all(&mut fs, name), data, "{name}");
    }
    let mut got = sorted_walk(&mut fs);
    got.sort();
    let mut want: Vec<(String, u64)> = files.iter().map(|(n, d)| (n.clone(), d.len() as u64)).collect();
    want.sort();
    assert_eq!(got, want);
}
