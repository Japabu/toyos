//! A file round trip on `/home`, which is the only mount backed by the NVMe
//! device. Every other filesystem test reads `/bin` out of the initrd or
//! writes `/tmp` into tmpfs, so nothing else in the suite makes the block
//! layer allocate, cache and write back a block.

use std::fs;
use std::io::Write;

const PATH: &str = "/home/nvme_roundtrip.bin";

/// Three whole blocks and a partial fourth: enough that the file needs an
/// extent rather than a single block, and that the tail is the case an
/// off-by-one in the block mapping gets wrong.
const LEN: usize = 3 * 4096 + 17;

fn main() {
    let data: Vec<u8> = (0..LEN).map(|i| (i.wrapping_mul(31) ^ 0xA5) as u8).collect();

    {
        let mut f = fs::File::create(PATH).expect("create on /home");
        f.write_all(&data).expect("write to /home");
        f.sync_all().expect("fsync /home file");
    }

    let back = fs::read(PATH).expect("read back from /home");
    assert_eq!(back.len(), data.len(), "length changed across the round trip");
    let mismatch = back.iter().zip(&data).position(|(a, b)| a != b);
    assert!(mismatch.is_none(), "byte {} differs after the round trip", mismatch.unwrap());

    println!("nvme round trip ok: {LEN} bytes on /home");
}
