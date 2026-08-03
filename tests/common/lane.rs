//! One scratch directory per thread that boots guests.
//!
//! Every scratch file in this harness is named for what it is — `esp-boot.img`,
//! `usb-gate-512.img`, the size-keyed NVMe image `boot_with_options` makes — and
//! not for the boot that uses it, because until the suite ran serially there was
//! only ever one boot. Two guests handed one path is the defect
//! `specs/test-cost-audit.md` §5.1 records killing the first consolidation
//! attempt, and it is not a defect a per-file fix closes: the next test to add a
//! staged image gets the shared directory by default and nothing says otherwise.
//!
//! So the *directory* carries the answer. A worker enters its lane once and
//! every path derived from [`dir`] afterwards is that worker's alone, including
//! ones written after this was.
//!
//! Reuse **within** a lane is deliberate and is what the serial suite already
//! did: the NVMe scratch image is created by the first boot that wants that size
//! and mounted by the ones after it. A filtered `cargo test -- <name>` has
//! always given its first boot a blank disk, so no test may depend on a
//! particular predecessor — which is what makes a lane's first boot the same
//! situation.

use std::cell::Cell;
use std::path::PathBuf;

thread_local! {
    /// `None` on any thread that never entered one, which is the suite's own
    /// thread and every serial-tail test on it.
    static LANE: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Name this thread's lane for the rest of its life.
///
/// Called once by each worker before it takes any work. A lane is a property of
/// the *thread* rather than of the boot, because the paths it decides are
/// derived deep inside test bodies that have no business knowing the suite runs
/// wide.
pub fn enter(index: usize) {
    LANE.with(|lane| {
        assert!(lane.get().is_none(), "a worker entered two lanes");
        lane.set(Some(index));
    });
}

/// This thread's scratch directory, created if it is not there.
pub fn dir() -> PathBuf {
    let pid = std::process::id();
    let mut dir = std::env::temp_dir().join(format!("toyos-tests-{pid}"));
    if let Some(index) = LANE.with(Cell::get) {
        dir.push(format!("lane-{index}"));
    }
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("create the test directory {}: {e}", dir.display()));
    dir
}
