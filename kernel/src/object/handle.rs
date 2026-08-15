//! One process's handle table.
//!
//! Lives inside `ProcessData`, behind the lock that is already there — no new
//! lock and no new ordering edge. Every accessor hands back an **owned** value:
//! no borrow into the table can outlive the guard, which is what stops a
//! syscall holding a reference into a table another thread of the same process
//! is editing.

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
/// [`refuse`]: Self::refuse
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleError {
    /// Out of range, or an empty slot.
    BadHandle,
    /// The slot is live but at a later generation: this handle was closed.
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

struct Slot {
    /// The generation this slot is *at*. A handle naming an earlier one is
    /// `Stale`, which is a different fact from `BadHandle` and is worth telling
    /// a crash report apart by.
    generation: u32,
    entry: Option<HandleEntry>,
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
    pub fn has_room(&self, n: usize) -> bool {
        self.free.len() + (MAX_HANDLES - self.slots.len()) >= n
    }

    pub fn install(&mut self, entry: HandleEntry) -> Result<RawHandle, TableFull> {
        if let Some(slot) = self.free.pop() {
            let s = &mut self.slots[slot as usize];
            debug_assert!(s.entry.is_none(), "a live slot was on the free list");
            s.entry = Some(entry);
            return Ok(RawHandle::new(slot, s.generation));
        }
        if self.slots.len() >= MAX_HANDLES {
            return Err(TableFull);
        }
        let slot = self.slots.len() as u16;
        self.slots.push(Slot { generation: 0, entry: Some(entry) });
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
            self.slots.push(Slot { generation: 0, entry: None });
        }
        self.free.retain(|&s| s != slot);
        let s = &mut self.slots[slot_index];
        let displaced = s.entry.take();
        s.entry = Some(entry);
        Ok((RawHandle::new(slot, s.generation), displaced))
    }

    fn slot_of(&self, h: RawHandle) -> Result<&Slot, HandleError> {
        let slot = self.slots.get(h.slot() as usize).ok_or(HandleError::BadHandle)?;
        if slot.generation != h.generation() {
            return Err(HandleError::Stale);
        }
        Ok(slot)
    }

    fn entry_of(&self, h: RawHandle) -> Result<&HandleEntry, HandleError> {
        self.slot_of(h)?.entry.as_ref().ok_or(HandleError::BadHandle)
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
    /// (`specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md`). Nothing escapes —
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
        let slot = self.slots.get_mut(h.slot() as usize).ok_or(HandleError::BadHandle)?;
        if slot.generation != h.generation() {
            return Err(HandleError::Stale);
        }
        slot.entry.take().ok_or(HandleError::BadHandle)
    }

    /// The handle is gone for good.
    ///
    /// **A slot at the last generation is retired, never wrapped.** One leaked
    /// slot of 4096 against a handle that silently names a different object is
    /// not a trade; it is also what keeps `HANDLE_INVALID` unreachable, since
    /// that encoding is slot 4095 at this generation.
    fn retire(&mut self, h: RawHandle) {
        let slot = &mut self.slots[h.slot() as usize];
        if slot.generation == RawHandle::MAX_GENERATION - 1 {
            slot.generation = RawHandle::MAX_GENERATION;
        } else {
            slot.generation += 1;
            self.free.push(h.slot());
        }
    }

    /// Put an entry back at the number it was taken from.
    fn give_back(&mut self, h: RawHandle, entry: HandleEntry) {
        let slot = &mut self.slots[h.slot() as usize];
        debug_assert!(
            slot.entry.is_none() && slot.generation == h.generation(),
            "a slot taken for transfer was written under the same lock",
        );
        slot.entry = Some(entry);
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
            if let Some(entry) = slot.entry.take() {
                out.push(entry);
            }
        }
        self.free.clear();
        out
    }

    pub fn iter(&self) -> impl Iterator<Item = (RawHandle, &HandleEntry)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| {
            slot.entry.as_ref().map(|e| (RawHandle::new(i as u16, slot.generation), e))
        })
    }
}
