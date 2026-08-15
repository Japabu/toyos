use core::hash::Hash;
use core::ops::Add;
use hashbrown::HashMap;

/// A HashMap with auto-incrementing keys. IDs are never reused.
pub struct IdMap<K, V> {
    map: HashMap<K, V>,
    next: K,
}

pub trait IdKey: Copy + Eq + Hash + Ord + Add<Output = Self> {
    const ZERO: Self;
    const ONE: Self;
}

// The four keys the tree has: two process-table ids and the two driver-side
// ids below them. `u32`, `u64` and `usize` had impls of their own and no
// `IdMap` was ever keyed by one — a bare integer is exactly the key this type
// exists to stop a caller from inventing.
impl IdKey for toyos_abi::Pid {
    const ZERO: Self = Self(0);
    const ONE: Self = Self(1);
}
impl IdKey for toyos_abi::Tid {
    const ZERO: Self = Self(0);
    const ONE: Self = Self(1);
}

impl<K: IdKey, V> IdMap<K, V> {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            next: K::ZERO,
        }
    }

    /// Insert with auto-assigned ID. Returns the new ID.
    pub fn insert(&mut self, value: V) -> K {
        let id = self.next;
        self.next = self.next + K::ONE;
        self.map.insert(id, value);
        id
    }

    /// Insert with auto-assigned ID, providing the ID to a closure that constructs the value.
    /// Eliminates the need for temporary invalid state (e.g. `pid: 0`).
    pub fn insert_with(&mut self, f: impl FnOnce(K) -> V) -> K {
        let id = self.next;
        self.next = self.next + K::ONE;
        let value = f(id);
        self.map.insert(id, value);
        id
    }

    pub fn get(&self, id: K) -> Option<&V> {
        self.map.get(&id)
    }

    pub fn get_mut(&mut self, id: K) -> Option<&mut V> {
        self.map.get_mut(&id)
    }

    pub fn remove(&mut self, id: K) -> Option<V> {
        self.map.remove(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        self.map.iter().map(|(&k, v)| (k, v))
    }

}
