//! What a handle is, before anything is done with one.
//!
//! `specs/capability-endowment-spec.md` §8.6 requires this gate and nothing in
//! the tree was it. Four properties of `HandleTable` that every other gate
//! assumes and none of them exercises:
//!
//! 1. **A closed slot is reissued at the next generation**, so a handle number
//!    a process is still holding names *nothing* rather than whatever landed
//!    there next. That is the whole difference between a handle and the
//!    `PipeId`/`SharedToken`/service name it replaced, and it is invisible
//!    unless somebody looks at the number.
//! 2. **Rights only shrink.** A duplicate may ask for a subset; asking for a
//!    superset is refused, and the narrowed handle really is narrower.
//! 3. **A handle without `DUP` cannot be duplicated** — the property that makes
//!    a device claim's exclusivity the type's rather than a check in `dup`.
//! 4. **`SYS_HANDLE_DUP_AT` answers the slot's own generation and not the
//!    number that went in**, which is the one place this ABI deliberately
//!    breaks POSIX's `dup2` and the only thing that says so.
//!
//! The census arm underneath them is what makes the whole file a leak test as
//! well: every object here is made and closed, per kind, so a handle path that
//! forgets to decrement is a named kind rather than a total that drifted.

use toyos::census::Census;
use toyos::AsHandle;
use toyos_abi::handle::{Rights, HANDLE_INVALID};
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

/// A slot far above anything this process is using, so `dup2` onto it cannot
/// displace something the SDK is holding.
const SPARE_SLOT: u16 = 900;

/// Rounds the census is taken over. Large enough that one leaked object per
/// round is a number no drain lag can hide.
const ROUNDS: usize = 16;

fn main() {
    a_closed_slot_comes_back_at_a_new_generation();
    rights_only_shrink();
    a_claim_shaped_handle_cannot_be_duplicated();
    dup_at_answers_the_slot_not_the_number();
    nothing_here_leaks();
    println!("a handle is a slot, a generation and a rights word, and it gives itself back");
}

/// Close a handle, make another object, and require the new handle to be the
/// same slot at a later generation.
///
/// **The reissue is the point.** A table that reused the number would hand a
/// process holding the old one silent access to whatever landed there — which
/// is exactly what a `PipeId` did, and what `SYS_PIPE_OPEN` was retired for.
/// The old number must therefore be *unusable*, and it is: `Stale` is one of
/// the three kinds that end the caller, so this arm asserts the number rather
/// than trying to use it. `handle_kill_policy`'s `stale` child asserts the kill.
fn a_closed_slot_comes_back_at_a_new_generation() {
    let (read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let closed = read.as_handle();
    drop(read);
    drop(write);

    // The freed slot is at the front of the free list, so the next install
    // takes it. Nothing else in this process allocates between the two.
    let (read, write) = toyos::pipe_pair().expect("a second pipe of our own");
    let reissued = read.as_handle();
    assert_eq!(
        reissued.slot(),
        closed.slot(),
        "the freed slot was not reissued, so this arm never asked its question",
    );
    assert_eq!(
        reissued.generation(),
        closed.generation() + 1,
        "a reissued slot came back at the same generation: {closed:?} then {reissued:?}",
    );
    assert_ne!(reissued, closed, "a closed handle names the object that replaced it");
    assert_ne!(reissued, HANDLE_INVALID, "a table issued the invalid handle");
    drop(read);
    drop(write);
    println!("  reissue: slot {} came back at generation {}", closed.slot(), reissued.generation());
}

/// A duplicate may drop rights and may never add one.
fn rights_only_shrink() {
    let (read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let narrowed = syscall::dup_narrowed(write.as_handle(), Rights::DUP.union(Rights::WRITE))
        .expect("a subset of what the handle carries");

    assert_eq!(
        syscall::dup_narrowed(narrowed, Rights::ALL),
        Err(SyscallError::PermissionDenied),
        "a duplicate widened its own rights",
    );
    // And the narrowing is real rather than recorded: `WAIT` went, so a
    // blocking read of the *read* end narrowed the same way is refused.
    let no_read = syscall::dup_narrowed(read.as_handle(), Rights::DUP)
        .expect("a handle carrying only DUP is still a handle");
    let mut buf = [0u8; 4];
    assert_eq!(
        syscall::read_nonblock(no_read, &mut buf),
        Err(SyscallError::PermissionDenied),
        "a handle without READ read",
    );
    syscall::close(no_read);
    syscall::close(narrowed);
    println!("  rights: a subset is granted, a superset is PermissionDenied");
}

/// A handle that does not carry `DUP` is refused a duplicate.
///
/// This is what makes a `DeviceClaim`'s "at most one handle in existence" a
/// property of the object rather than a special case in `dup` — the claim is
/// simply created without the right. Asserted here on an ordinary object,
/// because the property belongs to the table and a test config's claims are
/// scarce.
fn a_claim_shaped_handle_cannot_be_duplicated() {
    let (_read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let undupable = syscall::dup_narrowed(write.as_handle(), Rights::WRITE)
        .expect("a handle carrying only WRITE");
    assert_eq!(
        syscall::dup(undupable),
        Err(SyscallError::PermissionDenied),
        "a handle without DUP was duplicated",
    );
    // It is a working handle otherwise, so the refusal was the right and not
    // the handle.
    syscall::write_nonblock(undupable, b"ok").expect("the narrowed handle still writes");
    syscall::close(undupable);
    println!("  no DUP: PermissionDenied, and the handle still works");
}

/// `dup2(h, slot)` answers `slot` at *that slot's* generation.
///
/// POSIX says `dup2` returns `newfd`; here the answer carries a generation the
/// caller has no business choosing, so it equals the bare slot number only
/// while that slot has never been closed. `userland/libc` says so at its own
/// `dup2` and this is the gate under it.
fn dup_at_answers_the_slot_not_the_number() {
    let (_read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let first = syscall::dup2(write.as_handle(), SPARE_SLOT).expect("dup2 onto a spare slot");
    assert_eq!(first, RawHandle::new(SPARE_SLOT, 0), "a fresh slot did not answer generation 0");

    syscall::close(first);
    let second = syscall::dup2(write.as_handle(), SPARE_SLOT).expect("dup2 onto it again");
    assert_eq!(second.slot(), SPARE_SLOT, "dup2 installed somewhere else");
    assert_eq!(
        second.generation(),
        1,
        "a reused slot answered its old generation, so the closed handle still names it",
    );
    syscall::close(second);
    println!("  dup2: slot {SPARE_SLOT} answered generation 0, then 1");
}

/// Every object above is made and closed, so nothing of any kind is left.
///
/// Per kind rather than in total: a total hides a leak of one kind behind churn
/// in another, which is how `File`, `Device`, `Acceptor`, `Connection`,
/// `IoUring` and `Console` went uncounted by every census assertion in the
/// estate.
fn nothing_here_leaks() {
    let before = round();
    let after = round();
    let grown: Vec<_> = after.grown_since(&before).collect();
    assert!(
        grown.is_empty(),
        "{ROUNDS} more rounds of handle churn left more live objects behind: {grown:?} — \
         first {before}, then {after}",
    );
    println!("  census: {} live objects, then {}", before.total(), after.total());
}

fn round() -> Census {
    for _ in 0..ROUNDS {
        let (read, write) = toyos::pipe_pair().expect("a pipe of our own");
        let dup = syscall::dup(write.as_handle()).expect("a duplicate");
        let narrowed =
            syscall::dup_narrowed(write.as_handle(), Rights::WRITE).expect("a narrowed duplicate");
        let (acceptor, connector) = toyos::port::create().expect("a port of our own");
        let region = toyos::shm::SharedMemory::create(4096).expect("a region of our own");
        syscall::close(dup);
        syscall::close(narrowed);
        drop(region);
        drop(acceptor);
        drop(connector);
        drop(read);
        drop(write);
    }
    Census::now()
}
