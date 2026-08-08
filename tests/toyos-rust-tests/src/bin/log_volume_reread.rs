//! A write into a page of a `/log` file that the device will not give back.
//!
//! The host puts the file on the volume before this machine exists, so its
//! bytes are on the stick and none of its pages are in the file cache. Writing
//! *inside* it is therefore a partial write into a page that has to be fetched
//! first — `file_cache::write_page` re-reads through `FatBacking::read_page` and
//! merges — which is the one code path a failed read can silently turn into a
//! page of zeros written back over real data.
//!
//! The kernel's log sink used to reach this path by itself, when a boot reopened
//! the `kernel.log` the boot before it left. One file per boot ended that: the
//! sink now always creates, its own pages stay resident for the whole boot, and
//! nothing it does re-reads from the stick. The hazard in `write_page` is
//! unchanged and reachable by anything that appends to an existing file, which
//! is what this is.
//!
//! The *read* of the same page is the other half and is the sharper of the two:
//! `file_cache::read_page` returned `()`, so a page the device would not give
//! back reached the process as zeros under a success — which nothing above it
//! can tell from a file that really is zeros there.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

/// Staged by the host in `tests/common/volumes.rs`. Two halves of one fixture.
const PATH: &str = "/log/staged-reread.txt";
/// Inside the staged bytes, so the page is fetched rather than extended: past
/// the end there is nothing to preserve and `write_page` does not read at all.
const AT: u64 = 50;

fn main() {
    // First, and it costs the write below nothing: a fetch that failed is
    // deliberately never made resident, so the write still misses and still
    // re-fetches the same page.
    match std::fs::read(PATH) {
        Ok(bytes) => println!(
            "reread: the read succeeded with {} bytes, {} of them zero",
            bytes.len(),
            bytes.iter().filter(|b| **b == 0).count()
        ),
        Err(e) => println!("reread: the read failed: {e}"),
    }

    let mut file = match OpenOptions::new().write(true).open(PATH) {
        Ok(file) => file,
        Err(e) => {
            println!("reread: {PATH} did not open: {e}");
            return;
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(AT)) {
        println!("reread: seek failed: {e}");
        return;
    }
    match file.write_all(b"XXXXXXXX") {
        // The defect: the read failed, the cache merged into zeros, and the
        // caller was told the write worked.
        Ok(()) => println!("reread: the write succeeded"),
        Err(e) => println!("reread: the write failed: {e}"),
    }
}
