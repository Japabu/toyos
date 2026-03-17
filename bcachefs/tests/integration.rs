use bcachefs::{Formatted, Mounted, ReadOnly, ReadWrite, VecBlockIO};

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
    assert_eq!(files[0].1, 13); // "Hello, world!" is 13 bytes

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
fn read_link() {
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("real.txt", b"real data", 0).expect("create file");
    fs.create_symlink("link.txt", "real.txt", 0).expect("create symlink");

    let mounted = fs.mount_readonly();

    // read_link should return target for symlink
    let target = mounted.read_link("link.txt");
    assert_eq!(target.as_deref(), Some("real.txt"));

    // read_link should return None for regular file
    assert_eq!(mounted.read_link("real.txt"), None);

    // read_link should return None for nonexistent
    assert_eq!(mounted.read_link("nope"), None);
}

#[test]
fn large_file_single_extent() {
    // 100KB file should be contiguous on a fresh filesystem
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
    // File that's exactly 4096 bytes (one block)
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
    // File that's 4097 bytes (spans two blocks)
    let data: Vec<u8> = (0..4097).map(|i| (i % 256) as u8).collect();
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("cross.bin", &data, 0).expect("create");

    let mounted = fs.mount_readonly();
    let read_data = mounted.read_file("cross.bin").expect("read");
    assert_eq!(read_data.len(), 4097);
    assert_eq!(read_data, data);
}

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
fn incremental_insert_and_read() {
    // Insert files one at a time, checking readability after each insert
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
        // Unmount and continue inserting
        fs = mounted.into_formatted();
    }
}

#[test]
fn many_files_triggers_node_split() {
    // Create enough files to overflow a single btree leaf node
    // Each entry is ~24 (key) + ~30 (value header + name) = ~54 bytes minimum
    // A 4KB node fits ~70 entries. Create 100 to ensure at least one split.
    let io = VecBlockIO::new(1024);
    let mut fs = Formatted::format(io);

    for i in 0..100 {
        let name = format!("file_{:04}.txt", i);
        let data = format!("content of file {}", i);
        fs.create(&name, data.as_bytes(), i as u64).expect(&format!("create {}", name));
    }

    let mounted = fs.mount_readonly();
    let files = mounted.list().expect("list after splits");
    assert_eq!(files.len(), 100, "expected 100 files, got {}", files.len());

    // Verify every file is readable with correct content
    for i in 0..100 {
        let name = format!("file_{:04}.txt", i);
        let expected = format!("content of file {}", i);
        let data = mounted.read_file(&name).unwrap_or_else(|e| {
            panic!("failed to read {}: {:?}", name, e);
        });
        assert_eq!(
            String::from_utf8(data).unwrap(),
            expected,
            "content mismatch for {}",
            name
        );
        assert_eq!(mounted.file_mtime(&name), i as u64, "mtime mismatch for {}", name);
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

    assert!(mounted.delete("a.txt"));
    assert_eq!(mounted.list().unwrap().len(), 1);
    assert!(mounted.read_file("a.txt").is_err());
    assert_eq!(mounted.read_file("b.txt").unwrap(), b"bbb");

    // Deleting nonexistent returns false
    assert!(!mounted.delete("nonexistent"));
}

#[test]
fn mounted_readwrite_delete_prefix() {
    let io = VecBlockIO::new(512);
    let fs = Formatted::format(io);
    let mut mounted = Mounted::<_, ReadWrite>::open(fs.into_io()).expect("open");

    mounted.create("bin/shell", b"shell", 0).expect("create");
    mounted.create("bin/editor", b"editor", 0).expect("create");
    mounted.create("lib/core.so", b"core", 0).expect("create");
    mounted.create("share/font", b"font", 0).expect("create");

    assert_eq!(mounted.list().unwrap().len(), 4);

    mounted.delete_prefix("bin/");
    let files = mounted.list().unwrap();
    assert_eq!(files.len(), 2, "expected 2 files after deleting bin/, got: {:?}", files);
    assert!(mounted.read_file("bin/shell").is_err());
    assert!(mounted.read_file("bin/editor").is_err());
    assert_eq!(mounted.read_file("lib/core.so").unwrap(), b"core");
    assert_eq!(mounted.read_file("share/font").unwrap(), b"font");
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
    assert_eq!(mounted.list().unwrap().len(), 1); // should not duplicate
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

    // Extract raw bytes, create new IO, reopen
    // We need to get the raw data out. Use into_formatted -> into_io
    let raw = mounted.into_formatted().into_io().into_vec();
    let io2 = VecBlockIO::from_vec(raw);
    let mounted2 = Mounted::<_, ReadOnly>::open(io2).expect("reopen");

    assert_eq!(mounted2.read_file("persistent.txt").unwrap(), b"I survive reboots");
    assert_eq!(mounted2.file_mtime("persistent.txt"), 42);
}

#[test]
fn superblock_backup_recovery() {
    // Create a filesystem, corrupt block 0, verify mount reads backup
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("test.txt", b"test data", 0).expect("create");
    let mut raw = fs.into_io().into_vec();

    // Corrupt block 0 (superblock)
    raw[0..4].copy_from_slice(b"JUNK");

    // Should still mount from backup at last block
    let io = VecBlockIO::from_vec(raw);
    let mounted = Mounted::<_, ReadOnly>::open(io).expect("mount from backup");
    let data = mounted.read_file("test.txt").expect("read after recovery");
    assert_eq!(data, b"test data");
}

#[test]
fn crc_verification_on_nodes() {
    // Create a filesystem, corrupt a btree node, verify read fails with ChecksumMismatch
    let io = VecBlockIO::new(128);
    let mut fs = Formatted::format(io);
    fs.create("test.txt", b"hello", 0).expect("create");
    let mut raw = fs.into_io().into_vec();

    // The root node is at block 2 (superblock=0, bitmap=1, root=2 for small fs)
    // Corrupt one byte in the middle of the root node
    let root_offset = 2 * 4096 + 100; // byte 100 of the root node
    raw[root_offset] ^= 0xFF;

    let io = VecBlockIO::from_vec(raw);
    let mounted = Mounted::<_, ReadOnly>::open(io).expect("mount");
    let result = mounted.read_file("test.txt");
    assert!(result.is_err(), "expected checksum error, got: {:?}", result.ok().map(|d| d.len()));
}
