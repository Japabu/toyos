use bcachefs::{Extent, Formatted, FsError, Mounted, ReadOnly, ReadWrite, VecBlockIO};

// --- Basic read-only tests ---

#[test]
fn format_and_mount_empty() {
    let io = VecBlockIO::new(128);
    let fs = Formatted::format(io);
    let mounted = fs.mount_readonly();
    let files = mounted.list().expect("list failed");
    assert!(files.is_empty(), "expected empty filesystem, got {:?}", files);
}

#[test]
fn create_single_small_file() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("hello.txt", b"Hello, world!", 42).expect("create failed");
    let mounted = fs.mount_readonly();

    let files = mounted.list().expect("list failed");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "hello.txt");
    assert_eq!(files[0].1, 13);

    let data = mounted.read_file("hello.txt").expect("read failed");
    assert_eq!(data, b"Hello, world!");

    assert_eq!(mounted.file_mtime("hello.txt"), 42);
}

#[test]
fn create_multiple_files() {
    let io = VecBlockIO::new(256);
    let mut fs = Formatted::format(io);

    fs.create("bin/shell", b"shell-binary-data", 100).expect("create shell");
    fs.create("bin/compositor", b"compositor-binary-data-longer", 200).expect("create compositor");
    fs.create("share/font.ttf", b"font-data", 300).expect("create font");

    let mounted = fs.mount_readonly();

    let files = mounted.list().expect("list failed");
    assert_eq!(files.len(), 3, "expected 3 files, got: {:?}", files);

    assert_eq!(mounted.read_file("bin/shell").unwrap(), b"shell-binary-data");
    assert_eq!(mounted.read_file("bin/compositor").unwrap(), b"compositor-binary-data-longer");
    assert_eq!(mounted.read_file("share/font.ttf").unwrap(), b"font-data");

    assert_eq!(mounted.file_mtime("bin/shell"), 100);
    assert_eq!(mounted.file_mtime("bin/compositor"), 200);
    assert_eq!(mounted.file_mtime("share/font.ttf"), 300);
}

#[test]
fn file_not_found() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("exists.txt", b"data", 0).expect("create");
    let mounted = fs.mount_readonly();

    let result = mounted.read_file("nonexistent.txt");
    assert!(result.is_err(), "expected NotFound error");
}

#[test]
fn file_mtime_nonexistent() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("exists.txt", b"data", 999).expect("create");
    let mounted = fs.mount_readonly();

    // file_mtime returns 0 for nonexistent files, not panic
    assert_eq!(mounted.file_mtime("exists.txt"), 999);
    assert_eq!(mounted.file_mtime("nope.txt"), 0);
}

#[test]
fn read_link() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("real.txt", b"real data", 0).expect("create file");
    fs.create_symlink("link.txt", "real.txt", 0).expect("create symlink");

    let mounted = fs.mount_readonly();

    let target = mounted.read_link("link.txt");
    assert_eq!(target.as_deref(), Some("real.txt"));

    assert_eq!(mounted.read_link("real.txt"), None);
    assert_eq!(mounted.read_link("nope"), None);
}

#[test]
fn list_includes_symlinks() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("file.txt", b"data", 0).expect("create file");
    fs.create_symlink("link.txt", "file.txt", 0).expect("create symlink");

    let mounted = fs.mount_readonly();
    let files = mounted.list().expect("list");
    assert_eq!(files.len(), 2, "expected 2 entries (file + symlink), got: {:?}", files);

    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"file.txt"), "missing file.txt in {:?}", names);
    assert!(names.contains(&"link.txt"), "missing link.txt in {:?}", names);

    assert!(mounted.is_symlink("link.txt"));
    assert!(!mounted.is_symlink("file.txt"));
}

#[test]
fn dangling_symlink_allowed() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    // Symlink to nonexistent target should succeed — symlinks are just strings
    fs.create_symlink("dangling", "/nonexistent/path", 0).expect("create dangling symlink");

    let mounted = fs.mount_readonly();
    assert_eq!(mounted.read_link("dangling").as_deref(), Some("/nonexistent/path"));
    assert!(mounted.is_symlink("dangling"));
}

// --- File size edge cases ---

#[test]
fn empty_file() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("empty", b"", 0).expect("create empty file");

    let mounted = fs.mount_readonly();
    let files = mounted.list().expect("list");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].0, "empty");
    assert_eq!(files[0].1, 0);

    let data = mounted.read_file("empty").expect("read empty");
    assert!(data.is_empty());
}

#[test]
fn large_file_single_extent() {
    let data = vec![0xABu8; 100 * 1024];
    let io = VecBlockIO::new(512);
    let mut fs = Formatted::format(io);
    fs.create("big.bin", &data, 0).expect("create large file");

    let mounted = fs.mount_readonly();
    let read_data = mounted.read_file("big.bin").expect("read large file");
    assert_eq!(read_data.len(), data.len());
    assert_eq!(read_data, data);
}

#[test]
fn large_file_exact_block_boundary() {
    let data = vec![0x42u8; 4096];
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("block.bin", &data, 0).expect("create");

    let mounted = fs.mount_readonly();
    let read_data = mounted.read_file("block.bin").expect("read");
    assert_eq!(read_data, data);
}

#[test]
fn large_file_crosses_block_boundary() {
    let data: Vec<u8> = (0..4097).map(|i| (i % 256) as u8).collect();
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("cross.bin", &data, 0).expect("create");

    let mounted = fs.mount_readonly();
    let read_data = mounted.read_file("cross.bin").expect("read");
    assert_eq!(read_data.len(), 4097);
    assert_eq!(read_data, data);
}

// --- Filename edge cases ---

#[test]
fn long_filename_near_entry_limit() {
    // 200-byte filename should work fine (fits in a leaf entry)
    let name: String = (0..200).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create(&name, b"data", 0).expect("create with 200-byte name");

    let mounted = fs.mount_readonly();
    assert_eq!(mounted.read_file(&name).unwrap(), b"data");

    // 513-byte filename should be rejected (MAX_NAME_LEN = 512)
    let too_long: String = (0..513).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    let io2 = VecBlockIO::new(128);
    let mut fs2 = Formatted::format(io2);
    let result = fs2.create(&too_long, b"data", 0);
    assert!(result.is_err(), "expected NameTooLong for 513-byte filename");
}

#[test]
fn zero_length_filename_rejected() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    let result = fs.create("", b"data", 0);
    assert!(result.is_err(), "empty filename should be rejected");
}

#[test]
fn duplicate_filename_overwrites() {
    // Creating a file with the same name should overwrite, not duplicate
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("test.txt", b"version 1", 10).expect("create v1");
    fs.create("test.txt", b"version 2", 20).expect("create v2");

    let mounted = fs.mount_readonly();
    let files = mounted.list().expect("list");
    assert_eq!(files.len(), 1, "duplicate filename should overwrite, not create second entry");
    assert_eq!(mounted.read_file("test.txt").unwrap(), b"version 2");
    assert_eq!(mounted.file_mtime("test.txt"), 20);
}

// --- B+ tree split correctness ---

#[test]
fn incremental_insert_and_read() {
    // Insert files one at a time, checking readability after each insert.
    // This exercises node splits — hashed keys land in unpredictable order,
    // and we verify no entries are lost across splits.
    let io = VecBlockIO::new(2048);
    let mut fs = Formatted::format(io);

    for i in 0..100 {
        let name = format!("file_{:04}.txt", i);
        let data = format!("content of file {}", i);
        fs.create(&name, data.as_bytes(), i as u64)
            .unwrap_or_else(|e| panic!("create {} failed: {:?}", name, e));

        // Verify ALL previously inserted files are still readable
        let mounted = fs.mount_readonly();
        for j in 0..=i {
            let check_name = format!("file_{:04}.txt", j);
            let expected = format!("content of file {}", j);
            let read_data = mounted.read_file(&check_name).unwrap_or_else(|e| {
                panic!(
                    "after inserting file_{:04}, cannot read {}: {:?}",
                    i, check_name, e
                );
            });
            assert_eq!(
                String::from_utf8(read_data).unwrap(),
                expected,
                "content mismatch for {} after inserting file_{:04}",
                check_name,
                i
            );
        }
        fs = mounted.into_formatted();
    }
}

#[test]
fn many_files_with_large_data() {
    // Simulate a realistic initrd: 50 files of varying sizes
    let io = VecBlockIO::new(4096);
    let mut fs = Formatted::format(io);

    let mut expected: Vec<(String, Vec<u8>)> = Vec::new();

    for i in 0..50 {
        let name = format!("bin/program_{}", i);
        let size = (i + 1) * 1024; // 1KB to 50KB
        let data: Vec<u8> = (0..size).map(|j| ((i + j) % 256) as u8).collect();
        fs.create(&name, &data, i as u64 * 1000).expect(&format!("create {}", name));
        expected.push((name, data));
    }

    let mounted = fs.mount_readonly();
    let files = mounted.list().expect("list");
    assert_eq!(files.len(), 50);

    for (name, data) in &expected {
        let read_data = mounted.read_file(name).unwrap_or_else(|e| {
            panic!("failed to read {}: {:?}", name, e);
        });
        assert_eq!(read_data.len(), data.len(), "size mismatch for {}", name);
        assert_eq!(&read_data, data, "data mismatch for {}", name);
    }
}

// --- Read-write tests ---

#[test]
fn mounted_readwrite_create_and_read() {
    let io = VecBlockIO::new(256);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    mounted.create("test.txt", b"hello world", 100).expect("create");
    let data = mounted.read_file("test.txt").expect("read");
    assert_eq!(data, b"hello world");
    assert_eq!(mounted.file_mtime("test.txt"), 100);
}

#[test]
fn mounted_readwrite_delete() {
    let io = VecBlockIO::new(256);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    mounted.create("a.txt", b"aaa", 0).expect("create a");
    mounted.create("b.txt", b"bbb", 0).expect("create b");
    assert_eq!(mounted.list().unwrap().len(), 2);

    assert!(mounted.delete("a.txt").expect("delete"));
    assert_eq!(mounted.list().unwrap().len(), 1);
    assert!(mounted.read_file("a.txt").is_err());
    assert_eq!(mounted.read_file("b.txt").unwrap(), b"bbb");

    assert!(!mounted.delete("nonexistent").expect("delete"));
}

#[test]
fn mounted_readwrite_delete_prefix() {
    // 200 files, only 3 match the prefix. Verifies the full-tree scan
    // doesn't accidentally delete non-matching entries (keys are hash-ordered,
    // not name-ordered, so prefix matching must check every leaf).
    let io = VecBlockIO::new(4096);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    // Create 200 files across various prefixes
    for i in 0..100 {
        let name = format!("lib/lib_{:03}.so", i);
        mounted.create(&name, format!("lib{}", i).as_bytes(), 0).expect("create lib");
    }
    for i in 0..100 {
        let name = format!("share/asset_{:03}", i);
        mounted.create(&name, format!("asset{}", i).as_bytes(), 0).expect("create share");
    }
    mounted.create("bin/shell", b"shell", 0).expect("create");
    mounted.create("bin/editor", b"editor", 0).expect("create");
    mounted.create("bin/compositor", b"compositor", 0).expect("create");

    assert_eq!(mounted.list().unwrap().len(), 203);

    mounted.delete_prefix("bin/").expect("delete_prefix");
    let files = mounted.list().unwrap();
    assert_eq!(files.len(), 200, "expected 200 files after deleting bin/, got {}", files.len());
    assert!(mounted.read_file("bin/shell").is_err());
    assert!(mounted.read_file("bin/editor").is_err());
    assert!(mounted.read_file("bin/compositor").is_err());

    // Verify all non-matching files survived
    for i in 0..100 {
        let name = format!("lib/lib_{:03}.so", i);
        assert_eq!(
            mounted.read_file(&name).unwrap(),
            format!("lib{}", i).as_bytes(),
            "{} corrupted after delete_prefix",
            name
        );
    }
    for i in 0..100 {
        let name = format!("share/asset_{:03}", i);
        assert_eq!(
            mounted.read_file(&name).unwrap(),
            format!("asset{}", i).as_bytes(),
            "{} corrupted after delete_prefix",
            name
        );
    }
}

#[test]
fn mounted_readwrite_overwrite_file() {
    let io = VecBlockIO::new(256);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    mounted.create("test.txt", b"version 1", 10).expect("create v1");
    assert_eq!(mounted.read_file("test.txt").unwrap(), b"version 1");

    mounted.create("test.txt", b"version 2 is longer", 20).expect("create v2");
    assert_eq!(mounted.read_file("test.txt").unwrap(), b"version 2 is longer");
    assert_eq!(mounted.file_mtime("test.txt"), 20);
    assert_eq!(mounted.list().unwrap().len(), 1);
}

#[test]
fn mounted_readwrite_symlink() {
    let io = VecBlockIO::new(256);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    mounted.create("real.txt", b"real data", 0).expect("create");
    mounted.create_symlink("link.txt", "real.txt").expect("symlink");

    assert_eq!(mounted.read_link("link.txt").as_deref(), Some("real.txt"));
    assert_eq!(mounted.read_link("real.txt"), None);
    assert!(mounted.is_symlink("link.txt"));
    assert!(!mounted.is_symlink("real.txt"));
}

#[test]
fn mounted_readwrite_sync_and_reopen() {
    let io = VecBlockIO::new(512);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    mounted.create("persistent.txt", b"I survive reboots", 42).expect("create");
    mounted.sync();

    let raw = mounted.into_formatted().into_io().into_vec();
    let io2 = VecBlockIO::from_vec(raw);
    let mounted2 = Mounted::<_, ReadOnly>::open(io2).expect("reopen");

    assert_eq!(mounted2.read_file("persistent.txt").unwrap(), b"I survive reboots");
    assert_eq!(mounted2.file_mtime("persistent.txt"), 42);
}

#[test]
fn mounted_readwrite_double_roundtrip() {
    // Create → sync → reopen rw → create more → sync → reopen ro → verify all.
    // Catches bitmap free count drift or state corruption across reopens.
    let io = VecBlockIO::new(512);
    let fs = Formatted::format(io);
    let mut m = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    m.create("round1.txt", b"first round", 10).expect("create round1");
    m.sync();
    let raw = m.into_formatted().into_io().into_vec();

    // Reopen read-write, add more
    let mut m = Mounted::<_, ReadWrite>::open(VecBlockIO::from_vec(raw)).expect("reopen rw");
    assert_eq!(m.read_file("round1.txt").unwrap(), b"first round");
    m.create("round2.txt", b"second round", 20).expect("create round2");
    m.sync();
    let raw = m.into_formatted().into_io().into_vec();

    // Final read-only verification
    let m = Mounted::<_, ReadOnly>::open(VecBlockIO::from_vec(raw)).expect("reopen ro");
    assert_eq!(m.list().unwrap().len(), 2);
    assert_eq!(m.read_file("round1.txt").unwrap(), b"first round");
    assert_eq!(m.read_file("round2.txt").unwrap(), b"second round");
    assert_eq!(m.file_mtime("round1.txt"), 10);
    assert_eq!(m.file_mtime("round2.txt"), 20);
}

#[test]
fn mounted_readwrite_overwrite_with_smaller_data() {
    // Overwrite a 4KB file with 10 bytes. Verifies old extents are freed
    // and the reclaimed blocks are reusable.
    let io = VecBlockIO::new(64); // tight on space
    let fs = Formatted::format(io);
    let mut m = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    // Fill with a large file (uses most free blocks)
    let big = vec![0xBBu8; 40 * 1024]; // 10 blocks
    m.create("big.bin", &big, 0).expect("create big");

    // Overwrite with tiny data — should free the 10 blocks
    m.create("big.bin", b"tiny", 0).expect("overwrite with smaller");
    assert_eq!(m.read_file("big.bin").unwrap(), b"tiny");
    assert_eq!(m.list().unwrap().len(), 1);

    // The freed blocks should be reusable — create another large file
    let big2 = vec![0xCCu8; 40 * 1024];
    m.create("big2.bin", &big2, 0).expect("create big2 with reclaimed space");
    assert_eq!(m.read_file("big2.bin").unwrap(), big2);
}

// --- Filesystem capacity ---

#[test]
fn filesystem_full_returns_no_space() {
    // Tiny filesystem: 32 blocks total. Fill until alloc fails.
    let io = VecBlockIO::new(32);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    let mut created = 0;
    for i in 0..100 {
        let name = format!("f{}", i);
        let data = vec![0xFFu8; 4096]; // 1 block per file
        match mounted.create(&name, &data, 0) {
            Ok(()) => created += 1,
            Err(_) => break, // NoSpace expected
        }
    }
    assert!(created > 0, "should have created at least one file");
    assert!(created < 32, "should have hit NoSpace before 32 files");

    // All previously created files should still be readable
    for i in 0..created {
        let name = format!("f{}", i);
        let data = mounted.read_file(&name).unwrap_or_else(|e| {
            panic!("file {} unreadable after NoSpace: {:?}", name, e);
        });
        assert_eq!(data.len(), 4096, "data corruption in {} after NoSpace", name);
    }
}

// --- Integrity ---

#[test]
fn superblock_backup_recovery() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("test.txt", b"test data", 0).expect("create");
    let mut raw = fs.into_io().into_vec();

    // Corrupt block 0 (superblock)
    raw[0..4].copy_from_slice(b"JUNK");

    let io = VecBlockIO::from_vec(raw);
    let mounted = Mounted::<_, ReadOnly>::open(io).expect("mount from backup");
    let data = mounted.read_file("test.txt").expect("read after recovery");
    assert_eq!(data, b"test data");
}

#[test]
fn crc_verification_on_nodes() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("test.txt", b"hello", 0).expect("create");
    let mut raw = fs.into_io().into_vec();

    // Corrupt a byte in the root node (block 2 for small fs)
    let root_offset = 2 * 4096 + 100;
    raw[root_offset] ^= 0xFF;

    let io = VecBlockIO::from_vec(raw);
    let mounted = Mounted::<_, ReadOnly>::open(io).expect("mount");
    let result = mounted.read_file("test.txt");
    assert!(result.is_err(), "expected checksum error, got: {:?}", result.ok().map(|d| d.len()));
}

#[test]
fn corrupt_data_block_returns_raw_bytes() {
    // Data blocks have no CRC, so corruption reads back silently.
    //
    // Layout for a 128-block filesystem:
    //   block 0: superblock
    //   block 1: bitmap (128 blocks / 32768 bits_per_block = 1 block)
    //   block 2: root btree node
    //   block 3+: data blocks (first file's data starts here)
    //   block 127: superblock backup
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    let original = vec![0xAAu8; 4096];
    fs.create("data.bin", &original, 0).expect("create");
    let mut raw = fs.into_io().into_vec();

    // Corrupt byte 50 of the first data block (block 3)
    let data_offset = 3 * 4096 + 50;
    raw[data_offset] ^= 0xFF;

    let io = VecBlockIO::from_vec(raw);
    let mounted = Mounted::<_, ReadOnly>::open(io).expect("mount");
    let data = mounted.read_file("data.bin").expect("read should succeed — no data CRC");
    assert_ne!(data, original, "corruption should be visible in read data");
    assert_eq!(data[50], 0xAA ^ 0xFF, "byte 50 should be flipped");
}

// --- State transitions ---

#[test]
fn format_mount_unmount_create_mount_roundtrip() {
    // Verify into_formatted preserves all state: superblock, bitmap, free count
    let io = VecBlockIO::new(512);
    let mut fs = Formatted::format(io);

    // Create files in Formatted state
    fs.create("phase1.txt", b"created during format", 10).expect("create phase1");

    // Mount readonly, verify, unmount back to Formatted
    let mounted = fs.mount_readonly();
    assert_eq!(mounted.list().unwrap().len(), 1);
    assert_eq!(mounted.read_file("phase1.txt").unwrap(), b"created during format");
    fs = mounted.into_formatted();

    // Create more files in Formatted state after round-trip
    fs.create("phase2.txt", b"created after round-trip", 20).expect("create phase2");

    // Mount readonly again, verify both files exist
    let mounted = fs.mount_readonly();
    let files = mounted.list().unwrap();
    assert_eq!(files.len(), 2, "expected 2 files after round-trip, got: {:?}", files);
    assert_eq!(mounted.read_file("phase1.txt").unwrap(), b"created during format");
    assert_eq!(mounted.read_file("phase2.txt").unwrap(), b"created after round-trip");
    assert_eq!(mounted.file_mtime("phase1.txt"), 10);
    assert_eq!(mounted.file_mtime("phase2.txt"), 20);
}

// --- Values sized by userland: the extent list lives inline in a btree node ---

/// The path the kernel's flush takes: one `resolve_or_alloc_block` per dirty
/// page, in ascending order, then one `update_metadata` for the whole file.
fn write_pages(fs: &mut Mounted<VecBlockIO, ReadWrite>, pages: u32) -> Vec<bcachefs::Extent> {
    let mut extents = Vec::new();
    for page in 0..pages {
        fs.resolve_or_alloc_block(&mut extents, page).expect("allocate a block for the page");
    }
    extents
}

#[test]
fn a_sequential_file_needs_one_extent() {
    let mut fs = Formatted::format(VecBlockIO::new(4096)).mount();
    fs.create("seq.bin", b"", 1).expect("create");

    let extents = write_pages(&mut fs, 600);

    // 600 pages used to be 600 extents of 16 bytes — 9600 bytes into a value
    // that has to fit a 4040-byte node payload.
    assert_eq!(
        extents.len(), 1,
        "a run of consecutive blocks became {} extents: {:?}", extents.len(), extents,
    );
    assert_eq!(extents[0].block_count, 600);
}

#[test]
fn a_file_past_the_old_extent_cap_round_trips() {
    // ~250 pages was where `19 + name + 16*pages` crossed the node payload and
    // `write_to` underflowed `MAX_PAYLOAD - used`. This is four times that.
    let mut fs = Formatted::format(VecBlockIO::new(4096)).mount();
    fs.create("big.bin", b"", 1).expect("create");

    let extents = write_pages(&mut fs, 1000);
    let size = 1000 * 4096;
    fs.update_metadata("big.bin", &extents, size, 7).expect("metadata for a 1000-page file");

    let (back, got_size) = fs.file_extents("big.bin").expect("reopen");
    assert_eq!(got_size, size);
    assert_eq!(back.len(), 1, "extents did not survive the round trip: {back:?}");
    assert_eq!(back[0].block_count, 1000);
}

#[test]
fn a_value_too_large_for_any_node_is_an_error_not_a_panic() {
    let mut fs = Formatted::format(VecBlockIO::new(8192)).mount();
    fs.create("frag.bin", b"", 1).expect("create");

    // Discontiguous by construction: merging cannot help, so this is the case
    // that has to be *refused*. Each extent is a separate 16-byte run.
    let extents: Vec<bcachefs::Extent> = (0..400)
        .map(|i| bcachefs::Extent { start_block: 3000 + i * 2, block_count: 1, _reserved: 0 })
        .collect();

    match fs.update_metadata("frag.bin", &extents, 400 * 4096, 7) {
        Err(bcachefs::FsError::EntryTooLarge { size, max }) => {
            assert!(size > max, "rejected an entry that fits: {size} <= {max}");
        }
        other => panic!("expected EntryTooLarge, got {other:?}"),
    }

    // And the rejection did not take the file with it. update_metadata used to
    // delete the old entry before attempting the insert.
    assert!(
        fs.file_extents("frag.bin").is_some(),
        "the file was deleted by a metadata update that failed",
    );
}

#[test]
fn rename_bounds_the_new_name() {
    let mut fs = Formatted::format(VecBlockIO::new(256)).mount();
    fs.create("short.txt", b"data", 1).expect("create");

    // Every other name-taking entry point checked this one and rename did not.
    let huge = "n".repeat(64 * 1024);
    match fs.rename("short.txt", &huge) {
        Err(bcachefs::FsError::NameTooLong { .. }) => {}
        other => panic!("expected NameTooLong, got {other:?}"),
    }
    assert!(fs.file_extents("short.txt").is_some(), "rename lost the source file");
}

// --- Rename: the destination's blocks are freed, the source's are carried ---

/// The blocks an extent list names, as the pairs a comparison can read.
fn runs(extents: &[bcachefs::Extent]) -> Vec<(u64, u32)> {
    extents.iter().map(|e| (e.start_block, e.block_count)).collect()
}

/// Every name on the volume, sorted.
fn names(fs: &Mounted<VecBlockIO, ReadWrite>) -> Vec<String> {
    let mut names: Vec<String> = fs.list().expect("list").into_iter().map(|(n, _)| n).collect();
    names.sort();
    names
}

#[test]
fn a_rename_carries_the_file_to_the_new_name() {
    // Nothing covered a *successful* rename: the only test that called it
    // asserted the NameTooLong direction, so a rename that reported success
    // and freed the file's blocks was green everywhere.
    let mut fs = Formatted::format(VecBlockIO::new(256)).mount();
    let data: Vec<u8> = (0..3 * 4096 + 17).map(|i: usize| (i.wrapping_mul(31) ^ 0x5A) as u8).collect();
    fs.create("a.bin", &data, 77).expect("create");
    let (before, size) = fs.file_extents("a.bin").expect("the source's extents");

    fs.rename("a.bin", "b.bin").expect("rename");

    assert_eq!(names(&fs), ["b.bin"], "the old name outlived the rename");
    assert!(fs.read_file("a.bin").is_err(), "the old name still resolves");
    assert_eq!(fs.read_file("b.bin").expect("read the renamed file"), data);
    assert_eq!(fs.file_mtime("b.bin"), 77, "the rename invented an mtime");

    let (after, size_after) = fs.file_extents("b.bin").expect("the renamed file's extents");
    assert_eq!(runs(&after), runs(&before), "the rename moved the file's blocks");
    assert_eq!(size_after, size);

    // And on disk, not just in the tree this process is holding.
    fs.sync();
    let raw = fs.into_formatted().into_io().into_vec();
    let reopened = Mounted::<_, ReadOnly>::open(VecBlockIO::from_vec(raw)).expect("reopen");
    assert_eq!(reopened.read_file("b.bin").expect("read after reopen"), data);
    assert!(reopened.read_file("a.bin").is_err(), "the old name came back from disk");
}

#[test]
fn a_renamed_file_keeps_every_extent_it_had() {
    // A file in one run survives a rename that reallocates as readily as one
    // that carries the extents. Discontiguous runs do not.
    let (mut fs, _) = one_block_holes(64);
    let data: Vec<u8> = (0..4 * 4096 + 11).map(|i| (i % 251) as u8).collect();
    fs.create("frag.bin", &data, 3).expect("create a fragmented file");
    let (before, _) = fs.file_extents("frag.bin").expect("extents");
    assert!(before.len() > 1, "the volume is not fragmented, this proves nothing: {before:?}");

    fs.rename("frag.bin", "moved.bin").expect("rename");

    let (after, _) = fs.file_extents("moved.bin").expect("the renamed file's extents");
    assert_eq!(runs(&after), runs(&before), "the extent list did not survive the re-encode");
    assert_eq!(fs.read_file("moved.bin").expect("read back"), data);
}

#[test]
fn a_rename_onto_an_existing_name_frees_that_file_and_only_it() {
    let mut fs = Formatted::format(VecBlockIO::new(64)).mount();
    let source = vec![0xA5u8; 10 * 4096];
    let doomed = vec![0x5Au8; 10 * 4096];
    fs.create("keep.bin", &source, 1).expect("create the source");
    fs.create("doomed.bin", &doomed, 2).expect("create the destination");
    let (source_blocks, _) = fs.file_extents("keep.bin").expect("the source's extents");

    // Spend the rest, so the only free blocks after the rename are the ones
    // the displaced file gave back.
    let mut filler = 0;
    while fs.create(&format!("filler{filler}"), &vec![0xFF; 4096], 0).is_ok() {
        filler += 1;
    }

    fs.rename("keep.bin", "doomed.bin").expect("rename onto an existing name");

    assert_eq!(fs.read_file("doomed.bin").expect("read the destination"), source);
    assert!(fs.read_file("keep.bin").is_err(), "the source name survived");
    assert_eq!(
        names(&fs).iter().filter(|n| n.ends_with(".bin")).count(),
        1,
        "the volume holds more than one entry under the two names: {:?}",
        names(&fs),
    );
    let (after, _) = fs.file_extents("doomed.bin").expect("extents");
    assert_eq!(runs(&after), runs(&source_blocks), "the source's blocks were not carried");

    // Ten blocks were freed by the displaced file and nothing else was, so a
    // ten-block file fits exactly once. A rename that freed the *source's*
    // blocks would leave room for two.
    fs.create("reclaim.bin", &vec![0xCCu8; 10 * 4096], 0).expect("the displaced blocks are free");
    assert_eq!(fs.read_file("doomed.bin").expect("read after reclaim"), source);
    assert!(
        fs.create("again.bin", &vec![0xDDu8; 10 * 4096], 0).is_err(),
        "twenty blocks came free where one ten-block file was displaced",
    );
}

#[test]
fn a_rename_onto_itself_keeps_the_file() {
    let mut fs = Formatted::format(VecBlockIO::new(128)).mount();
    fs.create("same.bin", b"the file that is its own destination", 9).expect("create");
    let (before, _) = fs.file_extents("same.bin").expect("extents");

    fs.rename("same.bin", "same.bin").expect("rename onto itself");

    assert_eq!(names(&fs), ["same.bin"]);
    assert_eq!(fs.read_file("same.bin").expect("read"), b"the file that is its own destination");
    let (after, _) = fs.file_extents("same.bin").expect("extents");
    assert_eq!(runs(&after), runs(&before));
}

#[test]
fn a_rename_of_a_symlink_stays_a_symlink() {
    let mut fs = Formatted::format(VecBlockIO::new(128)).mount();
    fs.create_symlink("link", "/home/target").expect("create a symlink");

    fs.rename("link", "moved-link").expect("rename");

    assert!(fs.is_symlink("moved-link"), "the rename turned a symlink into a file");
    assert_eq!(fs.read_link("moved-link").as_deref(), Some("/home/target"));
    assert_eq!(fs.read_link("link"), None, "the old name still reads as a symlink");
    assert_eq!(names(&fs), ["moved-link"]);
}

#[test]
fn a_file_renamed_onto_a_symlink_leaves_one_entry() {
    // The two differ in key type, so the insert does not replace the
    // destination — nothing but an explicit delete removes it, and what it
    // leaves behind answers to the same name with blocks no name can reach.
    let mut fs = Formatted::format(VecBlockIO::new(128)).mount();
    fs.create_symlink("shadow", "/somewhere").expect("create a symlink");
    fs.create("file.bin", b"a file, not a symlink", 4).expect("create a file");

    fs.rename("file.bin", "shadow").expect("rename a file onto a symlink");

    assert_eq!(names(&fs), ["shadow"], "the displaced symlink is still on the volume");
    assert!(!fs.is_symlink("shadow"), "the symlink still shadows the file that replaced it");
    assert_eq!(fs.read_file("shadow").expect("read"), b"a file, not a symlink");
}

#[test]
fn a_rename_with_no_source_touches_nothing() {
    let mut empty = Formatted::format(VecBlockIO::new(128)).mount();
    assert!(matches!(empty.rename("a", "b"), Err(bcachefs::FsError::NotFound)));
    assert!(empty.list().expect("list").is_empty(), "a rename created an entry from nothing");

    let mut fs = Formatted::format(VecBlockIO::new(128)).mount();
    fs.create("bystander.bin", b"not part of this", 6).expect("create");
    assert!(matches!(fs.rename("absent", "bystander.bin"), Err(bcachefs::FsError::NotFound)));
    assert_eq!(fs.read_file("bystander.bin").expect("read"), b"not part of this");
    assert_eq!(names(&fs), ["bystander.bin"]);
}

#[test]
fn entries_of_mixed_size_survive_node_splits() {
    // Keys are hashed, so the order entries land in a leaf is not the order
    // they are created in. Varying the name length by two orders of magnitude
    // is the cheapest way to put non-uniform entries in front of the split
    // logic, which is where halving by *count* stops being the right rule.
    let mut fs = Formatted::format(VecBlockIO::new(8192)).mount();
    let names: Vec<String> = (0..60)
        .map(|i| format!("{}{i:03}", "e".repeat(if i % 3 == 0 { 500 } else { 4 })))
        .collect();
    for (i, n) in names.iter().enumerate() {
        fs.create(n, b"x", i as u64).expect("create");
    }
    for n in &names {
        assert_eq!(fs.read_file(n).expect("read back"), b"x", "{n} did not survive splitting");
    }
}


// --- A short allocation is not the allocation that was asked for ---

/// A volume whose free space is nothing but one-block holes, so the allocator
/// can only ever report a run shorter than a multi-block request.
///
/// Returns the surviving files' single data blocks alongside it: a block the
/// allocator did *not* hand out is the ground truth for "this write landed on
/// somebody else's file".
fn one_block_holes(blocks: u64) -> (Mounted<VecBlockIO, ReadWrite>, Vec<(String, u64)>) {
    let mut fs = Formatted::format(VecBlockIO::new(blocks)).mount();
    let mut made = Vec::new();
    for i in 0..blocks {
        let name = format!("f{i:03}");
        if fs.create(&name, &vec![0xAAu8; 4096], 0).is_err() {
            break;
        }
        made.push(name);
    }
    assert!(made.len() > 8, "volume too small to fragment: {} files", made.len());
    for (i, name) in made.iter().enumerate() {
        if i % 2 == 0 {
            assert!(fs.delete(name).expect("delete"), "delete {name}");
        }
    }
    let survivors = made
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, n)| {
            let (extents, _) = fs.file_extents(n).expect("a survivor kept its extents");
            assert_eq!(extents.len(), 1);
            assert_eq!(extents[0].block_count, 1);
            (n.clone(), extents[0].start_block)
        })
        .collect();
    (fs, survivors)
}

#[test]
fn a_sparse_write_resolves_inside_the_blocks_it_reserved() {
    let (mut fs, survivors) = one_block_holes(64);

    // Page 3 of an empty file needs four blocks, and no free run here is
    // longer than one. `alloc_contiguous` says so in its second return value;
    // this caller used to read it as "all four, starting here".
    let mut extents = Vec::new();
    let block = fs.resolve_or_alloc_block(&mut extents, 3).expect("allocate page 3");

    let reserved: Vec<u64> = extents
        .iter()
        .flat_map(|e| (0..e.block_count as u64).map(move |i| e.start_block + i))
        .collect();
    assert!(
        reserved.contains(&block),
        "page 3 resolved to block {block}, outside the extents it recorded: {extents:?}",
    );
    assert!(
        !survivors.iter().any(|(_, b)| *b == block),
        "page 3 resolved to block {block}, which belongs to {:?}",
        survivors.iter().find(|(_, b)| *b == block).map(|(n, _)| n),
    );
}

#[test]
fn every_page_of_a_fragmented_file_owns_a_distinct_block() {
    let (mut fs, _) = one_block_holes(64);

    let mut extents = Vec::new();
    fs.resolve_or_alloc_block(&mut extents, 5).expect("allocate through page 5");
    let covered: u32 = extents.iter().map(|e| e.block_count).sum();
    assert_eq!(covered, 6, "six pages need six blocks, got {extents:?}");

    let mut seen: Vec<u64> = Vec::new();
    for page in 0..=5 {
        let block = fs.resolve_or_alloc_block(&mut extents, page).expect("resolve");
        assert!(!seen.contains(&block), "page {page} shares block {block} with an earlier page");
        seen.push(block);
    }
    assert_eq!(
        extents.iter().map(|e| e.block_count).sum::<u32>(),
        6,
        "resolving a page that already has a block allocated another: {extents:?}",
    );
}

// --- Write-path ordering: an operation that fails must not have destroyed
//     what it was asked to replace, nor kept what it took. ---

/// Blocks a 64-block volume has to give: everything but the superblock, the
/// bitmap, the root node and the backup superblock.
const FREE_BLOCKS_64: usize = 60;

fn small_volume() -> Mounted<VecBlockIO, ReadWrite> {
    Formatted::format(VecBlockIO::new(64)).mount()
}

/// How many one-block files this volume still has room for.
fn one_block_files_that_fit(fs: &mut Mounted<VecBlockIO, ReadWrite>) -> usize {
    let mut fitted = 0;
    for i in 0..FREE_BLOCKS_64 * 2 {
        let name = format!("p{:03}", i);
        if fs.create(&name, b"x", 0).is_err() {
            break;
        }
        fitted += 1;
    }
    fitted
}

#[test]
fn a_create_that_runs_out_of_space_leaves_the_old_file_where_it_was() {
    let mut fs = small_volume();
    let original = vec![0x5Au8; 5 * 4096];
    fs.create("keep.bin", &original, 7).expect("the first create fits");

    let err = fs
        .create("keep.bin", &vec![0xA5u8; 400 * 4096], 8)
        .expect_err("400 blocks do not fit in a 64-block volume");

    assert!(
        matches!(err, FsError::NoSpace { .. }),
        "expected NoSpace, got {err:?}",
    );
    assert_eq!(
        fs.read_file("keep.bin").expect("the file that was already here"),
        original,
        "the replacement failed and took the original with it",
    );
    assert_eq!(fs.file_mtime("keep.bin"), 7, "the old entry's mtime was rewritten");
}

#[test]
fn a_write_that_runs_out_of_space_gives_back_what_it_took() {
    let mut fresh = small_volume();
    let untouched = one_block_files_that_fit(&mut fresh);
    assert_eq!(
        untouched, FREE_BLOCKS_64,
        "the baseline is wrong, so the comparison below proves nothing",
    );

    let mut fs = small_volume();
    fs.create("big.bin", &vec![0u8; 400 * 4096], 0)
        .expect_err("400 blocks do not fit in a 64-block volume");

    assert_eq!(
        one_block_files_that_fit(&mut fs),
        untouched,
        "a create that failed kept the blocks it had already reserved",
    );
}

#[test]
fn a_metadata_update_that_cannot_be_reinserted_leaves_the_entry_alone() {
    let mut fs = small_volume();
    // Every free block spent, and the root leaf filled to within one entry's
    // growth of a split — so the reinsert has to split and the split has no
    // block to split into.
    for i in 0..FREE_BLOCKS_64 {
        fs.create(&format!("f{:02}", i), b"x", 100 + i as u64).expect("fill");
    }

    let (extents, _) = fs.file_extents("f00").expect("f00 is on the volume");
    let grown: Vec<Extent> = (0..16).map(|_| extents[0]).collect();

    let err = fs
        .update_metadata("f00", &grown, 1, 999)
        .expect_err("a 16-extent value needs a split this volume cannot pay for");
    assert!(
        matches!(err, FsError::NoSpace { .. }),
        "expected NoSpace, got {err:?}",
    );

    assert_eq!(
        fs.read_file("f00").expect("f00 after a metadata update that failed"),
        b"x",
    );
    assert_eq!(fs.file_mtime("f00"), 100, "the failed update left its mtime behind");
}
