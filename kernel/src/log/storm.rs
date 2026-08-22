//! `log-storm`: every CPU emitting patterned records at once, so a reader can
//! check a conservation law over them.
//!
//! **Nothing else can reach this.** A real workload's record rate is whatever
//! the kernel happens to log, which is a handful of lines a second and cannot
//! be made to saturate a shard however long a test waits — and the property
//! under test is exactly what happens when producers outrun the one reader and
//! the ring starts dropping.
//!
//! **Kernel threads at preempt depth 0 with `IF` set, one per shard**, so the
//! records really are written by every CPU at once and through the shipped
//! `emit`, the shipped reservation and the shipped publication. *One per
//! shard* is how many are spawned and not where they land — nothing in this
//! kernel pins a task to a CPU, and [`body`] says what that costs the gate.
//!
//! **What this workload cannot reach, measured rather than assumed**: a
//! producer that moves between CPUs *mid-storm*. A Ring 0 loop that takes no
//! lock is never preempted here — `need_resched` is polled by `preempt::enable`
//! and by the Ring 3 exit, and this loop reaches neither — and a producer that
//! parks and is woken is placed back on the CPU it left. Three shapes were
//! measured on 2026-08-15 at `--smp 8`: one thread per shard, two threads per
//! shard with an explicit `yield_now` every sixteen records, and two per shard
//! parking for 50 µs every sixteen. All three reported **0 of 8 (and 0 of 16)
//! producers with records on a second shard**. That is not bad luck and no
//! workload improves on it: nothing in this kernel switches a Ring 0 context
//! out between two instructions, so no producer can be moved off its CPU inside
//! the reservation window at any rate
//! (`issues/kernel/a-ring-0-loop-is-never-preempted.md`).
//! `log-nested-reserve`'s gate reaches that window instead, on one CPU, where
//! an interrupt is a stimulus a test can actually aim.
//!
//! **It starts when the reader does.** The storm exists to race a reader, so
//! arming it at boot would spend it before `test-runner` had opened a cursor;
//! the first `SYS_LOG_READ` of the boot is what spawns the threads, which makes
//! the overlap a property of the mechanism instead of of the harness's timing.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::sched::kthread::{self, OnPanic};

/// Records one storm thread emits before it says it is done.
///
/// **Bounded by the console rather than by the shards**: `klogd` renders every
/// record it manages to drain, and what it cannot keep up with is dropped from
/// the ring and counted — which is the behaviour under test, so the storm is
/// sized to be readable rather than to be complete. At the shipped
/// `SHARD_RECORDS` of 512 this laps every shard twice, so the loss path is
/// reached at every `--smp` the gate runs at.
const STORM_RECORDS: u64 = 1024;

/// Message bytes past the identity, derived from it.
///
/// **Long enough that a torn body is a body with a wrong middle**, not one with
/// a wrong first word: a record whose whole message fits in one machine word
/// would be published by a single store and no interleaving could split it.
/// This is 96 bytes, which is twelve of the slot's message words.
const PAYLOAD: usize = 96;

/// The `k=` field: a checksum over the two numbers that identify the record.
///
/// The reader regenerates the whole line from `t` and `i` and compares it byte
/// for byte, so this is not what catches a tear — the payload is. It is here
/// because a record has to be identifiable as *this* storm's from its own text
/// alone, and a number that is a function of both halves of the identity says
/// so more loudly than the two halves side by side.
pub fn checksum(thread: u64, index: u64) -> u64 {
    (thread.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ index.wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
        .rotate_left(17)
}

/// One storm record's payload byte, from the checksum and the offset. Lowercase
/// ASCII, so the record renders as text on the console like anything else.
pub fn payload_byte(checksum: u64, offset: usize) -> u8 {
    b'a' + (checksum.wrapping_add(offset as u64) % 26) as u8
}

/// One patterned record, from the two numbers that identify it and nothing
/// else.
///
/// **Public because the nesting gate emits these too**, from an interrupt
/// handler rather than from a storm thread: one text, one generator, and one
/// regeneration in the reader — so a body that came out of the wrong generation
/// fails on the byte that differs, wherever it was written.
pub fn emit_patterned(thread: u64, index: u64) {
    let checksum = checksum(thread, index);
    let mut payload = [0u8; PAYLOAD];
    for (offset, byte) in payload.iter_mut().enumerate() {
        *byte = payload_byte(checksum, offset);
    }
    // Every byte is lowercase ASCII by construction, so the fallback is
    // unreachable — and it is a fallback rather than an `expect` because a
    // panic here would be a producer halting the machine over its own
    // formatting.
    let payload = core::str::from_utf8(&payload).unwrap_or("");
    crate::log!("logstorm t={thread} i={index} k={checksum:016x} {payload}");
}

static STARTED: AtomicBool = AtomicBool::new(false);

/// Spawn one storm thread per shard, once for the life of the machine.
///
/// Called from `SYS_LOG_READ`'s implementation, which is what makes the storm
/// concurrent with a reader by construction.
pub fn start_once() {
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let threads = super::shard_count();
    // The reader learns the shape of the storm from the log it is reading,
    // rather than from a constant it would have to be kept in step with — and
    // this line is a record like any other, so a storm that laps a shard before
    // the first read returns may drop it. The reader derives the same number
    // from its cursor and treats this as a cross-check.
    crate::log!("logstorm start threads={threads} records={STORM_RECORDS}");
    for thread in 0..threads {
        // `Halt`: a storm thread that panics has taken the workload the gate's
        // verdict is computed over with it, and a machine that carried on would
        // answer with a conservation law over a storm that stopped early.
        kthread::spawn("logstorm", body, thread as u64, OnPanic::Halt);
    }
}

extern "C" fn body(thread: u64) -> ! {
    for index in 0..STORM_RECORDS {
        emit_patterned(thread, index);
    }
    // **The last record this producer writes, and the reader may never see
    // it.**
    //
    // `sched::driver::placement` picks the least-loaded CPU from a rotating
    // start, so the threads spawned back to back above land on distinct CPUs
    // only while every published load is equal; one CPU with a ready task at
    // that moment sends two of them to the same place, and a task is stealable
    // between its spawn and its first run either way. Two producers on one
    // shard means the first one's `done` is lapped by the second's records —
    // **observed twice in seven suites on the dev host, 2026-08-15**, each time
    // as the reader's whole 30 s ceiling.
    //
    // **So this is evidence and nothing waits on it.** A barrier that put every
    // `done` past every patterned record was tried and hung a 12-wide suite;
    // the reader was rewritten instead to decide from its own cursor, which
    // removes the class rather than this instance
    // (`userland/test-runner/src/log_gate.rs`). The record still declares what
    // this producer emitted and the reader cross-checks it wherever it
    // survives.
    crate::log!("logstorm done t={thread} emitted={STORM_RECORDS}");

    // **It parks rather than exiting**, because `sched::kthread`'s rows are
    // never removed and this thread has said everything it has to say. A
    // spinning thread here would go on competing with the reader for the whole
    // rest of the boot.
    crate::completion::park_forever();
}
