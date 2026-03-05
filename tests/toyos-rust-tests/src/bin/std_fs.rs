use std::fs;

fn main() {
    // Read the test manifest (we know it exists in the initrd)
    let manifest = fs::read_to_string("/initrd/test-manifest")
        .expect("should be able to read test-manifest");
    assert!(!manifest.is_empty(), "test-manifest should not be empty");

    // List initrd directory
    let entries: Vec<_> = fs::read_dir("/initrd")
        .expect("should be able to read /initrd")
        .filter_map(|e| e.ok())
        .collect();
    assert!(!entries.is_empty(), "/initrd should not be empty");

    // Check that our own binary exists
    let self_exists = std::path::Path::new("/initrd/test_rs_std_fs").exists();
    assert!(self_exists, "our own binary should exist in /initrd");

    println!("all fs tests passed");
}
