//! A file bigger than the filesystem could describe, and a name bigger than it
//! will accept. Both used to panic the kernel from ordinary userland calls.
//!
//! bcachefs keeps a file's extent list inline in its btree value, and
//! `resolve_or_alloc_block` pushed one extent per page without merging
//! contiguous ones — so the value grew by 16 bytes per page against a
//! 4040-byte node payload, and at roughly 250 pages `Node::write_to`
//! underflowed `MAX_PAYLOAD - used`. `rename` reached the same line by a
//! shorter road: it was the one name-taking entry point with no length bound,
//! and `user_ptr::MAX_USER_STR` lets 64 KiB through.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};

const PAGE: usize = 4096;
/// Four times the old cap. Merging makes a sequentially written file one
/// extent whatever its length, so this is a claim about the *shape* of the
/// value and not about 4 MB in particular.
const PAGES: usize = 1024;
const PATH: &str = "/home/fs_large.bin";

fn page_bytes(page: usize) -> Vec<u8> {
    (0..PAGE)
        .map(|i| ((page.wrapping_mul(0x9E37_79B9) ^ i.wrapping_mul(0x85EB_CA6B)) >> 11) as u8)
        .collect()
}

fn main() {
    {
        let mut f = fs::File::create(PATH).expect("create on /home");
        for page in 0..PAGES {
            f.write_all(&page_bytes(page)).expect("write a page");
        }
        // The fsync is the trigger: flush walks every dirty page allocating a
        // block each, then writes one metadata entry describing all of them.
        f.sync_all().expect("fsync a 1024-page file");
    }

    let mut f = fs::File::open(PATH).expect("reopen");
    let meta = f.metadata().expect("stat");
    assert_eq!(meta.len(), (PAGES * PAGE) as u64, "length changed across the round trip");

    for page in [0, 1, 249, 250, 251, PAGES / 2, PAGES - 1] {
        f.seek(SeekFrom::Start((page * PAGE) as u64)).expect("seek");
        let mut got = vec![0u8; PAGE];
        f.read_exact(&mut got).expect("read");
        let want = page_bytes(page);
        let bad = got.iter().zip(&want).position(|(a, b)| a != b);
        assert!(bad.is_none(), "page {page} byte {} differs", bad.unwrap());
    }
    drop(f);

    // A name no btree value can hold is untrusted input, so it owes an error.
    let huge = format!("/home/{}", "n".repeat(4096));
    let renamed = fs::rename(PATH, &huge);
    assert!(renamed.is_err(), "rename accepted a 4096-byte name");
    assert!(
        fs::File::open(PATH).is_ok(),
        "the rejected rename took the source file with it",
    );

    let created = fs::File::create(&huge);
    assert!(created.is_err(), "create accepted a 4096-byte name");

    fs::remove_file(PATH).expect("cleanup");
    println!("large file ok: {PAGES} pages round-tripped, oversized names refused");
}
