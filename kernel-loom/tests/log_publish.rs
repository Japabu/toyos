//! Loom: what publishing a shard's pointer publishes.
//!
//! **The pointer is not the payload.** An AP's shard is `alloc_zeroed` memory
//! whose `head` the BSP writes before the pointer is stored; a reader that saw
//! the pointer without that ordering would read whatever the heap held under a
//! slot's sequence number and accept it if it happened to equal the number it
//! asked for. On x86 every store is a release and this cannot happen. **ARM64
//! is planned.**
//!
//! The negative case is a cargo feature rather than a comment:
//!
//! ```text
//! cargo test --manifest-path kernel-loom/Cargo.toml --features shard-publish-relaxed \
//!   --test log_publish
//! ```
//!
//! makes both sides of the pointer's publication relaxed and this file must red
//! — loom answers `Causality violation: Concurrent load and mut accesses`,
//! which is the reader reaching the shard's own words while the publisher is
//! still building them. Verified 2026-08-16, both ways round.
//!
//! `specs/log-architecture-spec.md` §2.2.

#![cfg(feature = "loom")]

use kernel_loom::log_registry::{published, publish, slots};
use kernel_loom::log_shard::{Shard, FIRST_SEQ};
use loom::sync::Arc;

/// **W5 — publication.** The `Release`/`Acquire` pair puts the shard's
/// construction ahead of the pointer, so a reader that finds the shard finds it
/// built.
///
/// The negative is the whole test: weaken either side to `Relaxed` and the
/// reader can see the pointer with `head` still zero, which reads back as a
/// shard that has never answered for anything — a machine that lost a CPU's
/// whole log rather than a machine that noticed.
#[test]
fn a_reader_that_finds_a_shard_finds_it_built() {
    loom::model(|| {
        let registry = Arc::new(slots());
        let publisher = registry.clone();

        let w = loom::thread::spawn(move || {
            // What `alloc_log_shard` does, in the order it does it: build the
            // shard, then make it reachable.
            let shard = Box::into_raw(Box::new(Shard::new()));
            // SAFETY: leaked, so it outlives every reader; published once.
            unsafe { publish(&publisher[..], 1, shard) };
        });

        // The reader races the publication and must see nothing, or a shard
        // whose reservation counter is the one the constructor wrote.
        if let Some(shard) = published(&registry[..], 0) {
            assert_eq!(
                shard.head(),
                FIRST_SEQ,
                "a shard reached before its construction: `head` is still the allocator's zero, \
                 so every sequence number this CPU issues reads back as absent"
            );
        }

        w.join().unwrap();
        let shard = published(&registry[..], 0).expect("published once the writer has joined");
        assert_eq!(shard.head(), FIRST_SEQ);
    });
}

/// A slot nobody published answers with nothing rather than with a null
/// dereference, which is the state every AP is in until the BSP reaches it.
#[test]
fn an_unpublished_slot_answers_for_nothing() {
    loom::model(|| {
        let registry = slots();
        for ap in 0..registry.len() {
            assert!(published(&registry[..], ap).is_none());
        }
        assert!(published(&registry[..], registry.len()).is_none());
    });
}
