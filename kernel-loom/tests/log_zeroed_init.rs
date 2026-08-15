//! Host-fast regression for the AP shard's allocation path.
//!
//! Loom atomics are deliberately not byte-zeroable, so this test runs with
//! `--no-default-features` and exercises the real `core` layout and the real
//! in-place constructor used after `alloc_zeroed`.
//!
//! **The gate below therefore runs under exactly one of this crate's two
//! invocations, and the other one runs nothing from this file.** That is the
//! shape, not an accident — but it was silent until 2026-08-14, when CI ran
//! only the default invocation and this file's `running 0 tests` looked
//! identical to a pass. `.github/workflows/host-tests.yml` names both commands
//! now, and `specs/issues/build/kernel-loom-ungated-models-red-without-loom.md`
//! is why the second one is scoped to this target rather than to the crate.

#![cfg(not(feature = "loom"))]

use kernel_loom::log_shard::{Shard, FIRST_SEQ};
use std::alloc::{alloc_zeroed, dealloc, Layout};
use toyos_abi::log::LogRecord;

#[test]
fn a_zero_allocated_ap_shard_issues_first_seq_first() {
    let layout = Layout::new::<Shard>();
    let ptr = unsafe { alloc_zeroed(layout) }.cast::<Shard>();
    assert!(!ptr.is_null());

    // Negative witness: allocation alone leaves the first field — `head` by
    // the layout assertion in `shard.rs` — at zero. Publishing that pointer
    // directly is the reviewed defect and would issue the empty-state number.
    let zero_head = unsafe { ptr.cast::<u64>().read() };
    assert_eq!(zero_head, 0);
    assert_ne!(zero_head, FIRST_SEQ);

    // SAFETY: `ptr` is a fresh, aligned, zeroed allocation and remains private
    // to this test until initialization completes.
    unsafe { Shard::initialize_zeroed(ptr) };
    let shard = unsafe { &*ptr };

    assert_eq!(shard.head(), FIRST_SEQ);
    assert!(
        shard.read(0).is_none(),
        "zero must remain the empty-slot state"
    );

    let guard = kernel_loom::arch::LogCommitGuard::close();
    // SAFETY: this host thread is the allocation's only producer.
    let first = unsafe { shard.reserve(&guard) };
    assert_eq!(
        first, FIRST_SEQ,
        "an AP's first record must not collide with zero"
    );
    let mut record = LogRecord::EMPTY;
    record.seq = first;
    record.len = 1;
    record.msg[0] = b'A';
    // SAFETY: `first` came from this shard and is published exactly once under
    // the same guard.
    unsafe { shard.commit(first, &record, &guard) };
    let read_back = shard
        .read(first)
        .expect("an AP's first record must be readable");
    assert_eq!(read_back.seq, first);
    assert_eq!(&read_back.msg[..read_back.len as usize], b"A");
    drop(guard);

    // SAFETY: no references survive the deallocation and `Shard` has no drop
    // glue. The allocation came from this exact layout.
    unsafe { dealloc(ptr.cast(), layout) };
}
