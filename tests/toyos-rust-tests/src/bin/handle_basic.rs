//! What a handle is, before anything is done with one.
//!
//! Nothing in the tree was this gate. Five properties of `HandleTable` that
//! every other gate assumes and none of them exercises:
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
//! 5. **And the reissuing stops.** A generation counter is finite, so property
//!    1 has an end; by owner ruling of 2026-08-20 a slot that reaches it
//!    retires for good rather than starting again, and the table is one slot
//!    smaller from then on.
//!
//! The census arm underneath them is what makes the whole file a leak test as
//! well: every object here is made and closed, per kind, so a handle path that
//! forgets to decrement is a named kind rather than a total that drifted.

use std::collections::BTreeSet;

use toyos::census::Census;
use toyos::AsHandle;
use toyos_abi::handle::{Rights, HANDLE_INVALID};
use toyos_abi::syscall::{self, debug_action, SyscallError};
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
    a_spent_slot_retires_and_the_table_is_one_smaller();
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
    let (_read, write) = toyos::pipe_pair().expect("a pipe of our own");

    // One handle freed and one taken, so which slot the table offers next is
    // not a question about the order two of them were dropped in: the free list
    // has exactly this one on it.
    let closed = syscall::dup(write.as_handle()).expect("a duplicate to close");
    syscall::close(closed);
    let reissued = syscall::dup(write.as_handle()).expect("a duplicate at the freed slot");
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
    syscall::close(reissued);
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
/// POSIX says `dup2` returns the new descriptor; here the answer carries a generation the
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

    // **And replacing a *live* slot keeps its generation, deliberately.** The
    // number is what a POSIX caller goes on using — `printf` writes to the
    // literal `1`, and `userland/libc`'s `dup2` hands back the raw value — so a
    // bump here would make every write after `dup2(pipe, 1)` `Stale`, which
    // ends the process. The cost is that a handle the caller still holds for
    // the displaced object now names the replacement, which is exactly what
    // `dup2` is for and is stated at `HandleTable::install_at`.
    //
    // **Witnessed through the rights and not through a data path**, because the
    // two objects have to be told apart by what the *table* says: a pipe write
    // end carries `WRITE` and a region carries `DUP|TRANSFER|MAP` and does not.
    let held = syscall::dup2(write.as_handle(), SPARE_SLOT).expect("dup2 onto the spare slot");
    let writable = syscall::dup_narrowed(held, Rights::WRITE)
        .expect("the spare slot does not name the pipe end that was put there");
    syscall::close(writable);

    let region = toyos::shm::SharedMemory::create(4096).expect("a region of our own");
    let over = syscall::dup2(region.as_handle(), SPARE_SLOT).expect("dup2 over a live slot");
    assert_eq!(over, held, "replacing a live slot moved its generation");
    // The number resolves — a `Stale` one would end this process rather than
    // answer — and what it names is the region, which has no `WRITE` to give.
    assert_eq!(
        syscall::dup_narrowed(held, Rights::WRITE),
        Err(SyscallError::PermissionDenied),
        "a replaced slot did not name its replacement",
    );
    syscall::close(over);
    println!("  dup2: slot {SPARE_SLOT} answered generation 0, then 1, and a live replace kept it");
}

/// A slot at its last generation serves one more lifecycle and is then gone
/// for good, and the table is one slot smaller for it.
///
/// **The owner's ruling of 2026-08-20, asserted near exhaustion rather than at
/// it.** A slot has 1,048,575 lifecycles; running one out for real is two
/// syscalls apiece, so the alternative to staging the last generation is a test
/// nobody runs — which is exactly why a wrap could sit in this table unnoticed.
/// [`debug_action::SLOT_TO_LAST_GENERATION`] moves the counter and nothing else:
/// every install, close and refusal below is the shipped path.
///
/// What is asserted is the whole policy. The slot answers once more, through
/// the ordinary allocating path and at the generation the ABI says is the last
/// one. After that close, no path issues it again — not `dup`, which allocates,
/// and not `dup2`, which names its own slot and so has no free list in front of
/// it. And the loss is *exactly* one slot: the set of slots the table will hand
/// out shrinks by this one and by nothing else, which is what says the ruling
/// cost a slot rather than a region of them.
///
/// **The slot this stages is the last one, and that is the sharpest place to
/// stage it.** The fill above grows the table to its cap and gives every slot
/// back in ascending order, so the free list hands out the highest one first —
/// slot 4095, whose encoding at `MAX_GENERATION` *is* `HANDLE_INVALID`. A tree
/// that reissues an exhausted slot therefore does not merely repeat a number
/// here; it hands out the one word the ABI promises no table ever issues, which
/// is what the reverted-ruling run answers with: `Ok(RawHandle(4294967295))`.
fn a_spent_slot_retires_and_the_table_is_one_smaller() {
    let (_read, write) = toyos::pipe_pair().expect("a pipe of our own");
    let source = write.as_handle();
    let before = every_slot_the_table_will_issue(source);
    println!("  table: {} slots free to this process of {}", before.len(), RawHandle::MAX_SLOTS);

    // A slot of its own, put one lifecycle from the end while it is free — so
    // no handle anybody holds becomes stale, and what comes back is the number
    // the next install of it answers.
    let doomed = syscall::dup(source).expect("a duplicate to spend");
    let slot = doomed.slot();
    syscall::close(doomed);
    let last = RawHandle::new(slot, RawHandle::MAX_GENERATION - 1);
    assert_eq!(
        syscall::debug_with(debug_action::SLOT_TO_LAST_GENERATION, u64::from(slot)),
        u64::from(last.0),
        "the actuator did not stage slot {slot} at generation {}",
        RawHandle::MAX_GENERATION - 1,
    );

    // **The one lifecycle it has left, served through the allocating path.**
    // The free list is last-in-first-out and nothing else here takes a handle,
    // so the next duplicate is this slot; if it ever is not, the assertion says
    // so rather than passing having asked nothing.
    let issued = syscall::dup(source).expect("a slot at its last generation is still a slot");
    assert_eq!(issued, last, "the staged slot was not the one reissued");
    assert_ne!(issued, HANDLE_INVALID, "a table issued the invalid handle");
    // A working handle and not just a number: it resolves, and to the pipe.
    let narrowed = syscall::dup_narrowed(issued, Rights::WRITE)
        .expect("the last generation does not name what was put in it");
    syscall::close(narrowed);
    syscall::close(issued);

    // And now there is no such slot. `dup2` is the path that names its own,
    // and the word is the cap's: a slot the table no longer has.
    assert_eq!(
        syscall::dup2(source, slot),
        Err(SyscallError::ResourceExhausted),
        "dup2 reissued slot {slot} after its generations ran out",
    );
    let after = every_slot_the_table_will_issue(source);
    assert!(!after.contains(&slot), "the allocating path issued retired slot {slot}");
    let lost: Vec<u16> = before.difference(&after).copied().collect();
    assert_eq!(lost, [slot], "the table lost slots other than the retired one");
    assert_eq!(
        after.len(),
        before.len() - 1,
        "the table is not one slot smaller: {} slots, then {}",
        before.len(),
        after.len(),
    );
    println!(
        "  retirement: slot {slot} served generation {} and is gone; {} slots, then {}",
        RawHandle::MAX_GENERATION - 1,
        before.len(),
        after.len(),
    );
}

/// Every slot the table will hand out right now, by filling it and giving it
/// all back.
///
/// **The only instrument userland has for the size of its own table**: nothing
/// asks the kernel how big one is, so what a table *is* is the set of slots it
/// answers with before it says `ResourceExhausted`. The refusal is a word and
/// not a kill — a full table is a resource limit, which `handle_kill_policy`
/// is the gate for — so this leaves the process able to say what it found.
fn every_slot_the_table_will_issue(source: RawHandle) -> BTreeSet<u16> {
    let mut taken = Vec::new();
    loop {
        match syscall::dup(source) {
            Ok(h) => taken.push(h),
            Err(SyscallError::ResourceExhausted) => break,
            Err(e) => panic!("filling the table answered {e:?}"),
        }
    }
    let slots = taken.iter().map(|h| h.slot()).collect();
    for h in taken {
        syscall::close(h);
    }
    slots
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
