use std::fs;

fn main() {
    // List initrd directory
    let entries: Vec<_> = fs::read_dir("/initrd")
        .expect("should be able to read /initrd")
        .filter_map(|e| e.ok())
        .collect();
    assert!(!entries.is_empty(), "/initrd should not be empty");

    // Check that our own binary exists
    let self_exists = std::path::Path::new("/initrd/test_rs_std_fs").exists();
    assert!(self_exists, "our own binary should exist in /initrd");

    // Read our own binary
    let data = fs::read("/initrd/test_rs_std_fs")
        .expect("should be able to read our own binary");
    assert!(!data.is_empty(), "binary should not be empty");

    // Non-existent file should return NotFound
    let err = fs::read("/initrd/nonexistent").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    println!("all fs tests passed");
}
