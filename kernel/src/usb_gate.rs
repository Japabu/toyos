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
    if disk.read_blocks(0, 1, &mut head).is_err() {
        log!("usb-gate: disk {index} would not give up block 0");
        return;
    }
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
        if disk.read_blocks(block, 1, &mut buf).is_err() {
            reads_ok = false;
            log!("usb-gate: host block {block} could not be read");
            continue;
        }
        match first_bad(&buf, nonce, block) {
            None => log!("usb-gate: host block {block} verified"),
            Some((i, got, want)) => {
                reads_ok = false;
                log!("usb-gate: host block {block} differs at byte {i}: {got:#04x} not {want:#04x}");
            }
        }
    }

    // A read the device cannot serve, and the one assertion here that is about
    // the *error channel* rather than about bytes. `BlockDevice` returned `()`
    // until this landed, so a caller had no way to tell a refusal from data —
    // which is precisely how a failed fill used to reach the page cache and be
    // served under the block number it never held.
    let past_end = disk.read_blocks(blocks, 1, &mut buf).is_err();
    log!("usb-gate: read past the last block refused={past_end}");

    // A read whose *caller's* budget was already spent when it arrived — the
    // other half of the error channel, and the half no device state can
    // produce. `crate::block::OPERATION` is what bounds a device that answers
    // every transfer and takes too long over the work; no property of a host
    // device and no injection in this driver can stage a disk slow enough to
    // reach two seconds, so an operation that is already over is established
    // instead. What that proves is the plumbing and the verdict together: the
    // deadline reaches `XhciController::scsi` from above the driver, no command
    // is issued, and the disk is untouched — every assertion after this one
    // runs against a device this read must not have disturbed, which is why it
    // sits here and not at the end.
    //
    // Straight to the driver rather than through `BlockDevice`, so the line the
    // trait writes about a failed read is not in the capture for a read that was
    // never issued. Going through it would also work — `UsbBlockDevice` would
    // establish its own two seconds inside this one and an inner establishment
    // may only narrow — but this file is standing in for the caller of the
    // trait, and establishing exactly what that caller establishes is the
    // layering working rather than being dodged.
    //
    // `budget=` is the second half of the verdict and the reason the line is
    // not just `refused=`: a refusal that arrives as `BlockError::Device` is a
    // claim about the stick, and `/bin/logd` ends a boot's log on one. The two
    // were one value until 2026-08-22
    // (`issues/boot-media/fsync-on-log-returns-other-under-a-loaded-host.md`),
    // and collapsing them again reds here.
    let spent = {
        let _op = crate::scheduler::Operation::begin(crate::time::Deadline::passed());
        crate::drivers::xhci::storage_read(index, at(blocks, HOST_BLOCKS[0]), 1, &mut buf)
    };
    log!("usb-gate: read with a spent budget refused={} budget={}",
        spent.is_err(),
        spent == Err(crate::block::BlockError::BudgetExpired));

    // A read the controller cut short while the device's own CSW claims it
    // moved everything. Armed here rather than by a boot-wide count so the
    // transfer it lands on is a known one, and issued against a block the host
    // staged — so `matched` is a comparison with the host's bytes and not with
    // the guest's idea of them. The block read into that window immediately
    // before this one is a different LBA, which is what a caller is handed when
    // the two accounts disagree and only the device's is kept.
    #[cfg(feature = "boot-actuators")]
    if crate::actuator::usb_short_read() {
        let block = at(blocks, HOST_BLOCKS[0]);
        buf.fill(0);
        crate::drivers::xhci::arm_short_read();
        let refused = disk.read_blocks(block, 1, &mut buf).is_err();
        let matched = !refused && first_bad(&buf, nonce, block).is_none();
        log!("usb-gate: short read of block {block} refused={refused} matched={matched}");
    }

    // The guest's own bytes are keyed on the inverted nonce, so the host can
    // tell what the guest wrote from what it wrote itself — and a driver that
    // returned the wrong block cannot pass by returning a block that happens to
    // hold the right kind of data.
    let guest_nonce = !nonce;
    let mut writes_ok = true;
    // Counted separately from `writes_ok`, and reported separately, because
    // the two can disagree and the difference is the whole point: on a disk
    // that refuses writes the readback below fails either way, so a summary
    // that only said `writes=bad` would stay true with the error channel
    // deleted. This number is zero unless a write *said* it failed.
    let mut write_errors = 0usize;
    for index in GUEST_BLOCKS {
        let block = at(blocks, index);
        fill(&mut buf, guest_nonce, block);
        if disk.write_blocks(block, 1, &buf).is_err() {
            writes_ok = false;
            write_errors += 1;
            log!("usb-gate: block {block} refused the write");
        }
    }

    let mut run = vec![0u8; RUN_LEN as usize * BLOCK];
    for i in 0..RUN_LEN as u64 {
        let block = RUN_START + i;
        let at = i as usize * BLOCK;
        fill(&mut run[at..at + BLOCK], guest_nonce, block);
    }
    if disk.write_blocks(RUN_START, RUN_LEN, &run).is_err() {
        writes_ok = false;
        write_errors += 1;
        log!("usb-gate: the {RUN_LEN}-block run refused the write");
    }
    if disk.flush().is_err() {
        writes_ok = false;
        log!("usb-gate: the disk refused to flush");
    }

    let mut back = vec![0u8; RUN_LEN as usize * BLOCK];
    if disk.read_blocks(RUN_START, RUN_LEN, &mut back).is_err() {
        writes_ok = false;
        log!("usb-gate: the {RUN_LEN}-block run could not be read back");
    }
    if writes_ok && back != run {
        writes_ok = false;
        let at = back.iter().zip(&run).position(|(a, b)| a != b).unwrap_or(0);
        log!("usb-gate: readback of the {RUN_LEN}-block run differs at byte {at}");
    }
    for index in GUEST_BLOCKS {
        let block = at(blocks, index);
        buf.fill(0);
        if disk.read_blocks(block, 1, &mut buf).is_err() {
            writes_ok = false;
            log!("usb-gate: block {block} could not be read back");
            continue;
        }
        if let Some((i, got, want)) = first_bad(&buf, guest_nonce, block) {
            writes_ok = false;
            log!("usb-gate: readback of block {block} differs at byte {i}: {got:#04x} not {want:#04x}");
        }
    }

    log!(
        "usb-gate: disk done reads={} writes={} refusal={past_end} wr_err={write_errors} healthy={}",
        if reads_ok { "ok" } else { "bad" },
        if writes_ok { "ok" } else { "bad" },
        disk.healthy()
    );
}
