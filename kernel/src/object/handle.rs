//! One process's handle table.
//!
//! Lives inside `ProcessData`, behind the lock that is already there — no new
//! lock and no new ordering edge. Every accessor hands back an **owned** value:
//! no borrow into the table can outlive the guard, which is what stops a
//! syscall holding a reference into a table another thread of the same process
//! is editing.
//!
//! **A slot's generations are finite and what happens at the end of them is a
//! security decision, not an overflow.** A handle carries twelve bits of slot
//! and twenty of generation, so a slot has 1,048,575 lifecycles; a table that
//! wrapped would hand the holder of an ancient handle a live object again,
//! which is a use-after-free of authority however improbable. It does not wrap:
//! by owner ruling of 2026-08-20 a slot at its last generation **retires**, and
//! [`Slot`] is the shape that decision takes — a retired slot has no generation
//! to be issued at, so no insertion path can offer one by forgetting to look.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use toyos_abi::handle::{RawHandle, Rights};
use toyos_abi::syscall::SyscallError;

use super::{KObjectRef, KObjectVariant};

/// The table has no slot left. The one failure of an *install*, which is why it
/// is its own type: [`HandleError::refuse`] may take the process down, and the
/// object layer installs under the process's own lock where it may not be
/// called. A type with one state cannot carry a kind that kills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableFull;

/// Why a handle did not resolve.
///
/// **Three of these are bugs in the process that named the handle and two are
/// not.** A process may legitimately hold an attenuated handle and probe what
/// it can do with it, so `Rights` is an error return for ever, and a table with
/// no room is a resource limit. `BadHandle`, `Stale` and `WrongType` are
/// different: a handle is a local name a process was given, so naming one it
/// does not hold — or asking a pipe to accept a connection — is not something a
/// correct program can do. Fail-fast is for bugs, so [`refuse`] takes the
/// process down for those three rather than handing back a word it can ignore.
///
/// **The rule has exactly one named exception, and by owner ruling of
/// 2026-08-19 there is not a second.** The exception is the connector argument
/// to `SYS_NAMESPACE_BUILD`: an added connector is routinely one a *peer*
/// transferred, so `WrongType` there is not provably the caller's bug, and
/// faulting on it let any process holding the `launcher` connector end
/// `/bin/init` by sending it a pipe (`arch::syscall::sys_namespace_build`).
/// A spawn's slot map was the candidate for a second — it skipped a parent
/// handle that did not resolve, so the child started without a capability its
/// parent had named and could not tell that from having asked for nothing, and
/// the parent was told its spawn happened as asked. The owner ruled that
/// strictness wins: a parent naming a handle it does not hold has made exactly
/// the mistake this rule is about, and it now ends like every other
/// (`loader::start::build_child_handles`).
///
/// [`refuse`]: Self::refuse
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleError {
    /// Out of range, or an empty slot.
    BadHandle,
    /// The slot has moved past this handle: it was closed, or its generations
    /// ran out and it retired. Either way the number names nothing, and it will
    /// never name anything again.
    Stale,
    WrongType { held: &'static str, wanted: &'static str },
    /// The handle is fine and does not carry what the call needs.
    Rights { held: Rights, needed: Rights },
    TableFull,
}

impl HandleError {
    /// Answer this failure at the syscall boundary.
    ///
    /// **Call it with nothing held.** For the three kinds that are a bug in the
    /// caller it does not come back: it tears the process down where it stands,
    /// which needs the process's own lock, the table lock and the VFS lock.
    /// Every producer therefore carries the error *out* of whatever guard
    /// resolved the handle and refuses it there.
    pub fn refuse(self) -> u64 {
        self.refuse_as_error().to_u64()
    }

    /// [`refuse`](Self::refuse) for a call site whose answer is a `Result`. The
    /// same rule: nothing held.
    pub fn refuse_as_error(self) -> SyscallError {
        match self {
            Self::Rights { .. } => SyscallError::PermissionDenied,
            Self::TableFull => SyscallError::ResourceExhausted,
            fault => crate::process::handle_fault(fault),
        }
    }
}

/// A refusal on its way out of the guard that produced it.
///
/// A syscall that resolves handles under the process's own lock cannot answer a
/// [`HandleError`] where it finds one — [`HandleError::refuse`] may take the
/// process down, which needs that lock. So the closure hands back one of these
/// and the caller refuses it with nothing held. The `From` impls are what make
/// `?` inside such a closure work for both halves.
pub enum Refusal {
    Handle(HandleError),
    Error(SyscallError),
}

impl Refusal {
    /// See [`HandleError::refuse`]: nothing held.
    pub fn refuse(self) -> u64 {
        match self {
            Self::Handle(e) => e.refuse(),
            Self::Error(e) => e.to_u64(),
        }
    }
}

impl From<HandleError> for Refusal {
    fn from(e: HandleError) -> Self {
        Self::Handle(e)
    }
}

impl From<SyscallError> for Refusal {
    fn from(e: SyscallError) -> Self {
        Self::Error(e)
    }
}

impl core::fmt::Display for HandleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadHandle => write!(f, "no such handle"),
            Self::Stale => write!(f, "a handle closed at an earlier generation"),
            Self::WrongType { held, wanted } => {
                write!(f, "a {held} where the call takes a {wanted}")
            }
            Self::Rights { held, needed } => {
                write!(f, "rights {:#x} where the call needs {:#x}", held.bits(), needed.bits())
            }
            Self::TableFull => write!(f, "no free handle slot"),
        }
    }
}

/// One handle: what it names and what it may do to it.
///
/// **`!Clone`, and it moves by value between every container.** A second entry
/// for one slot is therefore not something a call site can write, and the
/// `handle_count` this drop decrements was incremented by exactly one
/// construction.
pub struct HandleEntry {
    object: KObjectRef,
    rights: Rights,
}

impl HandleEntry {
    /// Count one more handle to `object`.
    ///
    /// The only constructor. Resurrection — a fresh handle to an object whose
    /// count already reached zero — is a kernel bug and never a userland one,
    /// because userland cannot name an object it holds no handle to.
    pub fn new(object: KObjectRef, rights: Rights) -> Self {
        let core = object.core();
        assert!(
            !core.retired(),
            "a handle to a retired {} (koid {})",
            object.kind(),
            core.koid().raw(),
        );
        core.handle_count.fetch_add(1, Ordering::AcqRel);
        Self { object, rights }
    }

    pub fn object(&self) -> &KObjectRef {
        &self.object
    }

    /// A second handle to the same object, carrying no more than this one.
    pub fn duplicate(&self, rights: Rights) -> Result<Self, HandleError> {
        if !self.rights.contains(Rights::DUP) {
            return Err(HandleError::Rights { held: self.rights, needed: Rights::DUP });
        }
        if !rights.subset_of(self.rights) {
            return Err(HandleError::Rights { held: self.rights, needed: rights });
        }
        Ok(Self::new(self.object.clone(), rights))
    }
}

impl Drop for HandleEntry {
    fn drop(&mut self) {
        let core = self.object.core();
        if core.handle_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            let first = !core.retired.swap(true, Ordering::AcqRel);
            assert!(
                first,
                "handle_count resurrected after zero on {} (koid {})",
                self.object.kind(),
                core.koid().raw(),
            );
            // Never inline: see `object::drain_zero_handles`. This is the one
            // statement that makes "a hook cannot run under a lock" structural.
            //
            // Only for a type that *has* a hook. An object with none has
            // nothing to run with nothing held, and queueing it would move its
            // destructor off this stack onto whichever CPU drains next — a
            // killed process's file flush landed on a 16 KiB idle stack that
            // way and wrote through the guard page below it.
            if self.object.defers_release() {
                super::enqueue_zero_handles(self.object.clone());
            }
        }
    }
}

/// One slot of a process's table, and there are exactly two things it can be.
///
/// **A retired slot carries no generation, which is what makes retirement a
/// state rather than a check every insertion path has to remember.** A slot is
/// issued by handing out `RawHandle::new(slot, generation)`, and `Serving` is
/// the only place a generation exists — so a site that forgets to ask whether a
/// slot is still allocatable has no number to hand out and does not compile.
///
/// The shape this replaced parked an exhausted slot at `MAX_GENERATION` and
/// left the free list as the only thing keeping it out of circulation. That
/// held for [`install`](HandleTable::install), which allocates from that list,
/// and not for [`install_at`](HandleTable::install_at), which names its own
/// slot: `dup2` onto an exhausted slot reissued it at `MAX_GENERATION` — for
/// slot 4095 that encoding *is* `HANDLE_INVALID` — and the close after it
/// stepped the counter to `MAX_GENERATION + 1`, whose overflowing bit
/// `RawHandle::new` discards without panicking in any profile, putting the slot
/// back on the free list at generation 0. Every handle the process had ever
/// been issued for that slot named a live object again.
enum Slot {
    /// Issuable, at this generation. A handle naming an earlier one is `Stale`,
    /// which is a different fact from `BadHandle` and is worth telling a crash
    /// report apart by. `entry` says whether anything is in it: an empty
    /// `Serving` slot is on `HandleTable::free` and a full one is not.
    Serving { generation: u32, entry: Option<HandleEntry> },
    /// Spent. Never issued again and never written again — the table is one
    /// slot smaller for the rest of this process's life.
    ///
    /// **Owner ruling of 2026-08-20**, taken over widening the field,
    /// randomising the token, and accepting the wrap under a stated threat
    /// model: a handle that becomes valid again is a use-after-free of
    /// *authority*, and one leaked slot in 4096 buys it away for good. It is
    /// also this tree's standing instinct about a name that is spent — a
    /// deleted syscall's number is retired and never reused
    /// (`toyos_abi::syscall`).
    Retired,
}

/// Handles one process may hold.
///
/// Policy on the primitive, `MAX_*`-named, refused by name and never truncated
/// — four times the 1024 the descriptor table allowed, because a handle now
/// names things a descriptor never did.
pub const MAX_HANDLES: usize = RawHandle::MAX_SLOTS;

pub struct HandleTable {
    slots: Vec<Slot>,
    /// Slots whose entry is gone and whose generation has room left. A retired
    /// slot is in neither this nor the live set — it is simply never offered
    /// again.
    free: Vec<u16>,
}

impl HandleTable {
    pub const fn new() -> Self {
        Self { slots: Vec::new(), free: Vec::new() }
    }

    /// Whether `n` more `install`s can all succeed.
    ///
    /// Spawn's endowment vector asks before it takes anything out of the
    /// parent's table, because a move that fails halfway has already emptied a
    /// slot the caller is about to be told nothing happened to.
    ///
    /// A retired slot is counted in neither term — it is in `slots` and never
    /// in `free` — so the room it used to be is gone from this answer, which is
    /// what "the table is one slot smaller" means at the only place anything
    /// asks how big it is.
    pub fn has_room(&self, n: usize) -> bool {
        self.free.len() + (MAX_HANDLES - self.slots.len()) >= n
    }

    pub fn install(&mut self, entry: HandleEntry) -> Result<RawHandle, TableFull> {
        if let Some(slot) = self.free.pop() {
            // A retired slot is never pushed onto the free list and an occupied
            // one is taken off it, so a vacancy is the one shape this holds.
            let Slot::Serving { generation, entry: vacancy } = &mut self.slots[slot as usize]
            else {
                unreachable!("the free list offered a retired slot");
            };
            debug_assert!(vacancy.is_none(), "a live slot was on the free list");
            *vacancy = Some(entry);
            return Ok(RawHandle::new(slot, *generation));
        }
        if self.slots.len() >= MAX_HANDLES {
            return Err(TableFull);
        }
        let slot = self.slots.len() as u16;
        self.slots.push(Slot::Serving { generation: 0, entry: Some(entry) });
        Ok(RawHandle::new(slot, 0))
    }

    /// Install at a caller-chosen slot, replacing whatever is there.
    ///
    /// Spawn's stdio seeding and `SYS_HANDLE_DUP_AT`. The displaced entry is
    /// returned rather than dropped here, so its `handle_count` decrement
    /// happens where the caller decides — outside whatever guard it is holding.
    ///
    /// **Replacing a live slot does not advance its generation, and that is the
    /// point of `dup2` rather than an oversight.** A handle the caller was
    /// already holding for the displaced object therefore names the
    /// replacement. The alternative was considered and is wrong: the number is
    /// what a POSIX caller keeps using — `printf` writes to the literal `1`,
    /// and `userland/libc`'s `dup2` hands back `f.0 as i32` — so bumping here
    /// would make every write after `dup2(pipe, 1)` `Stale`, which ends the
    /// process. [`remove`](Self::remove) bumps because *there* the slot is
    /// being given up; here it is being pointed somewhere else by its owner,
    /// and no authority crosses a process boundary either way.
    ///
    /// The consequence to know: a `RawHandle` names one object for as long as
    /// its holder does not itself redirect the slot. Anything using a handle
    /// value as a *name* — `toyos::surface::ClientId` — is relying on that
    /// narrower statement.
    ///
    /// **A retired slot is refused here, and this is the path that needed
    /// saying so.** Naming the slot is the whole of what this call does, so the
    /// free list — which is what keeps a spent slot away from `install` — stands
    /// in front of nothing here. The word is the cap's own: a slot the table no
    /// longer has is the same answer as a slot past the end.
    #[must_use = "the displaced entry must be dropped by the caller"]
    pub fn install_at(
        &mut self,
        slot: u16,
        entry: HandleEntry,
    ) -> Result<(RawHandle, Option<HandleEntry>), TableFull> {
        let slot_index = slot as usize;
        // `MAX_HANDLES` **is** the slot range, so a slot past the end is the
        // table's cap rather than a malformed argument, and the caller sees the
        // same `ResourceExhausted` the allocating path gives it.
        if slot_index >= MAX_HANDLES {
            return Err(TableFull);
        }
        while self.slots.len() <= slot_index {
            self.free.push(self.slots.len() as u16);
            self.slots.push(Slot::Serving { generation: 0, entry: None });
        }
        self.free.retain(|&s| s != slot);
        let Slot::Serving { generation, entry: at } = &mut self.slots[slot_index] else {
            return Err(TableFull);
        };
        let displaced = at.replace(entry);
        Ok((RawHandle::new(slot, *generation), displaced))
    }

    /// The entry a handle names, or why it names none.
    ///
    /// A retired slot answers `Stale` rather than `BadHandle`: the slot is in
    /// range and the handle is one from before it was given up, which is the
    /// same fact about the same slot that a moved generation states.
    fn entry_of(&self, h: RawHandle) -> Result<&HandleEntry, HandleError> {
        match self.slots.get(h.slot() as usize).ok_or(HandleError::BadHandle)? {
            Slot::Retired => Err(HandleError::Stale),
            Slot::Serving { generation, .. } if *generation != h.generation() => {
                Err(HandleError::Stale)
            }
            Slot::Serving { entry, .. } => entry.as_ref().ok_or(HandleError::BadHandle),
        }
    }

    /// The typed accessor.
    ///
    /// Returns an owned `Arc`, so the object outlives the guard and the guard
    /// outlives no reference into the table.
    pub fn get<T: KObjectVariant>(
        &self,
        h: RawHandle,
        need: Rights,
    ) -> Result<Arc<T>, HandleError> {
        let entry = self.entry_of(h)?;
        if !entry.rights.contains(need) {
            return Err(HandleError::Rights { held: entry.rights, needed: need });
        }
        T::from_ref(&entry.object)
            .cloned()
            .ok_or(HandleError::WrongType { held: entry.object.kind(), wanted: T::NAME })
    }

    /// The borrowing accessor, for a call that runs to completion under the
    /// guard it was resolved through.
    ///
    /// `read` and `write` are that call, and they are the hottest pair in the
    /// kernel: cloning the `Arc` out would put one atomic read-modify-write on
    /// each of them, which is the operation TCG runs a translation block
    /// exclusively for
    /// (`issues/hardware/one-rmw-per-log-line-cost-350ms.md`). Nothing escapes —
    /// the lifetime is `&self`'s, so the compiler refuses a borrow that
    /// outlives the table.
    pub fn get_ref(&self, h: RawHandle, need: Rights) -> Result<&KObjectRef, HandleError> {
        let entry = self.entry_of(h)?;
        if !entry.rights.contains(need) {
            return Err(HandleError::Rights { held: entry.rights, needed: need });
        }
        Ok(&entry.object)
    }

    pub fn duplicate(
        &mut self,
        h: RawHandle,
        rights: Rights,
    ) -> Result<RawHandle, HandleError> {
        let entry = self.entry_of(h)?.duplicate(rights)?;
        self.install(entry).map_err(|TableFull| HandleError::TableFull)
    }

    /// What a handle carries, for a caller about to duplicate it unchanged.
    pub fn rights_of(&self, h: RawHandle) -> Result<Rights, HandleError> {
        Ok(self.entry_of(h)?.rights)
    }

    /// A duplicate for *another* table — a child's, built at spawn.
    pub fn duplicate_entry(
        &self,
        h: RawHandle,
        rights: Rights,
    ) -> Result<HandleEntry, HandleError> {
        self.entry_of(h)?.duplicate(rights)
    }

    /// Take a handle out of the table.
    ///
    /// The entry is returned rather than dropped, so the `handle_count`
    /// decrement — and the deferred hook it may enqueue — happen at a point the
    /// caller chose. The slot's generation is bumped here, which is what makes
    /// a handle to it `Stale` rather than a name for whatever lands there next.
    #[must_use = "the removed entry must be dropped by the caller"]
    pub fn remove(&mut self, h: RawHandle) -> Result<HandleEntry, HandleError> {
        let entry = self.take_for_transfer(h)?;
        self.retire(h);
        Ok(entry)
    }

    /// Take an entry out, leaving its slot claimed and at its own generation.
    ///
    /// [`remove`](Self::remove) is this plus [`retire`](Self::retire), and the
    /// split exists because retiring is what makes putting an entry back
    /// unrepresentable: a bumped generation means the handle number the caller
    /// still holds names nothing. See [`transfer`](Self::transfer).
    #[must_use = "the entry must be given back or its slot retired"]
    fn take_for_transfer(&mut self, h: RawHandle) -> Result<HandleEntry, HandleError> {
        match self.slots.get_mut(h.slot() as usize).ok_or(HandleError::BadHandle)? {
            Slot::Retired => Err(HandleError::Stale),
            Slot::Serving { generation, .. } if *generation != h.generation() => {
                Err(HandleError::Stale)
            }
            Slot::Serving { entry, .. } => entry.take().ok_or(HandleError::BadHandle),
        }
    }

    /// The handle is gone for good, and the slot either moves on or stops.
    ///
    /// **A slot at its last generation retires, never wraps** — the owner's
    /// ruling of 2026-08-20, and the reason [`Slot::Retired`] exists rather than
    /// a counter parked at its maximum. One leaked slot of 4096 against a handle
    /// that silently names a different object is not a trade; it is also what
    /// keeps `HANDLE_INVALID` unreachable, since that encoding is slot 4095 at
    /// `MAX_GENERATION` and no slot is ever issued at that generation now.
    fn retire(&mut self, h: RawHandle) {
        let index = h.slot() as usize;
        let spent = match &mut self.slots[index] {
            Slot::Serving { generation, entry } => {
                debug_assert!(entry.is_none(), "a slot still holding an entry was retired");
                if *generation == RawHandle::MAX_GENERATION - 1 {
                    true
                } else {
                    *generation += 1;
                    false
                }
            }
            Slot::Retired => unreachable!("a retired slot answered a handle"),
        };
        if spent {
            self.slots[index] = Slot::Retired;
        } else {
            self.free.push(h.slot());
        }
    }

    /// Put an entry back at the number it was taken from.
    fn give_back(&mut self, h: RawHandle, entry: HandleEntry) {
        let Slot::Serving { generation, entry: vacancy } = &mut self.slots[h.slot() as usize]
        else {
            unreachable!("a slot taken for transfer was retired under the same lock");
        };
        debug_assert!(
            vacancy.is_none() && *generation == h.generation(),
            "a slot taken for transfer was written under the same lock",
        );
        *vacancy = Some(entry);
    }

    /// Move `handles` out of this table into `sink`, and put every one of them
    /// back at its own number if `sink` refuses.
    ///
    /// **A refusal that keeps the handles is the reason this exists.** The two
    /// things a peer's queue can say — the reading end has gone, and the queue
    /// is full — are ones a caller reads as backpressure, and `ResourceExhausted`
    /// is exactly what a slow or hostile peer produces. Taking the entries out
    /// and dropping them on that answer destroys capabilities the caller was
    /// told nothing happened to: its next `close` of one is `Stale`, which ends
    /// it. `/bin/init` was that caller — a client that hung up after its launch
    /// frame made init's answering `Process` handle vanish and init's own close
    /// of it fatal.
    ///
    /// `sink` therefore hands the batch back with its refusal, which is the
    /// whole of the discipline: the type says a refused transfer still owns
    /// what it was given. Every handle must have been verified under this same
    /// hold — a number that does not resolve here is a kernel bug.
    pub fn transfer<E>(
        &mut self,
        handles: &[RawHandle],
        sink: impl FnOnce(Vec<HandleEntry>) -> Result<(), (Vec<HandleEntry>, E)>,
    ) -> Result<(), E> {
        let mut batch = Vec::with_capacity(handles.len());
        for h in handles {
            batch.push(
                self.take_for_transfer(*h).expect("a handle verified under this same hold"),
            );
        }
        match sink(batch) {
            Ok(()) => {
                for h in handles {
                    self.retire(*h);
                }
                Ok(())
            }
            Err((batch, e)) => {
                for (h, entry) in handles.iter().zip(batch) {
                    self.give_back(*h, entry);
                }
                Err(e)
            }
        }
    }

    /// Empty the table. Process exit and kill both come through here, on the
    /// killer's CPU, and the caller drops what it gets with nothing held.
    #[must_use = "the drained entries must be dropped by the caller"]
    pub fn drain(&mut self) -> Vec<HandleEntry> {
        let mut out = Vec::new();
        for slot in &mut self.slots {
            match slot {
                Slot::Serving { entry, .. } => out.extend(entry.take()),
                // Nothing to give back: a slot retires on the close that has
                // already taken its entry out.
                Slot::Retired => {}
            }
        }
        self.free.clear();
        out
    }

    pub fn iter(&self) -> impl Iterator<Item = (RawHandle, &HandleEntry)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| match slot {
            Slot::Serving { generation, entry } => {
                entry.as_ref().map(|e| (RawHandle::new(i as u16, *generation), e))
            }
            Slot::Retired => None,
        })
    }

    /// Test actuator: put a free slot at the last generation it can be issued
    /// at, and answer the handle its next install will carry.
    ///
    /// **The near-exhaustion instrument, and there is no other way to reach
    /// this state.** A slot's counter is twenty bits, so running one out for
    /// real is 1,048,575 close/reopen round trips against a table that answers
    /// each in a syscall — the property under test would be gated by a test
    /// nobody could afford to run, which is how it went ungated to begin with.
    /// Nothing is faked: the generation is the shipped field, the install that
    /// follows is the shipped path, and [`retire`](Self::retire) makes the
    /// shipped decision about what it finds.
    ///
    /// A slot that still holds an entry is refused, so this can never invalidate
    /// a handle its process is holding — the caller stages a slot it has just
    /// closed, and what comes back is the number the next `install` of it
    /// answers.
    #[cfg(feature = "test-actuators")]
    pub fn stage_last_generation(&mut self, slot: u16) -> Option<RawHandle> {
        match self.slots.get_mut(slot as usize)? {
            Slot::Serving { generation, entry: None } => {
                *generation = RawHandle::MAX_GENERATION - 1;
                Some(RawHandle::new(slot, *generation))
            }
            Slot::Serving { entry: Some(_), .. } | Slot::Retired => None,
        }
    }
}
