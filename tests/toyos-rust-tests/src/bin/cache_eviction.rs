//! Sustained pressure on both disk caches, and the one question a cache that
//! evicts has to answer: does a page that was thrown away come back as the
//! bytes that were written?
//!
//! Everything here is sized in pages against the `test-small-caches` budget of
//! 64 of them. `BIG_PAGES` is twice that, so the forward pass evicts the head
//! of the file before it reaches the tail and the reverse pass then re-reads
//! every page in the order that guarantees a miss on each one. `SMALL_FILES`
//! exists because the eviction sweep walks (file, page) order, and a single
//! file cannot exercise the step from one file to the next.
//!
//! At the shipped budget the same run fits in cache and proves only the round
//! trip, which is why the harness asserts on the kernel's own eviction series
//! rather than on this exit code alone.
//!
//! The ceiling on `BIG_PAGES` is the filesystem, not the cache: bcachefs keeps
//! a file's extent list inline in its btree value and `resolve_or_alloc_block`
//! pushes one extent per page without merging contiguous ones, so a value is
//! `19 + name + 16 * pages` bytes against a 4040-byte node payload — about 250
//! pages for a name this long, and one page past that panics the kernel from
//! an ordinary `write` + `fsync` (known-issues, filed separately). 128 leaves
//! that margin doubled.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};

const PAGE: usize = 4096;
const BIG: &str = "/home/cache_big.bin";
const BIG_PAGES: usize = 128;
const SMALL_FILES: usize = 8;
const SMALL_PAGES: usize = 32;

/// Distinct per (file, page, byte), so a page served from the wrong slot, from
/// the wrong file, or half-written is a mismatch rather than a coincidence.
fn byte_at(tag: usize, page: usize, i: usize) -> u8 {
    let mixed = (tag.wrapping_mul(0x9E37_79B9))
        ^ (page.wrapping_mul(0x85EB_CA6B))
        ^ (i.wrapping_mul(0xC2B2_AE35));
    (mixed >> 11) as u8
}

fn page_bytes(tag: usize, page: usize) -> Vec<u8> {
    (0..PAGE).map(|i| byte_at(tag, page, i)).collect()
}

fn write_file(path: &str, tag: usize, pages: usize) {
    let mut f = fs::File::create(path).unwrap_or_else(|e| panic!("create {path}: {e}"));
    for page in 0..pages {
        f.write_all(&page_bytes(tag, page))
            .unwrap_or_else(|e| panic!("write {path} page {page}: {e}"));
    }
    f.sync_all().unwrap_or_else(|e| panic!("fsync {path}: {e}"));
}

fn check_page(f: &mut fs::File, path: &str, tag: usize, page: usize, why: &str) {
    f.seek(SeekFrom::Start((page * PAGE) as u64))
        .unwrap_or_else(|e| panic!("seek {path} page {page}: {e}"));
    let mut got = vec![0u8; PAGE];
    f.read_exact(&mut got)
        .unwrap_or_else(|e| panic!("read {path} page {page}: {e}"));
    let want = page_bytes(tag, page);
    if let Some(i) = got.iter().zip(&want).position(|(a, b)| a != b) {
        panic!(
            "{path} page {page} byte {i}: {} != {} ({why})",
            got[i], want[i]
        );
    }
}

fn main() {
    write_file(BIG, 0, BIG_PAGES);

    // Forward: the pass that fills the cache and then keeps going.
    let mut f = fs::File::open(BIG).expect("reopen big file");
    for page in 0..BIG_PAGES {
        check_page(&mut f, BIG, 0, page, "forward pass");
    }
    // Backward: with a cache smaller than the file, the page the forward pass
    // ended on is the only one still resident, so every step from here is a
    // miss that has to be satisfied from the backing.
    for page in (0..BIG_PAGES).rev() {
        check_page(&mut f, BIG, 0, page, "reverse pass");
    }
    drop(f);

    for tag in 0..SMALL_FILES {
        write_file(&format!("/home/cache_small_{tag}.bin"), tag + 1, SMALL_PAGES);
    }

    // Interleaved across files, so the sweep's file-to-file step is on the
    // path every time and no single file's pages are the only candidates.
    let mut handles: Vec<fs::File> = (0..SMALL_FILES)
        .map(|tag| fs::File::open(format!("/home/cache_small_{tag}.bin")).expect("reopen small"))
        .collect();
    for _round in 0..3 {
        for page in 0..SMALL_PAGES {
            for (tag, f) in handles.iter_mut().enumerate() {
                let path = format!("/home/cache_small_{tag}.bin");
                check_page(f, &path, tag + 1, page, "interleaved pass");
            }
        }
    }
    drop(handles);

    // A file the cache still holds an fd for, truncated and rewritten shorter:
    // the pages past the new end must not survive as stale cache entries.
    {
        let mut f = fs::OpenOptions::new().write(true).open(BIG).expect("reopen for truncate");
        f.set_len((4 * PAGE) as u64).expect("truncate");
        f.seek(SeekFrom::Start(0)).expect("rewind");
        for page in 0..4 {
            f.write_all(&page_bytes(9, page)).expect("rewrite");
        }
        f.sync_all().expect("fsync truncated");
    }
    let back = fs::read(BIG).expect("read truncated file");
    assert_eq!(back.len(), 4 * PAGE, "truncated file is the wrong length");
    for page in 0..4 {
        let want = page_bytes(9, page);
        let got = &back[page * PAGE..(page + 1) * PAGE];
        let bad = got.iter().zip(&want).position(|(a, b)| a != b);
        assert!(bad.is_none(), "rewritten page {page} byte {} differs", bad.unwrap());
    }

    let pages = BIG_PAGES * 2 + SMALL_FILES * SMALL_PAGES * 3;
    println!("cache eviction ok: {pages} page reads verified");
}
