use std::fs::{self, File, OpenOptions};
use std::io::Write;

fn main() {
    let path = "/tmp/test_truncate";

    // Write 1000 bytes of pattern data
    {
        let mut f = File::create(path).expect("create failed");
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        f.write_all(&data).expect("write failed");
    }

    // Truncate to 500
    {
        let f = OpenOptions::new().write(true).open(path).expect("open for truncate failed");
        f.set_len(500).expect("set_len(500) failed");
    }

    // Read back and verify
    {
        let data = fs::read(path).expect("read failed");
        assert_eq!(data.len(), 500, "expected 500 bytes, got {}", data.len());
        for (i, &b) in data.iter().enumerate() {
            assert_eq!(b, (i % 256) as u8, "mismatch at byte {i}");
        }
    }

    // Truncate to 0
    {
        let f = OpenOptions::new().write(true).open(path).expect("open for truncate failed");
        f.set_len(0).expect("set_len(0) failed");
    }
    {
        let data = fs::read(path).expect("read after truncate to 0 failed");
        assert!(data.is_empty(), "expected empty file, got {} bytes", data.len());
    }

    // Extend with zeros
    {
        let f = OpenOptions::new().write(true).open(path).expect("open for extend failed");
        f.set_len(100).expect("set_len(100) failed");
    }
    {
        let data = fs::read(path).expect("read after extend failed");
        assert_eq!(data.len(), 100, "expected 100 bytes after extend");
        assert!(data.iter().all(|&b| b == 0), "extended bytes should be zero");
    }

    // Clean up
    fs::remove_file(path).ok();

    println!("all fs write tests passed");
}
