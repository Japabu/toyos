# Independent audit: `toyos-fat32/`, `toyos-gpt/`, `kernel/src/gpt.rs`

Read-only. Nothing in the repository was changed. Every experiment ran against a
copy of each crate in a scratch directory outside the tree; the workspace build
and the QEMU suite were not touched.

Verified against `4ef37fb` (2026-08-01); the HEAD-does-not-compile note below was
re-confirmed at that commit. Crates read at `124c2ac`
(`toyos-fat32`), `611c6d0` (`toyos-gpt`) and `247c403` (`kernel/src/gpt.rs`) —
all three unchanged in the working tree at the time of the audit.

Why this one is stricter than a normal audit: `toyos-fat32` writes to the ESP of
the USB stick the owner boots the T14 from. `specs/assessments/metal-track-history.md`
records ~70 confirmed defects in code whose own suites were green, and both of
these crates were audited by their own author.

**Every number below came from a command that was run.** Where something could
not be checked without the QEMU suite, it says so.

---

## Summary

Ten of the authors' claims hold, four do not, one could not be checked. The four
that do not hold are all in `toyos-fat32`, all on the write path, and three of
them are demonstrated with a reproducer rather than argued:

- **A crafted directory entry makes `Fat32::write` issue a device write 256 GiB
  outside the volume, and return `Ok(())`.** Demonstrated.
- **`flush_meta`'s stale-handle guard passes in the state every file starts in**,
  so a stale handle rewrites a different file's directory entry — and
  `fsck_msdos` calls the result clean. Demonstrated.
- **`set_len` on a cyclic chain frees the file's own live clusters and returns
  `Ok(())`.** Demonstrated. This is the case the crate's documented residual says
  is detected.
- **`remove` on a cyclic chain frees clusters and then errors, leaving a live
  directory entry naming free ones.** Demonstrated.

`toyos-gpt` came out well. The polynomial is right, the parser is genuinely
total on the paths tested, and the test suite has teeth. Two real gaps: a
partition may legally claim the blocks the backup GPT sits on, and a duplicate
unique GUID inside one table is resolved first-wins — which is the exact thing
the module's own prose says must never happen.

---

## Part 1 — verdicts on the authors' claims

### `toyos-fat32`

| # | Claim | Verdict |
|---|---|---|
| 1 | 69 tests, 0 failures | **Holds** |
| 2 | 2861 lines of driver, 1873 of tests | **Holds** |
| 3 | Zero `unwrap`/`expect`/`panic!`/`assert!` in non-test source | **Holds** |
| 4 | …and no panicking *equivalents* | **Holds**, with two nits |
| 5 | 16 breakages, 14 red, 2 slipped | **Could not check** the list |
| 6 | The two that slipped are now covered by raw-byte tests | **Holds** — both mutations verified red |
| 7 | No code that could write a BPB | **Holds** for the crate's own writes; the escape hatch is `Fat32::device()` |
| 8 | A stale `File` cannot write to the wrong place | **Does not hold** |
| 9 | A cycle is detected wherever it would do damage | **Does not hold** |
| 10 | "never asks for bytes it has not already bounded against the volume" | **Does not hold** |

**(1) 69 tests, 0 failures — holds.** `cargo test` inside `toyos-fat32/`:
17 unit + 10 `host_read` + 18 `host_write` + 24 `hostile` = 69, all green.
`toyos-gpt`: 5 unit + 30 `parse` = 35, all green.

**(2) Line counts — holds exactly.** `wc -l src/*.rs` = 2861;
`wc -l tests/*.rs tests/common/*.rs` = 1873.

**(3) No panicking constructs — holds.** `grep -n "unwrap\|expect(\|panic!\|assert\|unreachable!\|todo!"` over
`src/` returns 10 hits outside `#[cfg(test)]` and every one is `unwrap_or` /
`unwrap_or_else` on an `Option`, which is total. All the `assert!` hits are
inside `mod tests`.

**(4) Equivalents — holds, with two nits.**

- *Slice indexing.* Every direct index in non-test source is on a fixed-size
  array (`RawEntry([u8; 32])`, `ShortName = [u8; 11]`, `[u16; 260]`) at a
  constant or statically-bounded offset. The three argument-bounded ones —
  `name.rs:188`/`:205` (`short[n]`, guarded by `if n == 8` / `if n == 11`),
  `name.rs:261`/`:270` (`out[keep+1]`, `out[keep+5]`, guarded by `.min(6)` /
  `.min(2)`), `name.rs:324` (`units[len]`, guarded by `len == MAX_LFN_CHARS`) —
  are correct. Nothing is indexed by a disk-derived value.
- *Integer division.* `bytes_per_sector / 4`, `bytes_per_cluster() / ENTRY_SIZE`,
  `lba_bytes / entry_bytes` — every divisor is validated non-zero at parse.
- *`as` truncation.* 123 casts across `src/`. The narrowing ones
  (`first_data_sector as u32`, `cluster as u32`, `end as u32`) are each preceded
  by a comparison that bounds the value; `boot.rs:128-131` states the u64
  argument explicitly and it is correct.
- *Arithmetic overflow.* `write()` uses `checked_add`; `alloc_cluster` /
  `free_chain` use `saturating_add`/`saturating_sub`; `days_from_civil` is
  saturating and `every_bit_pattern_decodes` exercises all 65,536 date words.
- **Nit 1.** `fat.rs:250` takes `self.scratch.get_mut(..bps).ok_or(...)` and then
  `fat.rs:253` indexes the same range directly as `&self.scratch[..bps]`. Both
  are safe (`scratch.len() == bytes_per_sector` by construction at `fs.rs:139`);
  they should not disagree about which style the crate uses.
- **Nit 2, in `toyos-gpt`.** `Partition::lba_count` is `last_lba - first_lba + 1`
  with no check, on a `pub struct` with `pub` fields. Reached only after `locate`
  has validated the range, but a caller can build the struct. Demonstrated: a
  hand-built backwards `Partition` panics with *attempt to subtract with
  overflow* in debug.

**(5) 16 breakages, 14 red — could not check.** The list is not in the tree; the
commit message names only the two that slipped. Not counted against the author —
it is a claim about a scratch copy that no longer exists.

**(6) The two new raw-byte tests have teeth — holds. Verified in both
directions.** Each mutation was applied to a scratch copy and the whole
`host_write` suite run:

| mutation | result |
|---|---|
| `Geometry::fat_mirrors` returns `0..1` (write FAT 0 only) | **1 of 18 red**: `every_fat_copy_stays_in_step`, at `common/mod.rs:429`, "FAT 1 differs from FAT 0 at byte 12" |
| `insert_entry` drops the candidate loop (no 8.3 uniquifying) | **1 of 18 red**: `colliding_short_names_stay_unique`, at `host_write.rs:200`, `left: 4, right: 40` |

Both are exactly the right test and nothing else. In the second case the host
read-back assertions at `host_write.rs:186` passed *before* the raw-byte
assertion at `:200` fired — direct confirmation of the author's generalisation
that a host validator's silence is evidence about the validator.

**(7) No code that could write a BPB — holds for the crate, with a caveat.**
There are five `write_at` call sites (`boot.rs:296`, `dir.rs:375`, `fat.rs:42`,
`fat.rs:253`, `fs.rs:464`). `boot.rs:296` is FSInfo, whose sector is required
non-zero and below `reserved_sectors` at `boot.rs:160`; the other four compute
offsets from `cluster_offset` or `fat_entry_offset`. Verified empirically: a
workload of 40 file creations with long names, 20 deletions, three nested
`create_dir`s and a `sync` changed **exactly two bytes** in the first eight
sectors — offsets 1000 and 1004, i.e. FSInfo's `free_count` and `next_free`.
Boot sector, backup boot sector and the volume-label entry were byte-identical.

Two caveats:

- `Fat32::device(&mut self) -> &mut D` (`fs.rs:148`) is `pub` and hands the raw
  volume to the caller. The crate contains no code that writes a BPB; it does
  contain the means for its caller to. The tests use it, which is fine — but the
  claim should be stated as "no path *this crate takes*".
- The **backup FSInfo at sector 7 is never updated**. After the workload above it
  still holds `newfs_msdos`'s original counts. `fsck_msdos` did not care.

**(8) A stale `File` cannot write to the wrong place — DOES NOT HOLD.** Finding
F2. Demonstrated below.

`lib.rs:56-59` states it more broadly still: *"every operation that uses one
re-reads the directory entry and checks it still names the same chain."* Only
`flush_meta` does. `read`, `write` and `set_len` never touch the entry.
`fs.rs:60-63`'s version of the same paragraph is accurate about *which* call
checks; `lib.rs` is not.

**(9) A cycle is detected wherever it would do damage — DOES NOT HOLD.** Finding
F3. `fat.rs:86-90` names `free_chain`, `chain_len` and `chain_last` as the
detecting paths. `chain_len` and `chain_last` do detect (both are bounded by
`limit` and return `CorruptChain`). `free_chain` detects only when the walk
re-enters a cluster it has already zeroed — which is not what happens when
`truncate_chain` reaches it.

**(10) "never asks for bytes it has not already bounded against the volume"
(`device.rs:23-26`) — DOES NOT HOLD.** Finding F1. This is the sentence a kernel
adapter author will read while deciding whether to bound `write_at` themselves.

### `toyos-gpt` and `kernel/src/gpt.rs`

| # | Claim | Verdict |
|---|---|---|
| 11 | CRC-32 (zlib/Ethernet), not CRC-32C | **Holds** — verified against an external oracle |
| 12 | No panicking path on a hostile table | **Holds** on everything probed |
| 13 | No allocation sized by the disk | **Holds** — `no_std`, no `alloc`, array is streamed |
| 14 | The single-byte sweep is 11,776 parses | **Does not hold** — it is 34,816 |
| 15 | The GPT reader does not swallow a failed read | **Holds** |

**(11) The polynomial — holds, verified independently.** `crc32.rs:17` uses
`0xEDB8_8320`, the reflected zlib/Ethernet polynomial; `bcachefs/src/crc32c.rs`
would be `0x82F63B78`. Rather than trust the crate's own check value, four
vectors were computed with Python's `zlib.crc32` (itself cross-checked against a
from-scratch bitwise implementation, agreeing on 1000 random bytes) and run
through `toyos_gpt::crc32`:

| input | zlib | crate |
|---|---|---|
| `"123456789"` | `0xCBF43926` | match |
| `"The quick brown fox jumps over the lazy dog"` | `0x414FA339` | match |
| `"EFI PART"` | `0x94CC656D` | match |
| 128 zero bytes | `0xC2A8FA9D` | match |
| 512 `0xFF` bytes | `0xBD7BC39F` | match |

Three of those five are not in the crate's own tests. Castagnoli's check value
for `"123456789"` is `0xE3069283`; the crate is not producing it. **The hazard
the owner flagged did not land.**

**(12) Totality — holds on everything probed.** The suite's own sweep flips every
byte of the MBR, header and entry array with two masks and requires each parse to
return. Instrumented on a scratch copy: **34,816 parses, of which 1,852 (5.3%)
still locate the partition** — so the both-directions assertion
(`assert!(located > 0)`) has real margin rather than passing on one case. On top
of that, six hand-built hostile tables were run here (duplicate GUIDs, a
partition covering the backup GPT, a device that answers differently on the
second pass, the target in the last entry of a full 128-entry array); none
panicked, hung, or allocated.

**(14) "11,776 parses" — does not hold.** `tests/parse.rs:497` computes
`reach = (ARRAY_LBA + 32) * LBA = 34 * 512 = 17,408` bytes and runs two masks per
byte: **34,816**. Instrumented and printed. The commit message is off by 3×.
Against CLAUDE.md's rule that any number in a commit message comes from a command
that was run — and it is the specific failure that rule exists for, because
11,776 reads as perfectly plausible.

**(15) The bridge does not swallow a failed read — holds.** `kernel/src/gpt.rs:221`
is `if self.dev.read_blocks(block, 1, &mut self.buf).is_err() { self.cached = None; return false; }`
— it checks the status *and* drops the cache tag, with a comment explaining that
a failed read leaves the previous block's bytes in the buffer. This is the exact
defect `kernel-drivers.md` found in the NVMe path, and it is not repeated here.
`toyos_gpt::read` then turns `false` into `GptError::ReadFailed(lba)` and stops.
`toyos-fat32` likewise propagates every `IoError` through `?`; `hostile.rs`'s
`a_device_that_fails_mid_read_reports_it` covers it.

### Blocking: HEAD does not compile

`kernel/src/gpt.rs:221` calls `.is_err()` on `BlockDevice::read_blocks`.
`git show 677efae:kernel/src/block.rs` line 14 declares
`fn read_blocks(&mut self, lba: u64, count: u32, buf: &mut [u8]);` — return type
`()`, which has no `is_err`. `kernel/src/main.rs:31` has `mod gpt;`, so the
module is compiled. **`kernel/src/gpt.rs` was committed against an uncommitted
change to `kernel/src/block.rs`** (which is `M` in the working tree and does
return `BlockResult`). The working tree builds; HEAD does not.

Not a defect in the code under audit, and it resolves the moment the `block.rs`
change lands. Recorded because CLAUDE.md's "callee before caller" rule is exactly
about this, and because a future bisect through this range will fail to build for
a reason that has nothing to do with what it is bisecting.

---

## Part 2 — findings, ranked

### F1 (critical). An unvalidated cluster number reaches `BlockAccess::write_at`

**Location.** `fs.rs:205-212` (`node_from_entry`), `fat.rs:91-100` (`advance`),
reached through `fs.rs:353-368` (`cluster_at`) and `fs.rs:448-468`
(`write_allocated`).

**The bug the current shape permits.** `node_from_entry` validates the first
cluster only when the entry is a directory or has a non-zero size:

```rust
let needs_cluster = raw.is_dir() || raw.size() > 0;
if needs_cluster && !self.geom.valid_cluster(first) {
    return Err(Error::CorruptDirectory);
}
```

A file entry with `size == 0` and an arbitrary first cluster passes. `open`
returns a `File` carrying it. `write` then calls `ensure_capacity`, which for a
write of one cluster or less does not iterate; `cluster_at` calls
`advance(cluster, 0)`, whose loop body — the only place `next_cluster` and
therefore `valid_cluster` runs — does not execute; `contiguous_run(c, 1)` does
not iterate either. `write_allocated` computes
`self.geom.cluster_offset(cluster)` and issues the write.

`cluster_offset`'s own doc (`boot.rs:201-203`) says *"`cluster` must have passed
`Self::valid_cluster`; the caller's check is what makes the result land inside
the volume."* On this path there is no caller check.

**Reproduced.** A 64 MiB `newfs_msdos` volume presented on a device declaring 1
TiB of capacity (a partition with slack, or an adapter that reports the device
rather than the partition). One directory entry patched to `size = 0`,
`first_cluster = 536868866`:

```
volume is 67108864 bytes; device claims 1099511627776
crafted cluster 536868866 maps to byte 274877906944
write through a crafted zero-size entry -> Ok(())
bytes at the landing site after: "CLOBBERED BY FAT32"
landing site is OUTSIDE the declared volume (67108864 bytes)
```

`set_len` reaches the same place through `zero_range`, and was shown overwriting
19 bytes of pre-existing content at 100 GiB with zeroes.

**Blast radius.** Bounded by whatever `BlockAccess::write_at` refuses. On a
volume that exactly fills its partition the offset is out of range and the
adapter returns `IoError` (that is what `probe_g` saw: `write -> Err(Io)`). The
hazard is that `device.rs:23-26` tells the adapter author this cannot happen, so
an adapter that trusts it — and there is a kernel FAT32 adapter being written
right now — has nothing between a crafted ESP directory entry and a write
anywhere in the partition.

**Proposed shape.** The minimal fix is two lines. `fs.rs:205`:

```rust
fn node_from_entry(&self, raw: &RawEntry, loc: Loc) -> Result<Node, Error> {
    let first = raw.first_cluster();
    // A zero-length file has first cluster 0; anything else is inconsistent
    // before it is out of range, and this is the only site that decides
    // whether a number off the stick becomes a byte offset.
    let ok = if raw.is_dir() || raw.size() > 0 {
        self.geom.valid_cluster(first)
    } else {
        first == 0 || self.geom.valid_cluster(first)
    };
    if !ok {
        return Err(Error::CorruptDirectory);
    }
    Ok(Node { raw: *raw, first_cluster: first, loc: Some(loc) })
}
```

and `fat.rs:91`, so that `advance` is total rather than total-for-`steps > 0`:

```rust
pub(crate) fn advance(&mut self, cluster: u32, steps: u64) -> Result<Option<u32>, Error> {
    if !self.geom.valid_cluster(cluster) {
        return Err(Error::CorruptChain);
    }
    ...
```

**The shape that makes it unrepresentable**, and the reason this finding is also
the type-safety finding (see F11): a validated newtype, so `cluster_offset`
cannot be called with anything else.

```rust
/// A cluster number checked against this volume's geometry. The only way to
/// make one is `Geometry::cluster`, so a byte offset cannot be computed from a
/// number that came off the stick unchecked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cluster(u32);

impl Geometry {
    pub fn cluster(&self, n: u32) -> Option<Cluster> {
        (n >= 2 && n <= self.max_cluster()).then_some(Cluster(n))
    }
    pub fn cluster_offset(&self, c: Cluster) -> u64 { ... }
    pub fn fat_entry_offset(&self, fat: u32, c: Cluster) -> u64 { ... }
}
```

**What it deletes.** The nine runtime `valid_cluster` call sites
(`fat.rs:22`, `:33`, `:64`, `:146`, `:244`; `fs.rs:208`, `:313`, `:791`;
`boot.rs:265`) collapse to one constructor and three `Option` matches; `File`'s
"Cluster 0 means no position is known" convention (`fs.rs:74`) becomes
`Option<Cluster>`; `boot.rs:201-203`'s and `boot.rs:214`'s doc comments, whose
only job is stating the precondition and the unit, both go.

### F2 (critical). `flush_meta`'s stale-handle guard passes in the state every file starts in

**Location.** `fs.rs:589-601`, and the `File` doc at `fs.rs:57-63`.

**The bug the current shape permits.** The guard is

```rust
if raw.is_free() || raw.is_lfn() || raw.first_cluster() != f.entry_cluster {
    return Err(Error::NotFound);
}
```

`entry_cluster` is the first cluster *as the entry last recorded it*. For a file
created and not yet written, that is 0 — and 0 is also the first cluster of every
freshly created file. So when a directory slot is freed and taken by a new,
still-empty file, the equality holds and the guard passes. The stale handle then
writes its own size and first cluster into the newcomer's entry.

**Reproduced.** `create AAA.TXT` → `flush_meta` → `remove` → `create BBB.TXT`
(empty, takes the vacated slot) → `flush_meta` → write 4096 bytes through the
*stale* AAA handle → `flush_meta` through it:

```
flush_meta through a stale handle over an empty slot -> Ok(())
root: [DirEntry { name: "BBB.TXT", len: 4096, ... }]
  BBB.TXT len 4096 first8 [0, 133, 186, 147, 51, 194, 150, 127]
newcomer handle still says len 0
--- fsck ---
** Phase 3 - Checking for Orphan Clusters
Warning: 1 files, 64506 KiB free (129013 clusters)
```

`BBB.TXT` was never written to; it now reports 4096 bytes and reads back the
stale handle's data, while its own handle still says 0. `fsck_msdos` reports a
clean volume — no orphans — the
result is internally consistent and wrong, which is precisely the class the
volume-mirror and short-name tests were added for.

**Why it is not exotic.** The exposure window is any handle whose entry currently
records cluster 0: every handle between `create` and its first post-write
`flush_meta` (which is the whole of `common::write_new`, and every iteration of
`a_log_file_written_a_line_at_a_time` before the first flush), plus any handle to
a file that is legitimately empty. The existing test `a_stale_handle_is_refused`
(`hostile.rs:617`) removes a *12-byte* file, so it exits through the
`raw.is_free()` branch and never reaches the cluster comparison at all.

**Proposed shape.** A cluster number is not an identity. The 8.3 field is — it is
what FAT itself uses to distinguish entries in a directory.

```rust
pub struct File {
    loc: Loc,
    /// The 8.3 field of the entry this handle was opened on. The first cluster
    /// alone cannot say "still the same file": every empty file has cluster 0,
    /// and a slot freed and refilled by another empty file matches on it.
    entry_short: ShortName,
    entry_cluster: u32,
    ...
}

// flush_meta
if raw.is_free() || raw.is_lfn()
    || raw.short() != f.entry_short
    || raw.first_cluster() != f.entry_cluster
{
    return Err(Error::NotFound);
}
```

**What it deletes.** Nothing, and it costs 11 bytes per handle. It is the smaller
half of the fix; the larger half is F5.

### F3 (high). `set_len` on a cyclic chain frees the file's own live clusters and returns `Ok(())`

**Location.** `fat.rs:222-229` (`truncate_chain`) with `fat.rs:203-219`
(`free_chain`), reached from `fs.rs:560-574` (`shrink_chain`).

**The bug the current shape permits.** `truncate_chain` reads the tail, writes
`END_OF_CHAIN` at the keep point, then frees the tail. When the chain loops back
*through* the keep point, `free_chain`'s walk reaches it, reads the
end-of-chain marker `truncate_chain` has just written, and exits normally —
after freeing it. `free_chain`'s cycle detection ("a revisited cluster reads as
free") never fires, because the walk never revisits.

**Reproduced.** A 20,000-byte file at clusters 11→12→13→14→…, patched so
13's FAT entry points back at 11 (a rho closing above the truncation point).
`set_len(f, 600)` — keep two clusters:

```
set_len on a cyclic chain -> Ok(())
flush_meta -> Ok(())
cluster 11 -> 0x00000000
cluster 12 -> 0x00000000
cluster 13 -> 0x00000000
cluster 14 -> 0x0000000f
chain.bin size now 600
read -> Err(CorruptChain)
```

Every cluster the truncated file needs is now free, the directory entry still
names cluster 11 with size 600, and clusters 14 onward are orphaned. Both faults
in one operation, reported as success. The next allocation hands cluster 11 to
another file.

**Proposed shape.** The anchor is known; refuse if the walk reaches it.

```rust
pub(crate) fn truncate_chain(&mut self, cluster: u32) -> Result<(), Error> {
    let tail = self.next_cluster(cluster)?;
    self.set_fat_entry(cluster, END_OF_CHAIN)?;
    match tail {
        Some(t) => self.free_chain_below(t, cluster),
        None => Ok(()),
    }
}

/// Free a chain that must not run back into `anchor`. The end-of-chain marker
/// the caller just wrote there is an exit the walk would otherwise take, which
/// is how a cycle gets past `free_chain`'s "a revisited cluster reads as free".
fn free_chain_below(&mut self, start: u32, anchor: u32) -> Result<(), Error> {
    let mut c = start;
    for _ in 0..self.geom.cluster_count as u64 {
        if c == anchor {
            return Err(Error::CorruptChain);
        }
        ...
    }
    Err(Error::CorruptChain)
}
```

**What it deletes.** Nothing; `free_chain` becomes `free_chain_below(start, 0)`,
since cluster 0 is never a chain member. `fat.rs:86-90`'s residual paragraph
becomes true as written.

### F4 (high). `remove` on a cyclic chain frees clusters, then errors, leaving a live entry naming them

**Location.** `fs.rs:722-732`.

**The bug the current shape permits.** The order is free-then-erase:

```rust
if node.first_cluster != 0 {
    self.free_chain(node.first_cluster)?;
}
self.erase_entries(loc.dir_start, loc.first_index, loc.index)
```

`free_chain` on a cycle detects it — but only after freeing everything up to the
revisit. The `?` then propagates and the directory entry is never erased.

**Reproduced.** Same corpus, cluster 14 patched to point at 11:

```
remove -> Err(CorruptChain)
does the entry survive? Ok(Metadata { len: 20000, ... })
  cluster 11 -> 0x00000000
  cluster 12 -> 0x00000000
  cluster 13 -> 0x00000000
  cluster 14 -> 0x00000000
  cluster 15 -> 0x00000010
```

A live entry naming four free clusters. The volume still reads correctly — the
data is untouched — until something allocates, which is what makes it silent. In
the reproducer the next allocation happened to take a lower cluster, so the
byte-level cross-link was not observed; the FAT state is the finding.

**Proposed shape.** Erase the entry first. This is the ordering argument the
crate already makes twice — `append_cluster` (`fat.rs:186-191`) and `create_dir`
(`fs.rs:684-687`) both choose the order whose failure leaks rather than
cross-links. `remove` is the one place it was not applied.

```rust
// Entry first: a failure after this leaks clusters, which fsck reclaims. The
// other order leaves an entry naming free clusters, which fsck can only repair
// by guessing which file owns them.
self.erase_entries(loc.dir_start, loc.first_index, loc.index)?;
if node.first_cluster != 0 {
    self.free_chain(node.first_cluster)?;
}
Ok(())
```

`remove_dir` (`fs.rs:734-745`) has the same order and needs the same change.

**What it deletes.** Nothing. Two statements swap.

### F5 (medium). A write through a stale handle allocates, succeeds, and leaks everything it took

**Location.** `fs.rs:500-516` (`write`) — it never consults the directory entry.

**The bug the current shape permits.** `write` on a handle whose entry has been
freed returns `Ok(())` after allocating and writing real clusters. The
`flush_meta` that follows refuses (correctly, when the slot has not been
refilled), and nothing gives the clusters back. `write`'s own doc
(`fs.rs:486-490`) promises all-or-nothing — but this write did not fail, so no
rollback runs.

**Reproduced.**

```
write on a removed file -> Ok(()), flush -> Err(NotFound)
free before 66058752, after 65993216, lost 65536 bytes
fsck: Warning: Found 128 orphaned clusters
```

64 KiB written, 128 clusters permanently orphaned, on a volume whose only repair
tool is a host `fsck`. On the ESP that is unrecoverable in the field.

**Proposed shape.** Make `lib.rs:56-59`'s claim true by moving the check to where
it is claimed to be — one guard, called by both:

```rust
/// The directory entry this handle names, or `NotFound` if it no longer does.
fn live_entry(&mut self, f: &File) -> Result<(u64, RawEntry), Error> {
    let offset = self.entry_offset(f.loc)?;
    let raw = self.read_entry_at(offset)?;
    if raw.is_free() || raw.is_lfn()
        || raw.short() != f.entry_short
        || raw.first_cluster() != f.entry_cluster
    {
        return Err(Error::NotFound);
    }
    Ok((offset, raw))
}
```

`write`, `set_len` and `flush_meta` all call it. The cost is one sector read per
call, and it is almost always the sector already in `scratch` (`load_sector`
returns early on a hit), so on the append-a-line-at-a-time workload this crate
exists for it is free.

**What it deletes.** The three-condition guard inlined in `flush_meta`, and the
"can go stale" caveat in `File`'s docs stops being a caveat.

### F6 (medium). A crafted entry makes a file undeletable

**Location.** `fs.rs:205-212` and `fs.rs:722-732`.

An entry with `size == 0` and a garbage first cluster resolves fine, opens fine,
and cannot be removed:

```
metadata -> Ok(Metadata { len: 0, is_dir: false, ... })
open      -> Ok(0)
remove    -> Err(CorruptChain)
still there? Ok(Metadata { len: 0, ... })
read_dir  -> Ok(1)
```

`remove` calls `free_chain(garbage)` → `CorruptChain` → the entry survives,
forever. On the ESP, a log rotation that deletes old files wedges permanently on
one such entry. Closed by F1's `node_from_entry` fix (the entry is then
`CorruptDirectory` at resolve, so `remove` never reaches the free) — F4's
ordering fix would also make the entry removable.

### F7 (medium, `toyos-gpt`). A partition may claim the blocks the backup GPT sits on

**Location.** `lib.rs:288-292`.

`parse_header` accepts any `last_usable_lba < lba_count`. UEFI puts the backup
header at `lba_count - 1` and the backup entry array immediately below it, so a
table claiming `last_usable_lba == lba_count - 1` describes a disk where the last
partition overlaps its own backup GPT.

**Reproduced.** A table with `last_usable_lba = 4095` on a 4096-LBA disk and the
target partition spanning 34..4095:

```
partition covering the whole disk incl. backup GPT -> Ok(Located {
    partition: Partition { first_lba: 34, last_lba: 4095, ... }, ... })
```

`kernel/src/gpt.rs` then publishes that extent as `BootVolume`, and
`toyos-fat32` writes inside it. The firmware cross-check does not help: firmware
reads the *same* table, so it reports the same extent and the two agree.

**Notable**: `alternate_lba` at header offset 32 is the only field between
offsets 8 and 88 that `parse_header` never reads. Every other one is
(`le_u32`/`le_u64` at 8, 12, 16, 20, 24, 40, 48, 56, 72, 80, 84, 88).

**Proposed shape.**

```rust
// The backup header owns the last block and the backup array sits under it, so
// a table whose usable range reaches either is describing a disk where the last
// partition overwrites the copy of this table. Refused rather than clamped: the
// next thing anyone does with this answer is write.
let alternate_lba = le_u64(lba1, 32);
if alternate_lba != lba_count - 1 {
    return Err(GptError::BackupMisplaced(alternate_lba));
}
let backup_array_lbas = array_bytes.div_ceil(lba_bytes as u64);
if last_usable_lba >= alternate_lba.saturating_sub(backup_array_lbas) {
    return Err(GptError::UsableRange { first: first_usable_lba, last: last_usable_lba });
}
```

(`array_bytes` is already computed at `:303`; this needs it hoisted above the
usable-range check, or the check moved below it.)

### F8 (medium, `toyos-gpt`). Two entries with the same unique GUID: first one wins, silently

**Location.** `lib.rs:380` — `if unique_guid == target && found.is_none()`.

`kernel/src/gpt.rs:52` states the doctrine: *"The only safe answer to 'which is
mine' is then 'I do not know', forever, and never 'the first one I saw'."* It is
enforced across devices (`Resolution::Ambiguous`) and not within one table, where
the code is literally `found.is_none()`.

**Reproduced.** Two entries carrying the target GUID at 100..199 and 300..399:

```
duplicate unique GUID -> Ok(Located { partition: Partition { index: 0, ... }, used_entries: 2 })
```

`used_entries: 2` is reported but says nothing about the duplication, so the log
line reads "entry 0 of 2" and looks ordinary.

**Bounded, but not closed.** If the two entries overlap, `check_no_overlap`
refuses. If they are disjoint and entry 0 is the real one, the answer is right by
luck; if entry 0 is the decoy, the firmware extent cross-check refuses and the
machine has no boot volume. So the outcome is never a wrong *write*, but it is a
silent resolution of exactly the ambiguity the module exists to refuse.

**Proposed shape.** `scan_entries` already visits every entry.

```rust
if unique_guid == target {
    // Two entries claiming one unique GUID is the same fact as two devices
    // claiming it, and the kernel already refuses that. Refusing here keeps
    // one rule instead of two.
    if let Some(first) = found {
        return Err(GptError::DuplicateGuid { first: first.index, second: index });
    }
    found = Some(Partition { ... });
}
```

**What it deletes.** The `&& found.is_none()` conjunction, and the asymmetry
between `kernel/src/gpt.rs`'s two-device rule and the parser's within-table rule.

### F9 (low). `Loc.first_index` is set to a value that violates its own documented meaning

**Location.** `fs.rs:669` — `loc: Loc { dir_start: dir, first_index: index, index }`.

`Located.first_index` is documented at `dir.rs:188` as *"Index of the first entry
of the run — the first long-name entry, or the short entry when there is no long
name."* `insert_entry` returns the *short* entry's index (`dir.rs:508`), and
`create` puts it in both fields. For any file created with a long name, the
`File`'s `first_index` is `groups` entries too high.

No live bug today: `File.loc` is only ever read for `loc.index` (via
`entry_offset`), and the three `erase_entries` call sites all take their `Loc`
from `resolve`, which fills it correctly. It is a latent one — the first thing to
add a `remove_by_handle`, or to erase through a `File`, gets orphaned long-name
entries.

**Proposed shape.** `insert_entry` already knows both indices; return the `Loc`.

```rust
fn insert_entry(&mut self, dir_start: u32, name: &str, template: &RawEntry)
    -> Result<Loc, Error>
{
    ...
    Ok(Loc { dir_start, first_index: start, index: start + groups as u32 })
}
```

**What it deletes.** The three-field struct literal at `fs.rs:669`, and the
possibility of building a `Loc` whose fields disagree.

### F10 (low). Untyped units — cluster, sector, LBA, byte offset, entry index

**The shape.** Five distinct quantities, all spelled `u32` or `u64`:

- **cluster number** — `Geometry::cluster_offset(cluster: u32)`, `File::first_cluster`,
  `Loc::dir_start`, `DirScan::new(dir_start)`, `RawEntry::first_cluster()`
- **sector number** — `Geometry::sector_offset(sector: u32)`, `fsinfo_sector`,
  `first_data_sector`, `reserved_sectors`, `fat_sectors`, `total_sectors`
- **byte offset** — `read_at(offset: u64)`, `entry_offset`, `Extent::offset`
- **entry index within a directory** — `Loc::index`, `Loc::first_index`,
  `EntryCursor::offset_of(index)`
- **chain index** — `File::hint.0`, `cluster_at(index)`

`grep` finds 19 lines in `src/` that exist to say which unit an integer carries,
of which **10 are doc comments whose only job is that**: `boot.rs:37` ("Sectors
in one FAT."), `:201` and `:214` ("Byte offset of…"), `dir.rs:154` ("Byte offset
of entry `index`…"), `:186` and `:188` ("Index of…"), `fs.rs:33` ("Byte offset of
the sector…"), `:73` ("…as (index in chain, cluster)"), `:815` ("Free space, in
bytes."), and `fat.rs:102` ("Length of a chain in clusters…"). This is the same
shape `crates.md` reported for `toyos-ld` (17 lines whose only job was saying
which kind of `u64` each was), and
the same conclusion applies: the comments are load-bearing because the types are
not.

It is not academic here. F1 *is* this defect: a cluster number that had not been
checked was passed to a function whose doc comment says it must have been, and
nothing stopped it.

**Proposed shape.** `Cluster` first (see F1) — it is the one that carries a
validity invariant, so it buys safety and not just readability. `Sector` and
`ByteOffset` are readability-only and can follow or not. The measurable part is
the 9 runtime `valid_cluster` calls collapsing to one constructor, and 4 of the
10 unit doc-comments (`boot.rs:201`, `:214`, `dir.rs:154`, `fs.rs:33`) going
away because the signature says it.

### F11 (low). Three smaller items, stated without a proposed change

- **`check_no_overlap` decides on unchecksummed bytes.** `lib.rs:409-444`
  re-reads the entry array without recomputing the CRC. Demonstrated: a device
  that answers differently after the first pass gets its second answer trusted.
  A real device does not do this; a dying one might, and the outcome is either a
  spurious refusal of a good disk or a missed overlap. Cheap to close (keep a
  `Crc32` over the second pass too), and arguably not worth the code.
- **A lying-but-in-range FSInfo `free_count` is propagated, not recomputed.**
  `boot.rs:268` accepts any value `<= cluster_count`; the crate then adjusts it
  and writes it back at `sync`. `fsck_msdos` *does* check this field — verified:
  a poked value of 7 produced *"Free space in FSInfo block (7) not correct
  (129013)"* — so mounting a volume with a corrupt count and writing to it
  produces a volume the host calls broken. The crate's own accounting is correct:
  after a 1 MB write, a 1 KB write and a delete, the stored count was 127,067 and
  an independent FAT scan agreed exactly. Note this also means **the existing
  `image.fsck()` gate already covers FSInfo free-count correctness** — one of the
  probes this audit was asked to try turns out to be covered.
- **`Fat32::probe`'s doc says "Nothing in this crate writes"** (`fs.rs:130`). It
  means "nothing on the mount path writes", which is true and worth saying; as
  written it is false of a crate whose headline is that it writes the ESP.

---

## Part 3 — breakages tried that the authors did not

Fourteen probes were built and run. Six found something; eight did not, and the
eight are as much of the result as the six.

| # | probe | outcome |
|---|---|---|
| 1 | Stale handle over a slot reused by a **non-empty** file | **Refused** correctly (`NotFound`) — this is what the existing test covers |
| 2 | Stale handle over a slot reused by a **still-empty** file | **PASSED SILENTLY** → F2 |
| 3 | Write through a handle whose entry is gone | **PASSED SILENTLY** (`Ok`, 128 clusters orphaned) → F5 |
| 4 | `set_len` on a chain with a rho-shaped cycle | **PASSED SILENTLY** (`Ok`, live clusters freed) → F3 |
| 5 | `remove` on the same | Errored, **after** freeing; entry survives → F4 |
| 6 | Entry with `size == 0` and a wild first cluster, on a device larger than the volume | **PASSED SILENTLY** (wrote 256 GiB outside the volume) → F1 |
| 7 | Same, `set_len` path | **PASSED SILENTLY** (zero-filled outside the volume) → F1 |
| 8 | FSInfo `free_count` after a mixed write/delete workload | Correct — scan and stored value agree exactly |
| 9 | Does `fsck_msdos` notice a wrong FSInfo `free_count`? | **Yes** — so the existing gate covers it |
| 10 | BPB / backup BPB / volume label after a heavy workload | Untouched — exactly 2 bytes changed, both FSInfo |
| 11 | Short name colliding with the volume label (which `short_name_taken` skips) | Did not reproduce — the 8.3 basis truncates to 8 characters and did not collide. The skip is real (`DirScan::next` passes over volume labels and dot entries) but no reachable collision was found |
| 12 | GPT: duplicate unique GUID in one table | **PASSED SILENTLY** → F8 |
| 13 | GPT: partition covering the backup GPT | **PASSED SILENTLY** → F7 |
| 14 | GPT: device that answers differently on the overlap pass | **PASSED SILENTLY** → F11 |

Probes the prompt suggested that came back clean, stated explicitly because a
clean result is a result: **FSInfo free-count and next-free correctness**
(correct, and gated), **the volume label** (survives; slot 0 untouched),
**timestamps** (`RawEntry::set_write_time` writes the write time at 22/24 and the
access date at 18, which is the correct layout; `from_raw(..).to_unix_secs()` is
exercised over all 65,536 date words by `every_bit_pattern_decodes`), **the
reserved sector count and the BPB** (never written), **`.`/`..` entries** (written by `init_dot_entries`, repointed
by `set_dot_dot`, and `fsck_msdos` checks them — the existing
`rename_repoints_a_moved_directorys_parent` is the gate and it is a real one),
and **directory entry ordering** (`insert_entry` writes the long-name run
immediately before its short entry at `start..=start+groups`, contiguous by
construction).

---

## Part 4 — what was examined and deliberately not flagged

- **`Drop` guards that cannot fire.** Neither crate has an `impl Drop`. Nothing to
  flag; the CLAUDE.md caveat does not apply here.
- **A safety type that binds the wrong paths** (the `IpcPayload` shape). Neither
  crate has an `unsafe impl` or a marker trait; both are `#![forbid(unsafe_code)]`.
  The closest analogue is `flush_meta`'s guard, which *does* bind the wrong paths
  — filed as F2/F5 rather than as a separate type-safety item.
- **`MAX_DIR_ENTRIES = 65_536` as a per-directory bound.** It refuses a
  legitimately huge directory. Deliberate policy, documented with its reasoning
  at `dir.rs:24-32`, and an ESP will not reach it.
- **`insert_entry` scans the whole directory once per short-name candidate**, up
  to 64 times (`name.rs:283`). Worst case ~4.2 M entry reads per file creation,
  amortised 16× by the sector cache. Bounded and only reachable on an adversarial
  directory; not a correctness issue and not worth complicating the code for.
- **`zero_range` writes in 512-byte chunks** (`fs.rs:25`, `ZEROS`), each through
  the full `cluster_at` + `contiguous_run` path. Growing a file by 1 GiB is ~2 M
  device calls. Real, but the crate exists to append log lines; flagging it would
  be optimising for a workload nobody has.
- **`hybrid MBR → NoProtectiveMbr`** (`lib.rs:238-259`). Correct and deliberate,
  and worth knowing operationally: a USB stick written from a hybrid ISO carries
  exactly that layout, and the whole disk would be refused. The refusal is safe
  and the log line names it, so this is a note and not a finding.
- **`DeviceSectors::lba_count` rounds down to whole 4 KiB blocks**
  (`kernel/src/gpt.rs:209`), so the final partial block's LBAs are unreachable —
  including the backup header at the last LBA. Harmless today because there is no
  backup-GPT fallback; it becomes relevant when `issues/`' "no backup-GPT
  fallback" is closed.
- **Cross-linked (not cyclic) chains** — two files sharing clusters without a
  loop. No FAT driver detects this without a whole-volume pass, and `fsck` is the
  right place for it.
- **`toyos-fat32` has no format path.** Confirmed by reading: there is no code
  that writes offset 0, and the only sub-`first_data_sector` write is FSInfo at a
  sector required to be non-zero. This is the crate's strongest claim and it
  holds.

---

## What could not be checked

- Anything requiring the QEMU suite. There is no kernel FAT32 adapter yet, so
  nothing here was exercised against a real block device, a real ESP, or the page
  cache. `kernel/src/gpt.rs` was read but not run — `gpt::probe` is called for
  NVMe only (`main.rs:368`), and `issues/` already records that `boot_volume()`
  is `None` on every real machine.
- Whether `247c403`'s `DeviceSectors` behaves correctly against the NVMe driver's
  actual `BlockResult`, since HEAD does not compile (see above) and building the
  workspace was out of scope for this audit.
- The 16 deliberate breakages behind claim (5). The two named residuals were
  verified; the other 14 were applied to a scratch copy that no longer exists.
