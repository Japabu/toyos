//! `usb-storage-gate`: the in-guest half of the USB mass-storage gate.
//!
//! The harness stages a disk on the host, boots with this feature on, and
//! checks the backing file afterwards. This is what runs inside: it verifies
//! blocks the *host* wrote and then writes blocks the host can check, so
//! neither half of the driver is certified by the other half of the driver.
//!
//! It exists as a kernel feature for the same reason `xhci-one-slot` does:
//! nothing else can stage it. A raw block device has no path to userland —
//! there is no syscall for one and writing a filesystem to reach it is a
//! different agent's work — so the only in-guest actor that can drive
//! `BlockDevice` is the kernel.
//!
//! **It never writes to a disk it was not given.** The target must carry a
//! stamp in block 0 naming its own block count, exactly as
//! `bcachefs_adapter::probe` requires before it will format `/home`. A disk
//! without the stamp is read once and left alone, which is what makes it safe
//! for this to sit next to the boot stick.

use alloc::vec;

use crate::block::BlockDevice;
use crate::drivers::usb_storage;

/// Block 0 of a disk this test owns. 16 bytes so the block count behind it is
/// 8-byte aligned in the image the harness writes.
const MAGIC: &[u8; 16] = b"TOYOS-USB-GATE1\0";
const AT_BLOCKS: usize = 16;
const AT_NONCE: usize = 24;

const BLOCK: usize = 4096;

/// Blocks the host wrote and the guest must read back unchanged.
const HOST_BLOCKS: [i64; 2] = [1, -1];
/// Blocks the guest writes and the host must find afterwards.
const GUEST_BLOCKS: [i64; 2] = [2, -2];
/// A run long enough to cross the driver's per-command batch, so the batching
/// loop is exercised rather than assumed: one SCSI command moves eight blocks.
const RUN_START: u64 = 4;
const RUN_LEN: u32 = 9;

/// The byte a given side is expected to have written.
///
/// Mirrored byte-for-byte by the harness. The nonce comes out of the stamp, so
/// a guest that never read block 0 cannot produce these bytes and an image left
/// over from an earlier run cannot pass for this one.
fn pattern(nonce: u64, block: u64, i: usize) -> u8 {
    let n = (nonce >> ((i % 8) * 8)) as u8;
    let b = (block ^ (block >> 13) ^ (block >> 27)) as u8;
    n ^ b.wrapping_mul(37) ^ (i as u8).wrapping_mul(101)
}

fn fill(buf: &mut [u8], nonce: u64, block: u64) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = pattern(nonce, block, i);
    }
}

/// Where a block does not match, or `None` if it does.
fn first_bad(buf: &[u8], nonce: u64, block: u64) -> Option<(usize, u8, u8)> {
    buf.iter().enumerate().find_map(|(i, &got)| {
        let want = pattern(nonce, block, i);
        (got != want).then_some((i, got, want))
    })
}

/// Resolve a possibly-negative block index against the disk's size.
fn at(blocks: u64, index: i64) -> u64 {
    if index >= 0 {
        index as u64
    } else {
        blocks.saturating_sub(index.unsigned_abs())
    }
}

pub fn run() {
    let disks = usb_storage::count();
    log!("usb-gate: {disks} disk(s) on the bus");
    for index in 0..disks {
        let Some(mut disk) = usb_storage::open(index) else { continue };
        check(index, &mut disk);
    }
    log!("usb-gate: sweep complete");
}

fn check(index: usize, disk: &mut usb_storage::UsbBlockDevice) {
    let blocks = disk.block_count();
    let mut head = vec![0u8; BLOCK];
    disk.read_blocks(0, 1, &mut head);
    if &head[..MAGIC.len()] != MAGIC {
        log!("usb-gate: disk {index} carries no stamp, leaving it alone");
        return;
    }
    // The stamp names the device it was written for, so a stamped image handed
    // to a guest whose disk is a different size is refused rather than written
    // at offsets that mean something else.
    let stamped = u64::from_le_bytes(head[AT_BLOCKS..AT_BLOCKS + 8].try_into().unwrap());
    if stamped != blocks {
        log!("usb-gate: disk {index} is stamped for {stamped} blocks and has {blocks}");
        return;
    }
    let nonce = u64::from_le_bytes(head[AT_NONCE..AT_NONCE + 8].try_into().unwrap());
    // The run has to fit with room for the two blocks addressed from the end.
    if blocks < RUN_START + RUN_LEN as u64 + 2 {
        log!("usb-gate: disk {index} has only {blocks} blocks, too few to test");
        return;
    }
    log!("usb-gate: disk {index} designated, blocks={blocks} nonce={nonce:#018x}");

    let mut reads_ok = true;
    let mut buf = vec![0u8; BLOCK];
    for index in HOST_BLOCKS {
        let block = at(blocks, index);
        buf.fill(0);
        disk.read_blocks(block, 1, &mut buf);
        match first_bad(&buf, nonce, block) {
            None => log!("usb-gate: host block {block} verified"),
            Some((i, got, want)) => {
                reads_ok = false;
                log!("usb-gate: host block {block} differs at byte {i}: {got:#04x} not {want:#04x}");
            }
        }
    }

    // The guest's own bytes are keyed on the inverted nonce, so the host can
    // tell what the guest wrote from what it wrote itself — and a driver that
    // returned the wrong block cannot pass by returning a block that happens to
    // hold the right kind of data.
    let guest_nonce = !nonce;
    for index in GUEST_BLOCKS {
        let block = at(blocks, index);
        fill(&mut buf, guest_nonce, block);
        disk.write_blocks(block, 1, &buf);
    }

    let mut run = vec![0u8; RUN_LEN as usize * BLOCK];
    for i in 0..RUN_LEN as u64 {
        let block = RUN_START + i;
        let at = i as usize * BLOCK;
        fill(&mut run[at..at + BLOCK], guest_nonce, block);
    }
    disk.write_blocks(RUN_START, RUN_LEN, &run);
    disk.flush();

    let mut back = vec![0u8; RUN_LEN as usize * BLOCK];
    disk.read_blocks(RUN_START, RUN_LEN, &mut back);
    let mut writes_ok = true;
    if back != run {
        writes_ok = false;
        let at = back.iter().zip(&run).position(|(a, b)| a != b).unwrap_or(0);
        log!("usb-gate: readback of the {RUN_LEN}-block run differs at byte {at}");
    }
    for index in GUEST_BLOCKS {
        let block = at(blocks, index);
        buf.fill(0);
        disk.read_blocks(block, 1, &mut buf);
        if let Some((i, got, want)) = first_bad(&buf, guest_nonce, block) {
            writes_ok = false;
            log!("usb-gate: readback of block {block} differs at byte {i}: {got:#04x} not {want:#04x}");
        }
    }

    log!(
        "usb-gate: disk done reads={} writes={} healthy={}",
        if reads_ok { "ok" } else { "bad" },
        if writes_ok { "ok" } else { "bad" },
        disk.healthy()
    );
}
