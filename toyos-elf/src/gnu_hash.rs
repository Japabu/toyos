//! `.gnu.hash`, the one table in an ELF whose extent nothing declares.
//!
//! Its length is in no `DT_*` tag and in no header: the bucket and bloom counts
//! come out of its own first words, and the chain array is terminated by a bit
//! in the data rather than by a count. So every index into it is a file-chosen
//! number checked against the bytes the caller supplied, and a walk that runs
//! off the end answers `None` — a symbol count derived from a walk that ran off
//! the end is not a count.

use crate::read;
use crate::sym::SymTab;

/// A `.gnu.hash` table whose header is readable and internally usable.
///
/// [`GnuHash::parse`] refuses a zero bucket or bloom count — both are divisors
/// — and a `bloom_shift` of 64 or more, which is a shift no 64-bit word has.
/// glibc's own lookup shifts a 64-bit hash by that field on x86-64, so
/// anything below 64 behaves exactly as it does there and anything above is
/// undefined in C and unrepresentable here.
#[derive(Clone, Copy, Debug)]
pub struct GnuHash<'a> {
    data: &'a [u8],
    nbuckets: u32,
    symoffset: u32,
    bloom_size: u32,
    bloom_shift: u32,
}

/// The DJB variant `.gnu.hash` is built on.
pub fn hash(name: &str) -> u32 {
    let mut h: u32 = 5381;
    for &b in name.as_bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u32);
    }
    h
}

impl<'a> GnuHash<'a> {
    pub fn parse(data: &'a [u8]) -> Option<GnuHash<'a>> {
        let nbuckets = read::u32_at(data, 0)?;
        let symoffset = read::u32_at(data, 4)?;
        let bloom_size = read::u32_at(data, 8)?;
        let bloom_shift = read::u32_at(data, 12)?;
        if nbuckets == 0 || bloom_size == 0 || bloom_shift >= 64 {
            return None;
        }
        Some(GnuHash {
            data,
            nbuckets,
            symoffset,
            bloom_size,
            bloom_shift,
        })
    }

    /// Both counts are `u32` a file chose, so on a target where `usize` is not
    /// wider the products are not products.
    fn buckets_off(&self) -> Option<usize> {
        (self.bloom_size as usize).checked_mul(8)?.checked_add(16)
    }

    fn chains_off(&self) -> Option<usize> {
        (self.nbuckets as usize)
            .checked_mul(4)?
            .checked_add(self.buckets_off()?)
    }

    /// How many entries `.dynsym` holds, derived from the highest bucket and
    /// the chain that starts there.
    ///
    /// `None` for a table that cannot be walked to a terminating chain entry.
    /// The walk is bounded by the bytes it was given, so a chain with no
    /// terminator ends the walk rather than running forever.
    pub fn sym_count(&self) -> Option<usize> {
        let buckets = self.buckets_off()?;
        let mut max_sym = 0u32;
        for i in 0..self.nbuckets as usize {
            let val = read::u32_at(self.data, buckets + i * 4)?;
            max_sym = max_sym.max(val);
        }
        if max_sym < self.symoffset {
            return Some(self.symoffset as usize);
        }
        let chains = self.chains_off()?;
        let mut idx = (max_sym - self.symoffset) as usize;
        loop {
            let entry = read::u32_at(self.data, chains + idx * 4)?;
            if entry & 1 != 0 {
                return Some(self.symoffset as usize + idx + 1);
            }
            idx += 1;
        }
    }

    /// The index of `name` in `symtab`, or `None`.
    ///
    /// The chain is the file's; the answer is the symbol table's. A chain entry
    /// naming an index `symtab` does not hold is skipped rather than trusted —
    /// nothing else bounds the indices a chain may contain.
    pub fn lookup(&self, name: &str, symtab: &SymTab) -> Option<usize> {
        let h = hash(name);

        let bloom_idx = ((h as u64 / 64) % self.bloom_size as u64) as usize;
        let bloom_word = read::u64_at(self.data, 16 + bloom_idx * 8)?;
        let mask = (1u64 << (h % 64)) | (1u64 << ((h as u64 >> self.bloom_shift) % 64));
        if bloom_word & mask != mask {
            return None;
        }

        let bucket = (h % self.nbuckets) as usize;
        let first = read::u32_at(self.data, self.buckets_off()? + bucket * 4)?;
        if first == 0 {
            return None;
        }

        let chains = self.chains_off()?;
        let mut i = first;
        loop {
            // Both `i` and `symoffset` come out of the file, so the chain index
            // can be negative as easily as it can run off the end.
            let chain_idx = i.checked_sub(self.symoffset)? as usize;
            let entry = read::u32_at(self.data, chains + chain_idx * 4)?;
            if (entry | 1) == (h | 1) {
                let idx = i as usize;
                if symtab.get(idx).is_some_and(|s| s.is_defined()) && symtab.name(idx) == name {
                    return Some(idx);
                }
            }
            if entry & 1 != 0 {
                return None;
            }
            i = i.checked_add(1)?;
        }
    }
}
