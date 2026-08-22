//! `nvme-spent-budget`: the in-guest half of the NVMe operation-budget gate.
//!
//! **What it stages is the caller and not the device.** `block::OPERATION`
//! bounds a controller that answers every command and takes too long over the
//! work one `read_blocks` is made of, and no property of a host device reaches
//! that: QEMU's NVMe answers in microseconds, `rerror`/`werror` fail a command
//! rather than delaying one, and no injection inside the driver can stage two
//! seconds without spending two seconds. So the gate establishes an operation
//! that is *already over* and asks for a read, which is exactly what a caller
//! arriving with a spent budget looks like from the driver's side. `usb_gate`
//! stages the same refusal on the USB path for the same reason and says so at
//! more length.
//!
//! **Under both page-cache locks, because that is the state the refusal has to
//! be reachable from.** Every real caller holds `page_cache::BLOCK_CACHE` and
//! `page_cache::BLOCK_DEV` across the whole of `read_blocks`, and both are
//! `sync::Lock`s that disable preemption for their whole life — so a wait that
//! could not end was a CPU wedged holding two of the machine's statics. The
//! read below goes through those two guards and not around them.
//!
//! **What it proves is the plumbing and the verdict together**: the deadline is
//! established above `BlockDevice`, `NvmeBlockDevice::read_blocks` narrows its
//! own two seconds into it rather than widening back out, no command is
//! submitted, and the disk is untouched — which is what the second read is for
//! and the only thing that could tell an abandoned command from a refused one.
//!
//! It runs before anything has mounted the device, so the block it asks for is
//! one nothing else is reading yet, and it writes nothing anywhere.

use alloc::vec;

use crate::page_cache;
use crate::scheduler::Operation;
use crate::time::Deadline;

/// The block the gate asks for. Any block would do — the refusal is taken
/// before the LBA reaches a command — and block 0 is the one every later reader
/// asks for anyway, so a device disturbed by this would show it immediately.
const AT: u64 = 0;

pub fn run() {
    let mut buf = vec![0u8; 4096];

    // The establishment is the *caller's*, above the trait, which is what makes
    // this the layering working rather than being dodged: `read_blocks` begins
    // its own two seconds inside this one, and an inner establishment may only
    // narrow, so what the driver recovers is this deadline.
    let refused = {
        let _op = Operation::begin(Deadline::passed());
        let mut guard = page_cache::lock();
        let (_cache, dev) = guard.cache_and_dev();
        dev.read_blocks(AT, 1, &mut buf)
    };
    // `budget=` is the second half of the verdict: `may_issue` refuses an
    // abandoned controller and a spent budget alike, and only one of the two is
    // a fact about this device. They were one value until 2026-08-22.
    log!("nvme-gate: read with a spent budget refused={} budget={}",
        refused.is_err(),
        refused == Err(crate::block::BlockError::BudgetExpired));

    // And the controller is where the previous operation left it. A refusal
    // taken inside a command instead of between two would have abandoned one:
    // the queue would still be owed a completion entry and the DMA window still
    // owed a write, and this read is the first thing that would find out.
    let mut guard = page_cache::lock();
    let (_cache, dev) = guard.cache_and_dev();
    let served = dev.read_blocks(AT, 1, &mut buf).is_ok();
    drop(guard);
    log!("nvme-gate: the same block read afterwards ok={served}");
}
