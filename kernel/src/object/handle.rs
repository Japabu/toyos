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

/// Why a handle did not resolve.
///
/// Three of these are userland bugs and one is not: a process may legitimately
/// hold an attenuated handle and probe what it can do with it, so `Rights` is
/// an error return for ever. The other three become a kill in chunk 7 —
/// naming a handle you do not hold is a bug in the namer, and fail-fast is for
/// bugs.
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
    pub fn to_syscall_error(self) -> SyscallError {
        match self {
            Self::BadHandle | Self::Stale => SyscallError::NotFound,
            Self::WrongType { .. } | Self::Rights { .. } => SyscallError::PermissionDenied,
            Self::TableFull => SyscallError::ResourceExhausted,
        }
    }

    pub fn to_u64(self) -> u64 {
        self.to_syscall_error().to_u64()
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

    pub fn rights(&self) -> Rights {
        self.rights
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
            super::enqueue_zero_handles(self.object.clone());
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

    pub fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.entry.is_some()).count()
    }

    pub fn install(&mut self, entry: HandleEntry) -> Result<RawHandle, HandleError> {
        if let Some(slot) = self.free.pop() {
            let s = &mut self.slots[slot as usize];
            debug_assert!(s.entry.is_none(), "a live slot was on the free list");
            s.entry = Some(entry);
            return Ok(RawHandle::new(slot, s.generation));
        }
        if self.slots.len() >= MAX_HANDLES {
            return Err(HandleError::TableFull);
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
    #[must_use = "the displaced entry must be dropped by the caller"]
    pub fn install_at(
        &mut self,
        slot: u16,
        entry: HandleEntry,
    ) -> Result<(RawHandle, Option<HandleEntry>), HandleError> {
        let slot_index = slot as usize;
        if slot_index >= MAX_HANDLES {
            return Err(HandleError::BadHandle);
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

    /// The untyped one, for close, dup, transfer and stat. Clones the object
    /// reference out; still no borrow escapes.
    pub fn get_any(
        &self,
        h: RawHandle,
        need: Rights,
    ) -> Result<(KObjectRef, Rights), HandleError> {
        let entry = self.entry_of(h)?;
        if !entry.rights.contains(need) {
            return Err(HandleError::Rights { held: entry.rights, needed: need });
        }
        Ok((entry.object.clone(), entry.rights))
    }

    pub fn duplicate(
        &mut self,
        h: RawHandle,
        rights: Rights,
    ) -> Result<RawHandle, HandleError> {
        let entry = self.entry_of(h)?.duplicate(rights)?;
        self.install(entry)
    }

    /// Take a handle out of the table.
    ///
    /// The entry is returned rather than dropped, so the `handle_count`
    /// decrement — and the deferred hook it may enqueue — happen at a point the
    /// caller chose. The slot's generation is bumped here, which is what makes
    /// a handle to it `Stale` rather than a name for whatever lands there next.
    #[must_use = "the removed entry must be dropped by the caller"]
    pub fn remove(&mut self, h: RawHandle) -> Result<HandleEntry, HandleError> {
        let slot_index = h.slot() as usize;
        let slot = self.slots.get_mut(slot_index).ok_or(HandleError::BadHandle)?;
        if slot.generation != h.generation() {
            return Err(HandleError::Stale);
        }
        let entry = slot.entry.take().ok_or(HandleError::BadHandle)?;
        // **A slot at the last generation is retired, never wrapped.** One
        // leaked slot of 4096 against a handle that silently names a different
        // object is not a trade; it is also what keeps `HANDLE_INVALID`
        // unreachable, since that encoding is slot 4095 at this generation.
        if slot.generation == RawHandle::MAX_GENERATION - 1 {
            slot.generation = RawHandle::MAX_GENERATION;
        } else {
            slot.generation += 1;
            self.free.push(h.slot());
        }
        Ok(entry)
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
