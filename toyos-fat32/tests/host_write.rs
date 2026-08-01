//! Direction two of the gate: this crate writes the volume, macOS judges it.
//!
//! A write that only this crate's own reader can read back certifies nothing,
//! so every test here ends with `fsck_msdos` finding no fault and a real mount
//! producing the exact bytes. The `fsck` gate is strict about what "no fault"
//! means — see [`common::Image::fsck`], because `fsck_msdos -n` exits 0 on
//! volumes it has just called dirty.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use common::{pattern, read_all, sorted_walk, write_new, Image};
use toyos_fat32::{Error, Fat32, FatTime};

/// 2024-06-01 12:34:56, so every entry this crate stamps is checkable rather
/// than whatever the clock said.
fn stamp() -> FatTime {
    FatTime::from_unix_secs(1_717_245_296)
}

fn image(name: &str) -> Image {
    Image::new(name, 64 * 1024 * 1024, 1)
}

/// Everything under `mount`, as path → bytes, with directories as empty
/// entries suffixed with `/`.
fn host_tree(mount: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in fs::read_dir(dir).expect("read_dir") {
            let e = e.expect("entry");
            let path = e.path();
            let rel = path.strip_prefix(root).expect("relative").to_string_lossy().into_owned();
            if e.file_type().expect("file_type").is_dir() {
                out.insert(format!("{rel}/"), Vec::new());
                walk(root, &path, out);
            } else {
                out.insert(rel, fs::read(&path).expect("read"));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(mount, mount, &mut out);
    out
}

#[test]
fn the_host_accepts_a_volume_we_populated() {
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("PLAIN.TXT", b"a pure 8.3 name".to_vec()),
        ("lowercase.txt", b"needs a long entry to keep its case".to_vec()),
        ("A Rather Long Name With Spaces.dat", pattern(5000, 11)),
        // Greek pi and an astral emoji: neither has a canonical decomposition,
        // so the name macOS reads back is the name we wrote. A character that
        // does have one — any Latin letter with a diaeresis — comes back
        // decomposed, because macOS normalises at the mount layer and not in
        // the directory entry. The emoji is also the surrogate-pair case: it
        // is two UTF-16 units in one long-name entry.
        ("\u{3c0}-constant.txt", "non-ascii long name".as_bytes().to_vec()),
        ("emoji-\u{1F600}.txt", "outside the basic plane".as_bytes().to_vec()),
        ("zero.bin", Vec::new()),
        ("dir/inside.txt", b"nested".to_vec()),
        ("dir/deeper/still.bin", pattern(20_000, 12)),
    ];

    let image = image("write");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        fs.create_dir("dir", stamp()).expect("mkdir dir");
        fs.create_dir("dir/deeper", stamp()).expect("mkdir deeper");
        for (name, data) in &files {
            write_new(&mut fs, name, data, stamp());
        }
        fs.sync().expect("sync");
    }

    image.fsck();

    let tree = image.with_mount(host_tree);
    for (name, data) in &files {
        let got = tree.get(*name).unwrap_or_else(|| panic!("{name} missing; host saw {:?}", tree.keys()));
        assert_eq!(got, data, "contents of {name}");
    }
    assert!(tree.contains_key("dir/"));
    assert!(tree.contains_key("dir/deeper/"));
    assert_eq!(tree.len(), files.len() + 2);
}

/// The timestamp reaches the host's idea of mtime. FAT stores local time with
/// no zone, and macOS reads it back in the machine's zone, so the two can
/// differ by hours — the day is what is checkable without inventing a zone.
#[test]
fn the_host_sees_the_timestamp_we_stamped() {
    let image = image("time");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        write_new(&mut fs, "dated.txt", b"x", stamp());
        fs.sync().expect("sync");
    }
    image.fsck();
    let secs = image.with_mount(|mount| {
        fs::metadata(mount.join("dated.txt"))
            .expect("stat")
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs()
    });
    let delta = secs.abs_diff(1_717_245_296);
    assert!(delta < 24 * 3600, "host read {secs}, we wrote 1717245296");
}

/// One cluster is 512 bytes here, so these lengths straddle the boundary a
/// naive write handles by writing the first cluster twice.
#[test]
fn files_crossing_cluster_boundaries_are_intact() {
    let image = image("boundary");
    let lengths = [1usize, 511, 512, 513, 1023, 1024, 1025, 4096, 8191];
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        for len in lengths {
            write_new(&mut fs, &format!("len{len}.bin"), &pattern(len, len as u64), stamp());
        }
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        for len in lengths {
            let got = fs::read(mount.join(format!("len{len}.bin"))).expect("read");
            assert_eq!(got, pattern(len, len as u64), "length {len}");
        }
    });
}

/// A directory cluster holds 16 entries here. Sixty files with long names need
/// far more than one, so this is the test that the directory chain extends and
/// stays walkable across the join.
#[test]
fn a_directory_grows_into_further_clusters() {
    let image = image("dirgrow");
    let names: Vec<String> = (0..60).map(|i| format!("A Long Enough Name To Need Entries {i:03}.log")).collect();
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        fs.create_dir("logs", stamp()).expect("mkdir");
        for (i, name) in names.iter().enumerate() {
            write_new(&mut fs, &format!("logs/{name}"), &pattern(100, i as u64 + 1), stamp());
        }
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        let mut got: Vec<String> = fs::read_dir(mount.join("logs"))
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        got.sort();
        let mut want = names.clone();
        want.sort();
        assert_eq!(got, want);
        for (i, name) in names.iter().enumerate() {
            assert_eq!(fs::read(mount.join("logs").join(name)).expect("read"), pattern(100, i as u64 + 1));
        }
    });
}

/// Short names must stay unique inside a directory. These all reduce to the
/// same 8.3 basis, which is exactly the case a `~1` tail alone cannot scale
/// past.
#[test]
fn colliding_short_names_stay_unique() {
    let image = image("collide");
    let names: Vec<String> = (0..40).map(|i| format!("boot-{i:04}.log")).collect();
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        for (i, name) in names.iter().enumerate() {
            write_new(&mut fs, name, format!("entry {i}").as_bytes(), stamp());
        }
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        for (i, name) in names.iter().enumerate() {
            assert_eq!(fs::read(mount.join(name)).expect("read"), format!("entry {i}").into_bytes(), "{name}");
        }
    });

    // The host cannot check this and neither can `fsck_msdos`: both read the
    // long names. Duplicate 8.3 names are still a broken volume, and this is
    // the only place that would notice.
    let mut fs = Fat32::mount(image.device()).expect("remount");
    let root = fs.geometry().root_cluster;
    let mut shorts = common::short_names_in(&mut fs, root);
    assert_eq!(shorts.len(), names.len());
    shorts.sort();
    let before = shorts.len();
    shorts.dedup();
    assert_eq!(shorts.len(), before, "duplicate 8.3 names in one directory");
}

/// Both FATs must say the same thing. Nothing else in this suite would notice
/// if they did not — see [`common::assert_fats_agree`].
#[test]
fn every_fat_copy_stays_in_step() {
    let image = image("mirrors");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        assert_eq!(fs.geometry().num_fats, 2, "the corpus has one FAT, so this proves nothing");
        fs.create_dir("d", stamp()).expect("mkdir");
        write_new(&mut fs, "d/allocating.bin", &pattern(200_000, 91), stamp());
        write_new(&mut fs, "d/second.bin", &pattern(30_000, 92), stamp());
        fs.remove("d/allocating.bin").expect("remove");
        let mut f = fs.open("d/second.bin").expect("open");
        fs.set_len(&mut f, 100).expect("truncate");
        fs.flush_meta(&mut f, stamp()).expect("flush");
        fs.sync().expect("sync");
        common::assert_fats_agree(&mut fs);
    }
    image.fsck();
}

#[test]
fn appending_extends_the_chain() {
    let image = image("append");
    let chunk = pattern(700, 21);
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        let mut f = fs.create("grown.bin", stamp()).expect("create");
        for i in 0..50 {
            fs.write(&mut f, i * chunk.len() as u64, &chunk).expect("append");
        }
        fs.flush_meta(&mut f, stamp()).expect("flush");
        fs.sync().expect("sync");
    }
    image.fsck();
    let want: Vec<u8> = chunk.iter().cycle().take(50 * chunk.len()).copied().collect();
    image.with_mount(|mount| {
        assert_eq!(fs::read(mount.join("grown.bin")).expect("read"), want);
    });
}

/// Delete a large file, then write another that must land on the clusters the
/// first one gave back. A free that misses an entry leaves the volume with a
/// cross-link `fsck` will find.
#[test]
fn deleted_clusters_come_back() {
    let image = image("reuse");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        let before = fs.free_bytes().expect("free");

        write_new(&mut fs, "big.bin", &pattern(2_000_000, 31), stamp());
        let during = fs.free_bytes().expect("free");
        assert!(during < before, "{during} not less than {before}");

        fs.remove("big.bin").expect("remove");
        let after = fs.free_bytes().expect("free");
        assert_eq!(after, before, "space not returned");

        write_new(&mut fs, "second.bin", &pattern(2_000_000, 32), stamp());
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        assert!(!mount.join("big.bin").exists());
        assert_eq!(fs::read(mount.join("second.bin")).expect("read"), pattern(2_000_000, 32));
    });
}

/// Filling the volume must end in `NoSpace` with the filesystem still sound —
/// not a partly written chain, and not a panic.
#[test]
fn a_full_volume_refuses_cleanly() {
    let image = image("full");
    let mut written = 0u64;
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        let chunk = pattern(256 * 1024, 41);
        let mut hit_the_wall = false;
        for i in 0..1000 {
            let name = format!("fill{i:04}.bin");
            let mut f = match fs.create(&name, stamp()) {
                Ok(f) => f,
                Err(Error::NoSpace) => {
                    hit_the_wall = true;
                    break;
                }
                Err(e) => panic!("create {name}: {e}"),
            };
            match fs.write(&mut f, 0, &chunk) {
                Ok(()) => {
                    fs.flush_meta(&mut f, stamp()).expect("flush");
                    written += chunk.len() as u64;
                }
                Err(Error::NoSpace) => {
                    // The partial chain is still the file's; recording the
                    // size it actually reached is what keeps the volume sound.
                    fs.flush_meta(&mut f, stamp()).expect("flush");
                    hit_the_wall = true;
                    break;
                }
                Err(e) => panic!("write {name}: {e}"),
            }
        }
        assert!(hit_the_wall, "volume never filled after {written} bytes");
        assert!(written > 32 * 1024 * 1024, "only {written} bytes fit in a 64 MiB volume");
        fs.sync().expect("sync");
    }
    image.fsck();
}

#[test]
fn rename_moves_a_file_between_directories() {
    let image = image("rename");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        fs.create_dir("from", stamp()).expect("mkdir");
        fs.create_dir("to", stamp()).expect("mkdir");
        write_new(&mut fs, "from/Original Long Name.txt", b"payload", stamp());
        fs.rename("from/Original Long Name.txt", "to/Renamed Long Name.txt").expect("rename");
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        assert!(!mount.join("from/Original Long Name.txt").exists());
        assert_eq!(fs::read(mount.join("to/Renamed Long Name.txt")).expect("read"), b"payload");
        assert_eq!(fs::read_dir(mount.join("from")).expect("read_dir").count(), 0);
    });
}

/// A moved directory's `..` must point at its new parent. `fsck_msdos` checks
/// this and nothing else would.
#[test]
fn rename_repoints_a_moved_directorys_parent() {
    let image = image("mvdir");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        fs.create_dir_all("a/movable", stamp()).expect("mkdir");
        fs.create_dir("b", stamp()).expect("mkdir");
        write_new(&mut fs, "a/movable/file.txt", b"inside", stamp());
        fs.rename("a/movable", "b/moved").expect("rename");
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        assert_eq!(fs::read(mount.join("b/moved/file.txt")).expect("read"), b"inside");
    });
}

#[test]
fn truncation_releases_clusters_and_the_host_agrees() {
    let image = image("trunc");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        write_new(&mut fs, "shrink.bin", &pattern(100_000, 51), stamp());
        let full = fs.free_bytes().expect("free");

        let mut f = fs.open("shrink.bin").expect("open");
        fs.set_len(&mut f, 1000).expect("truncate");
        fs.flush_meta(&mut f, stamp()).expect("flush");
        assert!(fs.free_bytes().expect("free") > full, "truncation freed nothing");

        // Growing again must zero-fill rather than expose whatever the freed
        // clusters still held.
        let mut g = fs.open("shrink.bin").expect("open");
        fs.set_len(&mut g, 5000).expect("grow");
        fs.flush_meta(&mut g, stamp()).expect("flush");
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        let got = fs::read(mount.join("shrink.bin")).expect("read");
        assert_eq!(got.len(), 5000);
        assert_eq!(&got[..1000], &pattern(100_000, 51)[..1000]);
        assert!(got[1000..].iter().all(|&b| b == 0), "grow did not zero-fill");
    });
}

/// A write starting past the end of a file must zero the gap, not expose the
/// previous owner of the clusters it allocated.
#[test]
fn sparse_writes_zero_the_gap() {
    let image = image("sparse");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        // Dirty a region first, then free it, so the clusters the sparse write
        // gets are not already zero.
        write_new(&mut fs, "scratch.bin", &pattern(100_000, 61), stamp());
        fs.remove("scratch.bin").expect("remove");

        let mut f = fs.create("holes.bin", stamp()).expect("create");
        fs.write(&mut f, 50_000, b"tail").expect("write far");
        fs.write(&mut f, 0, b"head").expect("write near");
        fs.flush_meta(&mut f, stamp()).expect("flush");
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        let got = fs::read(mount.join("holes.bin")).expect("read");
        assert_eq!(got.len(), 50_004);
        assert_eq!(&got[..4], b"head");
        assert_eq!(&got[50_000..], b"tail");
        assert!(got[4..50_000].iter().all(|&b| b == 0), "gap not zeroed");
    });
}

#[test]
fn empty_directories_are_removable_and_full_ones_are_not() {
    let image = image("rmdir");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        fs.create_dir("keep", stamp()).expect("mkdir");
        fs.create_dir("gone", stamp()).expect("mkdir");
        write_new(&mut fs, "keep/file.txt", b"x", stamp());
        assert_eq!(fs.remove_dir("keep").unwrap_err(), Error::DirectoryNotEmpty);
        assert_eq!(fs.remove("keep").unwrap_err(), Error::IsADirectory);
        assert_eq!(fs.remove_dir("keep/file.txt").unwrap_err(), Error::NotADirectory);
        fs.remove_dir("gone").expect("rmdir");
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        assert!(!mount.join("gone").exists());
        assert!(mount.join("keep/file.txt").exists());
    });
}

/// Round trip through both readers: the host's, and this crate's own on a
/// volume this crate wrote.
#[test]
fn our_own_reader_agrees_with_the_host_on_our_own_volume() {
    let image = image("roundtrip");
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("SHORT.BIN", pattern(300, 71)),
        ("a long one.bin", pattern(9_000, 72)),
        ("d/nested name.bin", pattern(1_500, 73)),
    ];
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        fs.create_dir("d", stamp()).expect("mkdir");
        for (n, d) in &files {
            write_new(&mut fs, n, d, stamp());
        }
        fs.sync().expect("sync");
    }
    image.fsck();

    let mut fs = Fat32::mount(image.device()).expect("remount");
    let mut want: Vec<(String, u64)> = files.iter().map(|(n, d)| ((*n).into(), d.len() as u64)).collect();
    want.sort();
    assert_eq!(sorted_walk(&mut fs), want);
    for (n, d) in &files {
        assert_eq!(&read_all(&mut fs, n), d, "{n}");
    }

    let tree = image.with_mount(host_tree);
    for (n, d) in &files {
        assert_eq!(tree.get(*n).unwrap_or_else(|| panic!("{n} missing")), d);
    }
}

/// Two names that differ only in case are one file on FAT, and a create that
/// did not know that would leave two entries the host cannot tell apart.
#[test]
fn creating_an_existing_name_in_another_case_is_refused() {
    let image = image("case");
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        write_new(&mut fs, "Report.TXT", b"first", stamp());
        assert_eq!(fs.create("report.txt", stamp()).unwrap_err(), Error::AlreadyExists);
        assert_eq!(fs.create("REPORT.TXT", stamp()).unwrap_err(), Error::AlreadyExists);
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        assert_eq!(fs::read_dir(mount).expect("read_dir").count(), 1);
    });
}

/// A 4 KiB cluster volume: the same code with different arithmetic.
#[test]
fn four_kib_clusters_write_correctly() {
    let image = Image::new("write4k", 300 * 1024 * 1024, 8);
    let big = pattern(300_000, 81);
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        assert_eq!(fs.geometry().bytes_per_cluster(), 4096);
        fs.create_dir_all("x/y/z", stamp()).expect("mkdir");
        write_new(&mut fs, "x/y/z/Big File.bin", &big, stamp());
        write_new(&mut fs, "edge4095.bin", &pattern(4095, 82), stamp());
        write_new(&mut fs, "edge4097.bin", &pattern(4097, 83), stamp());
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        assert_eq!(fs::read(mount.join("x/y/z/Big File.bin")).expect("read"), big);
        assert_eq!(fs::read(mount.join("edge4095.bin")).expect("read"), pattern(4095, 82));
        assert_eq!(fs::read(mount.join("edge4097.bin")).expect("read"), pattern(4097, 83));
    });
}

/// The use this crate exists for: append a log line at a time and have the
/// result readable on another machine.
#[test]
fn a_log_file_written_a_line_at_a_time() {
    let image = image("log");
    let lines: Vec<String> = (0..500).map(|i| format!("[{i:04}] a line of kernel log output\n")).collect();
    {
        let mut fs = Fat32::mount(image.device()).expect("mount");
        fs.create_dir("TOYOS", stamp()).expect("mkdir");
        let mut f = fs.create("TOYOS/boot.log", stamp()).expect("create");
        let mut at = 0u64;
        for line in &lines {
            fs.write(&mut f, at, line.as_bytes()).expect("append line");
            at += line.len() as u64;
            // A crash-safe logger flushes metadata as it goes; this is also
            // what makes the entry's size track the data.
            fs.flush_meta(&mut f, stamp()).expect("flush");
        }
        fs.sync().expect("sync");
    }
    image.fsck();
    image.with_mount(|mount| {
        let got = fs::read_to_string(mount.join("TOYOS/boot.log")).expect("read");
        assert_eq!(got, lines.concat());
    });
}
