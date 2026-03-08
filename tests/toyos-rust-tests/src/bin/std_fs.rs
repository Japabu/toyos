use std::fs;

fn main() {
    // List bin directory
    let entries: Vec<_> = fs::read_dir("/bin")
        .expect("should be able to read /bin")
        .filter_map(|e| e.ok())
        .collect();
    assert!(!entries.is_empty(), "/bin should not be empty");

    // Check that our own binary exists
    let self_exists = std::path::Path::new("/bin/test_rs_std_fs").exists();
    assert!(self_exists, "our own binary should exist in /bin");

    // Read our own binary
    let data = fs::read("/bin/test_rs_std_fs")
        .expect("should be able to read our own binary");
    assert!(!data.is_empty(), "binary should not be empty");

    // Non-existent file should return NotFound
    let err = fs::read("/bin/nonexistent").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    println!("all fs tests passed");
}
