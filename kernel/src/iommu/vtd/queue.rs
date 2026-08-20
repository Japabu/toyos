//! Queued invalidation: telling the unit that what it cached is no longer what
//! memory says.
//!
//! **This is synchronous** — a descriptor followed by an Invalidation Wait
//! descriptor with a status write, polled to completion — and the wait is
//! bounded by a named constant whose expiry is a panic. A unit that will not
//! acknowledge an invalidation has left the kernel unable to say what a device
//! can reach, and there is no safe way to continue from that.
//!
//! A unit without `ECAP.QI` could be served through `CCMD_REG` and
//! `IOTLB_REG` instead, which is correct and slower. No such register path is
//! written here: every unit anyone can boot has queued invalidation, so that
//! path would be an arm no machine in reach executes. A unit that lacks it
//! is left unprogrammed and says so, which is the eventual refusal one stage
//! early and costs that machine nothing it has today.

use crate::mm::Mmio;
use crate::time::{Duration, Tripwire};

use super::table::{Table, Tables};
use super::{FSTS_REG, IQA_REG, IQT_REG};

/// 4 KiB of descriptors: 256 of 16 bytes, which is `IQA.QS = 0` and the
/// smallest queue the architecture defines.
const QUEUE_ENTRIES: usize = 256;

/// Descriptor types.
const CONTEXT_CACHE: u64 = 0x1;
const IOTLB: u64 = 0x2;
const WAIT: u64 = 0x5;

/// Granularity field, bits 5:4. `01` is global — every domain, every entry.
const GLOBAL: u64 = 1 << 4;
/// Drain writes / drain reads: the unit does not report the invalidation done
/// until transactions already in flight against the old translation have
/// completed. Set on every IOTLB invalidation here, because "the entry is
/// gone" is not the claim `Flushed` will make at I4 — "nothing is still using
/// it" is.
const DRAIN_WRITES: u64 = 1 << 6;
const DRAIN_READS: u64 = 1 << 7;

/// Status write, and the value the unit writes.
const WAIT_STATUS_WRITE: u64 = 1 << 5;
const WAIT_DONE: u32 = 1;

/// `FSTS` bits that mean the queue itself went wrong: a descriptor the unit
/// would not decode, a completion it could not write, a timeout on a device
/// TLB. Any of them stalls the head, so a wait that never completes should say
/// which one rather than only that it waited.
const FSTS_QUEUE_ERRORS: u32 = (1 << 4) | (1 << 5) | (1 << 6);

/// How long the unit is given to acknowledge. Linux uses one second for the
/// same wait; this is not a measurement of anything and does not pretend to
/// be, it is the bound past which the kernel gives up rather than spins for
/// ever. Expiry is a panic (§5.5): what a device can reach is unknown from
/// there.
const ACK_TIMEOUT: Tripwire = Tripwire::absurd(
    Duration::from_secs(1),
    "Linux waits one second for the same acknowledgement, and what a device can reach is unknown past it",
);

pub struct Queue {
    descriptors: Table,
    /// The unit writes its completion here. Its own page: the descriptor ring
    /// uses all 4 KiB of its own, and a status word inside it would be a
    /// descriptor slot the unit also reads.
    status: Table,
    tail: usize,
}

impl Queue {
    /// Allocate the ring and point the unit at it. The caller enables
    /// `GCMD.QIE` afterwards — arming a queue the unit has not been told about
    /// is a queue it never reads.
    pub fn new(tables: &mut Tables, regs: Mmio) -> Self {
        let queue = Self { descriptors: tables.alloc(), status: tables.alloc(), tail: 0 };
        // Descriptor width 0 (128-bit) and queue size 0 (256 entries), so the
        // register is the base address and nothing else.
        regs.write_u64(IQA_REG, queue.descriptors.phys());
        regs.write_u64(IQT_REG, 0);
        queue
    }

    /// Every cached translation and every cached context entry, gone, and the
    /// unit has said so before this returns.
    ///
    /// Both directions, always, and never a branch on `CAP.CM` (§5.5): the
    /// arm that skips an invalidation is the arm that is right on hardware and
    /// wrong under the only configuration a test can stage, and code that
    /// always invalidates is code the harness can certify.
    pub fn invalidate_all(&mut self, regs: Mmio) {
        self.submit(
            regs,
            &[
                (CONTEXT_CACHE | GLOBAL, 0),
                (IOTLB | GLOBAL | DRAIN_WRITES | DRAIN_READS, 0),
            ],
        );
    }

    fn submit(&mut self, regs: Mmio, descriptors: &[(u64, u64)]) {
        assert!(
            descriptors.len() + 1 < QUEUE_ENTRIES,
            "iommu: {} descriptors do not fit a {QUEUE_ENTRIES}-entry queue",
            descriptors.len()
        );
        self.status.write_u32(0, 0);
        for &(lo, hi) in descriptors {
            self.descriptors.write_pair(self.tail, lo, hi);
            self.tail = (self.tail + 1) % QUEUE_ENTRIES;
        }
        self.descriptors.write_pair(
            self.tail,
            WAIT | WAIT_STATUS_WRITE | ((WAIT_DONE as u64) << 32),
            self.status.phys(),
        );
        self.tail = (self.tail + 1) % QUEUE_ENTRIES;

        regs.write_u64(IQT_REG, (self.tail * 16) as u64);

        let deadline = crate::clock::nanos_since_boot() + ACK_TIMEOUT.nanos();
        while self.status.read_device_u32(0) != WAIT_DONE {
            let faults = regs.read_u32(FSTS_REG);
            assert!(
                faults & FSTS_QUEUE_ERRORS == 0,
                "iommu: the unit rejected an invalidation descriptor, FSTS={faults:#010x}"
            );
            assert!(
                crate::clock::nanos_since_boot() < deadline,
                "iommu: the unit did not acknowledge an invalidation within {} ns, \
                 FSTS={faults:#010x}",
                ACK_TIMEOUT.nanos()
            );
            core::hint::spin_loop();
        }
    }
}
