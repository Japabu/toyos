# Type-safety audit: `bcachefs/`, `toyos-sched/`, `toyos-ps2/`, `toyos-ld/`, `toyos-cc/`

Read-only audit against CLAUDE.md's rule — *"C-isms tolerated only when the Rust
alternative adds no safety or value. Prefer compile-time safety: unrepresentable >
checked at runtime > covered by tests."* Nothing here was changed or built.

Verified against `3e2c975` (2026-08-01). Every count came from a command that was
run; figures derived from field sizes rather than measured say so.

**How findings are judged.** Two tests, either of which is sufficient:

1. **It permits a bug.** Named, with the path.
2. **The proposed code reads better or is smaller.** Both versions are written out
   below at a real call site so this can be checked rather than asserted. A change
   that deletes code or collapses special cases is favoured on that basis alone,
   and the deletion is counted, not estimated.

**Blast radius is reported as a fact for sequencing, never as an argument against
a change.** Nothing in this document is softened because it is large.

| crate | src lines | `#[must_use]` | `unwrap`/`expect`/`panic!` |
|---|---|---|---|
| `bcachefs` | 2174 | 0 | 21 |
| `toyos-sched` (core) | 4738 | 15 | 42 |
| `toyos-ps2` | 334 | 0 | 0 |
| `toyos-ld` | 6494 | 0 | 51 |
| `toyos-cc` | 9964 | 1 | 241 |

**One staleness correction, up front, because it changes what is worth fixing.**
The record this was written against listed "A 3 MiB `fs::write` to `/home` panics the
kernel — `bcachefs/src/btree.rs:184`, `MAX_PAYLOAD - used` underflows" as open and
assigned; nothing under `specs/issues/` carries it now. It is **closed at
`bccab15`** (2026-08-01): `btree.rs:184` is now
`Ok(Self { level, entries })`, and `Node::write_to` (`:203-207`) returns
`FsError::NodeOverfull` before the subtraction at `:217`. Not re-filed. Cited
throughout as the precedent — the class is alive in six other places in the same
file, on the read side that commit did not touch.

---

## Cross-cutting: the project already owns the pattern it is missing

`kernel/src/mm/mod.rs:73` and `:117` define `UserAddr(u64)` and `DirectMap(u64)`;
`kernel/src/process.rs:203` comments its `UserStack` with *"Impossible to confuse
the two."* `kernel/src/mm/pmm.rs:108`'s `PhysPage` cannot leak because `Drop`
returns it to the PMM. The kernel pays the newtype cost exactly where address
spaces meet.

`toyos-ld` handles four coordinate systems at once — absolute virtual address, PE
relative virtual address, file offset, section-relative offset — and spells all
four `u64`. `bcachefs` defines `BlockNum(u64)` (`block_io.rs:7`) and then exports
`Extent { start_block: u64, .. }` (`fs.rs:14-18`) across the crate boundary, so
`kernel/src/file_backing.rs:39-49` does raw block arithmetic on bare integers.

The pattern is not unknown here. It is unevenly applied, and the two places it is
absent are the two that parse untrusted bytes and emit executables.

---

## 1. `bcachefs/`

### What this crate actually is

**It does not implement bcachefs.** It is a ToyOS-native on-disk format written
from scratch that shares only the name: `MAGIC = b"BCFS"` (`superblock.rs:5`),
`NODE_MAGIC = b"BTND"` (`btree.rs:7`), one 4096-byte block size, one flat
namespace of `(siphash(name), key_type)` keys in a B+ tree, extents inline in the
leaf value, a bitmap allocator, **no journal** (`journal_blocks = 0u32; // Phase
2`, `fs.rs:317`), no snapshots, no data checksums, no replication. Upstream
bcachefs is a UUID-based `BCHFS_MAGIC` / `BSET_MAGIC ^ sb.uuid` / `JSET_MAGIC`
format with none of these properties.

**A reader who audits this as bcachefs will look for the wrong things.** There is
no journal to check for replay soundness, no bset iteration, no `bch_val` union to
validate. There is one 4096-byte node format, and everything below is about how it
is parsed. `specs/issues/kernel/` already asks the owner to rename the crate;
this audit does not reopen that.

### The trust boundary, precisely

`bcachefs_adapter::probe()` (`kernel/src/bcachefs_adapter.rs:363`) reads block 0
of the NVMe namespace and mounts **any** device carrying a `BCFS` superblock whose
CRC32C checks out. A CRC is not authentication: whoever writes the image writes the
CRC. So every on-disk field — superblock contents, node headers, entry counts,
value bytes, child pointers, tree depth — is attacker-controlled input on any
machine that boots with a disk somebody else prepared. That is the metal track's
situation exactly (`specs/metal-boot-plan.md`), and `specs/issues/kernel/`
already accepts the analogous statement about the NVMe namespace ("the device said
so is not a bound").

### Findings

#### B1 — Six sites panic the kernel on an interior node whose value is shorter than 8 bytes

`btree.rs:260`, `:264`, `:385`, `:418`, `:517`, `:625` — six occurrences, counted.
`Node::parse` (`:148-182`) accepts `val_len = 0`; it only rejects
`val_end > BLOCK_SIZE`. A level-1 node whose first entry has a zero-length value
makes `entry.value[..8]` a slice-index panic, inside `vfs::lock()` —
`specs/issues/panic-path/`'s open "the VFS lock is the same shape" class.
`btree.rs:258`'s `debug_assert!(!node.entries.is_empty())` is compiled out in the
kernel and is followed immediately by `node.entries[0]`, so an *empty* interior
node is the same panic by a second route.

This is `specs/issues/isolation/`'s stated lesson verbatim — *a policy enforced at
one entry point was simply absent at another that reaches the same machinery.*
`bccab15` made the **write** side fallible because "`used` is a sum over values
whose size userland chooses"; the **read** side got nothing.

**Both ways, at `btree.rs:254-270` (`find_child`), the worst of the six:**

```rust
// current
fn find_child(node: &Node, key: &Key) -> BlockNum {
    debug_assert!(!node.entries.is_empty(), "interior node has no children");
    let mut child_block =
        BlockNum::new(u64::from_le_bytes(node.entries[0].value[..8].try_into().unwrap()));
    for entry in &node.entries {
        if entry.key <= *key {
            child_block =
                BlockNum::new(u64::from_le_bytes(entry.value[..8].try_into().unwrap()));
        } else {
            break;
        }
    }
    child_block
}
```

```rust
// proposed — the parse already produced BlockNums, so there is nothing to decode
fn find_child(children: &[(Key, BlockNum)], key: &Key) -> BlockNum {
    let mut child = children[0].1;                 // NodeBody::Interior is never empty
    for (k, block) in children {
        if k <= key { child = *block } else { break }
    }
    child
}
```

with the decode moved once into the parse:

```rust
/// A node's payload, already validated against its level and the device.
pub enum NodeBody {
    Leaf(Vec<Entry>),
    /// Never empty; every child parsed and range-checked at parse time.
    Interior(Vec<(Key, BlockNum)>),
}

impl Node {
    fn parse(buf: &BlockBuf, block: BlockNum, device_blocks: u64) -> Result<Self, FsError>;
}
```

The proposed version is shorter, has no `unwrap`, no `debug_assert`, and no
`from_le_bytes`. `Node::read` already holds `io: &dyn BlockIO`, which has
`block_count()`, so `Interior` can range-check the child in the same step — which
closes the out-of-range `read_block` as a side effect rather than as a second fix.

**What goes away (counted):** 6 `value[..8].try_into().unwrap()` decodes; 1
`debug_assert`; `Node.level: u16` becomes derived from the variant, deleting **11**
function parameters spelled `level: u16`/`root_level: u16` in `btree.rs`, **8**
`if level == 0` arms and **7** `level - 1`/`level -= 1` expressions; `Node::is_leaf`
(`btree.rs:110-112`) has **zero callers** and goes with them.

**Blast radius:** `btree.rs` throughout, `fs.rs` at 22 `root_level` mentions,
`kernel/src/bcachefs_adapter.rs` unchanged. This is the same edit as B2 and they
should land together.

#### B2 — Tree depth comes from the superblock and drives unbounded recursion

`superblock.rs:99` reads `root_level: read_u16(b, 32)` with no validation anywhere;
`Mounted::open` (`fs.rs:483-498`) stores it and checks nothing. Three functions
recurse on it: `collect_recursive` (`btree.rs:401-423`, reached from
`Mounted::list`), `delete_matching_recursive` (`:366-392`), `find_min_key`
(`:616-628`, reached from `btree::insert` on a root split). A node whose child
pointer names itself gives depth = `root_level`, up to 65535, against a 128 KiB
kernel stack (`kernel/src/process.rs:199`). One `ls /home` on a crafted disk.

`search` and `delete` are loops and are unaffected — which is itself the argument
that the recursive three are an accident of writing.

**Both ways, at `btree.rs:401-423`:**

```rust
// current
fn collect_recursive(io: &dyn BlockIO, block: BlockNum, level: u16,
                     results: &mut Vec<Entry>) -> Result<(), FsError> {
    let node = Node::read(io, block)?;
    if level == 0 {
        for entry in node.entries { .. }
    } else {
        for entry in &node.entries {
            let child = BlockNum::new(u64::from_le_bytes(entry.value[..8].try_into().unwrap()));
            collect_recursive(io, child, level - 1, results)?;
        }
    }
    Ok(())
}
```

```rust
// proposed — with B1's NodeBody, the level parameter disappears entirely
fn collect_recursive(io: &dyn BlockIO, block: BlockNum, depth: Depth,
                     results: &mut Vec<Entry>) -> Result<(), FsError> {
    match Node::read(io, block)?.body {
        NodeBody::Leaf(entries) => results.extend(
            entries.into_iter().filter(|e| e.key.key_type != KeyType::Deleted)),
        NodeBody::Interior(children) => {
            let deeper = depth.descend()?;          // Err(CorruptedNode) at the floor
            for (_, child) in children {
                collect_recursive(io, child, deeper, results)?;
            }
        }
    }
    Ok(())
}
```

```rust
/// A descent budget. A B+ tree over N blocks with >= 2 entries per interior
/// node is at most log2(N) deep, so 64 covers any device that can be addressed.
#[derive(Clone, Copy)]
pub struct Depth(u8);

impl Depth {
    const MAX: u8 = 64;
    fn parse(raw: u16) -> Result<Self, FsError> { .. }
    fn descend(self) -> Result<Self, FsError> { .. }
}
```

The proposed version has no `level == 0` branch (the variant is the branch), no
`level - 1`, and the depth bound is enforced by the only operation that can go
deeper. It is shorter and the recursion is bounded by construction.

**What goes away:** the 11/8/7 counts from B1 are shared with this finding — they
are one edit. Additionally `Superblock.root_level: u16` becomes `Depth`, which
makes the 22 `root_level` uses in `fs.rs` type-checked rather than integer.

#### B3 — A partial allocation is treated as a complete one, and the write lands outside the extent

`fs.rs:823-843`. **This is a live data-corruption path, not a hardening gap.**

```rust
// current
let needed = page_idx + 1 - cursor;
let (start, count) = self.alloc.alloc_contiguous(&self.io, needed)?;
push_extent(extents, start.raw(), count);
Ok(start.raw() + (page_idx - cursor) as u64)          // == start + needed - 1
```

`alloc_contiguous` is documented (`alloc_bitmap.rs:82-85`) as *"Try to allocate
**up to** `wanted` contiguous blocks. Returns (start_block, actual_count) where
actual_count >= 1"*, and `:171` is `let count = best_count.min(wanted)`. When the
bitmap has no run of `needed` free blocks it returns a shorter one. The function
then returns `start + needed - 1`, which is **past the end of the run it just
recorded** whenever `count < needed`.

`bcachefs_adapter::write_page` (`kernel/src/bcachefs_adapter.rs:185-191`) feeds
that block straight to `page_cache::raw_block_write`. The block is free, or belongs
to another file. The extent list records only `count` blocks, so a later read of
the same page takes the loop at `:830-835` and resolves to a *different* block: the
write is lost **and** a foreign block is clobbered.

Both `write_data`s (`fs.rs:409-441`, `:683-714`) get this right —
`while remaining > 0 { .. remaining -= count }`. `resolve_or_alloc_block` is the
one caller that does not loop.

Not reproducible on a freshly formatted `/home`, where the allocator hands out
contiguous runs. It needs a fragmented bitmap and a sparse write (`seek` past EOF,
then `write`), so `needed > 1`.

**This is `specs/issues/isolation/`'s filed class in a new shape.** That entry
("two syscalls discard a failure signal they already have") is about a caller
ignoring *no*. This caller ignores *yes, partially* — worse, because there is no
refusal to notice and the wrong answer is a block number that looks exactly like a
right one.

**Both ways:**

```rust
// current signature — the second element is a trap
pub fn alloc_contiguous(&mut self, io: &dyn BlockIO, wanted: u32)
    -> Result<(BlockNum, u32), FsError>;
```

```rust
// proposed
/// A run the allocator actually reserved. `len` is what you got, never what
/// you asked for.
pub struct Run { pub start: BlockNum, pub len: u32 }

pub fn alloc_up_to(&mut self, io: &dyn BlockIO, wanted: u32) -> Result<Run, FsError>;
/// All of it or nothing, for callers that cannot place a short run.
pub fn alloc_exact(&mut self, io: &dyn BlockIO, count: u32) -> Result<Run, FsError>;
```

and the call site becomes the loop its two siblings already are:

```rust
// proposed resolve_or_alloc_block tail
let mut remaining = page_idx + 1 - cursor;
while remaining > 0 {
    let run = self.alloc.alloc_up_to(&self.io, remaining)?;
    push_extent(extents, run.start.raw(), run.len);
    remaining -= run.len;
}
self.block_for(extents, page_idx).ok_or(FsError::NotFound)   // reuse the walk above
```

`(BlockNum, u32)` reads as "here is your block and how many" and is destructured
positionally at three sites; `Run { start, len }` cannot be read as "all of it".
`alloc_block` (`alloc_bitmap.rs:75-80`) — currently `match self.alloc_contiguous(io, 1)? { (block, 1) => Ok(block), _ => unreachable!() }` —
becomes `Ok(self.alloc_exact(io, 1)?.start)`, deleting the `unreachable!()`.

**What goes away:** 1 `unreachable!()`; the positional `(start, count)`
destructuring at 3 sites; the divergence between three callers of one allocator.

**Two smaller defects in the same function, fixed by the same edit:**
`cursor += ext.block_count` (`fs.rs:834`) is `u32` addition over disk-derived
counts — a panic under the kernel's `[profile.dev]` overflow checks, which is
exactly how `bccab15`'s underflow presented — and `page_idx + 1` (`:838`) overflows
at `u32::MAX`.

#### B4 — `Vec::with_capacity` sized from a `u16` on disk, 387× the physical maximum

`btree.rs:143,145`:

```rust
let entry_count = u16::from_le_bytes(b[10..12].try_into().unwrap()) as usize;
let mut entries = Vec::with_capacity(entry_count);
```

A 4096-byte block has `BLOCK_SIZE - NODE_HEADER_SIZE = 4064` payload bytes and
every entry costs at least `KEY_HEADER_SIZE = 24`, so **169** entries is the
physical maximum. The field admits 65535.

`Entry` is `Key { u64, u64, KeyType(repr(u16)) }` plus `Vec<u8>` — 42 bytes of
fields at alignment 8, so at least 48 (derived from field sizes, not measured;
`repr(Rust)` gives no layout guarantee, but no layout is smaller than 48).
`65535 × 48 = 3,145,680` against `mm::MAX_HEAP_ALLOC = PAGE_2M - 4096 = 2,093,056`
(`kernel/src/mm/mod.rs:63`), which is an `assert!` in `KernelAllocator::alloc`
(`kernel/src/mm/alloc.rs:115`). One crafted 4096-byte block panics the kernel.

```rust
// proposed — three lines, no invented number
const MAX_ENTRIES: usize = (BLOCK_SIZE - NODE_HEADER_SIZE) / KEY_HEADER_SIZE; // 169
if entry_count > MAX_ENTRIES {
    return Err(FsError::CorruptedNode(block));
}
```

This is the shape `specs/issues/isolation/` prefers to a bound — *count by type,
then reserve exactly*. `fs.rs:266`'s sibling `with_capacity(extent_count)` is **not**
a defect: that count is derived from the value's own length, which `Node::parse`
has already bounded.

**Blast radius:** four lines in `btree.rs`.

#### B5 — `Superblock::parse` validates the envelope and none of the contents

`superblock.rs:70-110` checks magic, version and CRC, then builds a `Superblock` of
nine raw integers with no cross-check: `block_count`, `root_node`, `root_level`,
`next_alloc`, `free_blocks`, `bitmap_start`, `bitmap_blocks`, `journal_start`,
`journal_blocks`. `Mounted::open` (`fs.rs:483-498`) copies five straight into a
`BitmapAllocator` and mounts.

`BlockNum` (`block_io.rs:7`) names the *unit* and asserts nothing about the
*range* — `BlockNum::new` is `pub const fn` over any `u64`. A `BlockNum` from disk
is indistinguishable from one the allocator produced. That is
`specs/issues/isolation/`'s class header ("an id or a name treated as a
capability") applied to a disk block.

Reachable consequences, all from a mounted foreign disk:

- `bitmap_start` anywhere: `set_used`/`set_free`/`is_free`
  (`alloc_bitmap.rs:24,38,66`) compute `bitmap_start + byte_idx / 4096` with no
  check against `bitmap_blocks` or `total_blocks`, so bitmap writes land on
  arbitrary blocks.
- `block_count` above the device: `Superblock::read` (`:147`) uses
  `io.block_count() - 1` for the backup, but `Superblock::write` (`:159`) uses
  `self.block_count - 1` — an underflow at 0 and an out-of-range write above it.
- `free_blocks`/`next_alloc` inconsistent with the bitmap: `alloc_contiguous`
  returns `NoSpace` or hands out used blocks depending on which lie.

**Both ways:**

```rust
// current — the only external fact is thrown away
pub fn parse(buf: &BlockBuf) -> Result<Self, FsError>;
pub fn read(io: &dyn BlockIO) -> Result<Self, FsError> {
    io.read_block(BlockNum::new(0), &mut buf);
    match Self::parse(&buf) { .. }
}
```

```rust
// proposed — the device is the arbiter, and it is already in scope
/// `device_blocks` is what the block device reports. A superblock that does
/// not describe *this* device is refused, not repaired.
pub fn parse(buf: &BlockBuf, device_blocks: u64) -> Result<Self, FsError>;
pub fn read(io: &dyn BlockIO) -> Result<Self, FsError> {
    let n = io.block_count();
    io.read_block(BlockNum::new(0), &mut buf);
    match Self::parse(&buf, n) { .. }
}
```

with the checks stated once, in the one place that has all of them: `block_count <=
device_blocks`; `root_node`, `bitmap_start`, `journal_start` all `< block_count`;
`bitmap_start + bitmap_blocks <= block_count`; `bitmap_blocks >= ceil(block_count /
32768)`; `free_blocks <= block_count`; `next_alloc < block_count`; `root_level` via
B2's `Depth::parse`.

The stronger version, and the one worth doing, makes the range part of the type so
the checks cannot be skipped by a future reader:

```rust
/// A block number proven to be on the device it came from. Constructible only
/// from a parse that had the device size, or from the allocator, which owns it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockNum(u64);

impl BlockNum {
    pub(crate) const fn trusted(n: u64) -> Self { Self(n) }   // allocator, mkfs
    pub fn parse(n: u64, device_blocks: u64) -> Result<Self, FsError> { .. }
}
```

`BlockNum::new`'s 24 call sites split into the two cases, and the `read_block`
range checks that B1 would add per-site stop being needed at all: a `BlockNum` on
the device is the only kind that exists.

**Blast radius:** `superblock.rs` (`parse`, `read`), `fs.rs:484`, `block_io.rs`,
and the `std`-side `mkfs` path in `src/build.rs` via `Formatted` (which uses
`trusted`). `kernel/src/bcachefs_adapter.rs` is unchanged — it calls
`Mounted::open`.

#### B6 — `read_extents` sizes a heap allocation from the on-disk file size

`fs.rs:284-303`: `let mut data = vec![0u8; size as usize];` where `size` is
`u64::from_le_bytes(value[3..11])` from a leaf value; `:294` then reads
`BlockNum::new(ext.start_block + i)` with no range check.

**Not currently a kernel path — established, not assumed.** The adapters use
`file_extents` + demand paging (`bcachefs_adapter.rs:97,208,258,263,310`) and never
call `Mounted::read_file`. It is listed because `read_link` *is* on the adapter
(`:85`, `:251`) and reaches `read_extents` through `LeafValue::Symlink`, so the
on-disk size is already partly live.

B5's `BlockNum::parse` closes the block half. The size half wants
`size <= extents.iter().map(block_count).sum() * BLOCK_SIZE`, which is one line and
is the honest bound (a file cannot be longer than the blocks it names).

#### B7 — `BlockNum::to_byte_offset` multiplies unchecked

`block_io.rs:18-20`: `self.0 * BLOCK_SIZE as u64`, next to a `checked_add` at
`:22-27`. No caller passes a disk-derived block number to it in this tree today.
`checked_byte_offset() -> Option<u64>` is three lines and removes the asymmetry;
under B5 it stops mattering, because a `BlockNum` is on the device by construction
and the product cannot overflow.

### Examined and deliberately not flagged (bcachefs)

- **`BlockNum` as a newtype** — right shape, works. The finding is that it does not
  travel (`Extent.start_block: u64`, `fs.rs:15`) and carries no range (B5), not
  that it should not exist.
- **`KeyType` / `TryFrom<u16>`** (`btree.rs:76-86`) — exactly right: the raw
  discriminant is parsed once into an enum and a bad value is
  `FsError::CorruptedKey`, not a panic. The model B1 asks for.
- **`Formatted` / `Mounted<IO, Mode>` typestate** (`fs.rs:65-81`) — `ReadOnly` and
  `ReadWrite` as `PhantomData` put every mutating method behind
  `impl<IO: BlockIO> Mounted<IO, ReadWrite>`. A write on the initrd does not
  compile.
- **`decode_leaf_value`** (`fs.rs:246-281`) — bounds-checked, `from_utf8` mapped to
  an error, unknown `entry_type` refused. The one function in the crate that parses
  untrusted bytes properly.
- **`Mounted::list` unbounded** — already filed (`specs/issues/isolation/`). Not
  re-filed. Note only that B4's per-node cap does not close it: `collect_all`
  accumulates across nodes.
- **`Formatted::format`'s `total_blocks - metadata_blocks`** (`alloc_bitmap.rs:210`,
  `:220`) — underflows on a device smaller than its own metadata. Reachable only
  through `Storage::Designated`, i.e. a disk somebody deliberately stamped with that
  device's own block count. Real, and B5's `parse` does not cover the format path;
  a `checked_sub` with `FsError::NoSpace` is the whole fix.
- **`SliceBlockIO::write_block`'s `panic!("read-only")`** (`block_io.rs:150`) —
  unreachable: `Mounted<SliceBlockIO, ReadOnly>` exposes no method that writes. The
  typestate already makes it dead code and it should be deleted, but it is not a
  defect.
- **`ftruncate` not persisting on `/home`** — already filed.
- **`siphash_2_4` "simplified"** — hash quality, not type safety. Out of scope.

---

## 2. `toyos-sched/`

### The verdict, answered rather than deferred

The spec states its own claims as a table (`specs/scheduler-core-spec.md` §2, B1–B10)
with a declared fate for each: **CT** = compile-time impossible, **RT** = runtime
fail-fast, **SIM/LOOM** = explored. That table is the right thing to audit against,
because it is what the design promised. Verdict per row, with the code path when a
caller can still do the thing:

| # | claim | declared | **actual** | why |
|---|---|---|---|---|
| B1 | task in two places at once | CT | **CT** | `Task<X>(Box<TaskInner<X>>)` has a private field, no `Clone`/`Copy` impl, and `TaskInner` is private (`task.rs:547-549`). The five wrappers are `pub struct $name<X>(Task<X>)` with private fields (`:646-650`), so outside `task.rs` a `ReadyTask` cannot be built from a `Task` at all. Every transition takes `self`. Nothing to circumvent. |
| B2 | lock guard leaked across the switch | CT + RT | **CT** for the core | The core implements no lock (`sync.rs:27-29`: it would need `unsafe`, which only `mailbox.rs` may write). `SchedPass::finish(self)` consumes the `&mut CpuSched` borrow and returns an `Action` holding raw pointers, so no guard can outlive the pass. `WaitQueue`'s `LeafLock` is never held across a post — `wake_one` closes the `with` before `deliver_wake` (`waitq.rs:154-160`). |
| B3 | five lost-wake windows | CT + LOOM | **CT for "a park needs a commit"; RT for "…of *this* task"** | `RunningTask::park` requires `&CommittedTicket` (`task.rs:833-839`), and `CommittedTicket`'s fields are private with no public constructor — it is produced only by `WaitTicket::commit` (`waitq.rs:389-401`). So parking without a won commit CAS is unwritable. **Which** task the ticket belongs to is two runtime asserts (`task.rs:841-846`), not a type. |
| B4 | ready task stranded on a sleeping CPU | RT + SIM + LOOM | **CT at the crate boundary** — stronger than claimed | `SleepToken::new` is private (`cpu.rs:88`) and needs a `Quiesced` (whose `_private: ()` field, `mailbox.rs:523-525`, makes it unforgeable outside `mailbox.rs`) plus a `TimerApplied`. `Machine::idle_wait(token: SleepToken)` is the only halt-with-proof path. The kernel cannot forge one. |
| B5 | deadline armed too late | CT | **CT on the idle path; convention on the run path** | See S1 below. This is the one row where the ledger overstates. |
| B6 | retire scans / KILLED[16] / 1 s timeout | CT + protocol | **CT that a retire is claimed; RT that there is only one** | `retire::begin` returns a `#[must_use] RetireTicket` and `claim_retire` is `pub(crate)` (`task.rs:445`), so the kernel must go through `begin`. "Exactly one retirer" is `assert!(shared.claim_retire())` (`retire.rs:25-29`) — a fail-fast, not a type. |
| B7 | RT wake does not preempt promptly | protocol + SIM | **`#[must_use]` lint only** | See S2 below. |
| B8 | silent wake drops on queue overflow | CT | **CT for overflow; RT for single-claim** | Nodes are embedded in the objects the messages are about (`task.rs:274-276`, `:561`), so there is no capacity anywhere and overflow genuinely has no representation. "One message per node" is `claim() -> Option<PostSlot>` with `.expect(..)` at the wake and retire sites (`waitq.rs:224-227`, `retire.rs:73-76`) — RT. |
| B9 | scheduler untestable off-target | architecture | **holds** | `hw::Hw` is the whole boundary; `toyos-sched/sim` and `toyos-sched/loom` compile the same sources. |
| B10 | IRQ-time timestamps | landed elsewhere | n/a | Front-loaded under the old scheduler. |

**The answer: delivered.** Not decoration, and not close to it. Four of the design's
load-bearing impossibilities — a task in two containers, a lock across the switch,
a park without a won commit, a mailbox that can overflow — are genuinely
unwritable, and each is unwritable because of a *structural* choice (private
fields, consuming transitions, embedded nodes, an unforgeable proof token) rather
than an assert. B4 is stronger than the spec claimed for itself. The pattern is
worth applying elsewhere on this evidence.

**The partition, stated precisely, because it is what a reader needs.** Enforced by
**types**: the task's linearity and exactly-once death; wrong-CPU access to a
`CpuSched` (`!Sync` via `PhantomData<*mut ()>`, `cpu.rs:161` — a `static` of it does
not compile, and `Hw` has no `cpu_id()` so an ambient wrong-CPU query has no
expression, `hw.rs:15-16`); registering someone else's task for a wait
(`CurrentTask` is produced only by `CpuSched::current_task`, `cpu.rs:202-206`);
parking without a commit; halting with work queued or with a deadline unarmed;
mailbox overflow. Enforced by **runtime assert or convention**: the ticket↔task
identity match; the single-retirer rule; one-message-per-node; the timer arming on
the run path (S1); the kick obligation (S2); the whole of `TaskShared`'s state
machine, which is `pub` (S3); and the `!Sync` guarantee at the kernel boundary,
which is re-established by an `unsafe impl` plus a runtime flag (S4).

**On the intrusive `wait_node` specifically** — the textbook version of this
question, and the reason CLAUDE.md flags it. The *mailbox* nodes are the answer to
"can an intrusive structure be safe?" and it is yes: `MailboxNode::claim` hands out
a `#[must_use] PostSlot` whose `Drop` releases the claim, so a forgotten post
cannot wedge the node (`mailbox.rs:189-205`); `MailboxNode::drop` asserts
`!in_flight`, so freeing a node with a queued message is a loud panic at the site
rather than a use-after-free later (`:173-184`); `post` requires an
`unsafe trait PreemptGuard` value, so an unguarded push does not typecheck (`:60`,
`:255`). N1, N2 and N3 are each a type. In C all three are comments. The **wait**
node is the one that was never built — `WaitList` holds `Arc<TaskShared>` in a
`VecDeque` (`waitq.rs:41`), and the guard that stands in for it cannot fire on the
path it exists for. That is S5, and it is the sharpest finding in this section.

### Findings

#### S1 — Invariant T is a type on the idle path and a convention on the run path

`Action` (`cpu.rs:100-110`) has three variants. `Idle(SleepToken)` carries proof
that the timer was programmed — `SleepToken::new(quiesced, timer)` needs a
`TimerApplied`. `Run(RunToken)` and `Resume` carry nothing. So:

```rust
// cpu.rs:910-931, current — the first line is the whole of invariant T here
fn switch_to_current(&mut self) -> Action<<H as Hw>::Payload> {
    self.apply_timer();                    // discards TimerApplied
    ..
    Action::Run(RunToken { restore, save, incoming, outgoing })
}
```

Deleting that first line compiles. `TimerPlan` is `#[must_use]`
(`timer.rs:90`) but `apply_timer` already consumed it, and `TimerApplied` is
dropped on the floor. The RT backstop is
`#[cfg(feature = "check")] crate::invariants::check_cpu(self.cpu)` inside
`apply_timer` (`cpu.rs:905-906`) — and `specs/issues/kernel/` records that
`src/build.rs` cannot enable `sched-check`, **so no CI run exercises it**. A quantum
never armed is therefore caught by nothing.

```rust
// proposed — the proof travels with every action, not just the idle one
pub enum Action<X: SchedPayload> {
    Run(RunToken<X>, TimerApplied),
    Resume(TimerApplied),
    Idle(SleepToken),
}
```

The driver already matches on `Action` exhaustively
(`kernel/src/sched/driver.rs:476`), so the extra binding is `_` at two arms. The
gain is that `switch_to_current` and `switch_to_idle` cannot construct their
variant without calling `apply_timer`, which is exactly what the spec says
`finish()` guarantees. This makes B5's declared **CT** true on all three paths
instead of one.

**Bug permitted today:** none observed — both call sites do call `apply_timer` first
(`cpu.rs:911`, `:937`). The finding is that the ledger claims CT and delivers
convention, on the row whose observed bug was *"2.9 ms sleep honored 7+ ms late"*.

**Blast radius:** `cpu.rs` (3 variants, 3 constructors), `kernel/src/sched/driver.rs`
(one match), the simulator's `Action` handling.

#### S2 — The kick obligation is a `#[must_use]` lint, and folding it into `post` deletes five branches

`Kick` (`mailbox.rs:432-437`) is `#[must_use = "an elided kick is a decision; a
required kick must be sent"]`. `must_use` is a lint: ignoring the return warns,
it does not fail the build, and the crate is consumed by the kernel where
`--cap-lints` policy for path dependencies is the tree's zero-warning bar rather
than a hard error. Five sites, counted, all currently correct:
`waitq.rs:229`, `cpu.rs:422`, `cpu.rs:1001`, `retire.rs:79`,
`kernel/src/sched/driver.rs:265`.

**Both ways, at `waitq.rs:224-231`:**

```rust
// current
let slot = shared.wake_node().claim()
    .expect("the wake claim admits one poster: node must be free");
let handle = cpus.get(cpu);
if handle.post(slot, M::wake(shared.key(), cause), cause.urgency(), preempt) == Kick::Send {
    kicker.kick(cpu);
}
```

```rust
// proposed
let slot = shared.wake_node().claim()
    .expect("the wake claim admits one poster: node must be free");
cpus.get(cpu).post(slot, M::wake(shared.key(), cause), cause.urgency(), preempt, kicker);
```

Every one of the five sites already has a `Kicker` in scope: `deliver_wake` and
`post_retire` take `kicker: &impl Kicker`; `hand_off` and `post_steal_probe` have
`env.hw`, which is a `Machine: Kicker`; the driver has `HW`. So the parameter costs
nothing at any call site.

**What goes away (counted):** the `Kick` enum and its `#[must_use]` (6 lines,
`mailbox.rs:431-437`); five `if .. == Kick::Send { kicker.kick(..) }` branches;
`Doorbell::ring`'s return value threading through `CpuHandle::post` and
`post_owned`. Net: roughly 25 lines and one class of "posted but never kicked",
which becomes unrepresentable rather than lint-checked.

**The counter-argument, considered and rejected.** Separating a decision from its
effect is a deliberate and good pattern in this crate — `TimerPlan`/`TimerApplied`
and `SleepArm`/`halt` both do it. But those separations *buy a proof token* that
something downstream consumes. `Kick` buys nothing: there is no `Kicked` type and
nothing consumes the decision. The separation is unearned, and the tests are
unaffected — `retire.rs`'s `Kicks(Mutex<Vec<CpuId>>)` records through the `Kicker`
impl, not through the return value.

#### S3 — `TaskShared`'s state machine is `pub`, so the shadow can be driven without owning the task

`task.rs:321` (`transition`), `:342` (`begin_commit`), `:363` (`commit_park`),
`:376` (`cancel_commit`), `:424` (`finish_wake`), `:430`/`:434`
(`set_waiting`/`clear_waiting`), `:452` (`mark_kill`) are all `pub` on a type
reachable through a freely cloneable `Arc` from anywhere in the kernel.

The linear `Task<X>` is unforgeable. The **shadow is not**.
`shared.transition(TaskState::Ready(c), TaskState::Running(c))` is a legal edge per
`legal()` (`task.rs:219`) and would leave the word saying `Running` while the value
sits in a `RunQueue`; the next real `dispatch` then assert-fails
(`task.rs:766-770`) on a different CPU with no trace of who did it.
`claim_retire` is already `pub(crate)` (`:445`), forcing callers through
`retire::begin` — the right treatment, simply not extended.

**Established, not assumed:** grepped `kernel/`, `toyos-sched/sim/` and
`toyos-sched/loom/` for all seven — **four hits, none in `kernel/`**:
`loom/tests/loom_retire.rs:171,207`, `loom/tests/loom_ticket.rs:209`,
`sim/src/vm.rs:1234`. So this is a wider-than-necessary API, not a live misuse.

**Proposed:** `pub(crate)` on all of the above; `claim_wake` stays `pub` because
`waitq::wake_direct` is the documented public wake path. The loom package is a
separate crate (`sync.rs:4-9` explains why), so it needs the harness surface behind
a feature:

```rust
// task.rs — one gate, not seven visibility exceptions
#[cfg(feature = "protocol-port")]
impl<M> TaskShared<M> {
    /// Drive the rendezvous word directly. Harness-only: the kernel owns tasks
    /// as values and reaches the word through them.
    pub fn force_transition(&self, from: TaskState, to: TaskState) -> bool {
        self.transition(from, to)
    }
    pub fn force_mark_kill(&self) { self.mark_kill() }
}
```

`protocol-port` is the feature the crate already uses for exactly this purpose
(`cpu.rs:279-305`, `queue.rs:21-38`): broken and privileged shapes exist as
compiled, tested artefacts behind a gate the kernel does not enable. This finding
extends an existing mechanism rather than inventing one, and it puts the four
harness call sites in the same place as the other escape hatches.

**Blast radius:** `task.rs` (8 visibility changes + one gated impl), 4 harness call
sites, `kernel/` untouched.

#### S4 — The core's `!Sync` guarantee is re-established by a runtime flag at the kernel boundary

`kernel/src/sched/driver.rs:102-134`. Outside this crate, but it is where the
crate's strongest property stops:

```rust
struct SchedSlot(UnsafeCell<Option<CpuSched<KernelPayload>>>);
unsafe impl Sync for SchedSlot {}
static SCHEDS: [SchedSlot; MAX_CPUS] = ..;
static IN_PASS: [AtomicBool; MAX_CPUS] = ..;
```

`CpuSched` is `!Sync` precisely so a global array does not compile; the driver
builds one anyway and re-derives the property from `percpu::cpu_id()` indexing plus
an `IN_PASS` reentry assert. The `unsafe impl`'s SAFETY comment states the argument
and the argument holds.

**Not a defect, and the check matters.** `IN_PASS[cpu].store(false)` runs only on
the normal return from `with_cpu`, so a panic inside a pass would strand the flag —
but `scheduler::schedule_no_return` (`kernel/src/scheduler.rs:449-453`) tests
`in_schedule_self()` first and calls `halt_all_cpus()`, so the case is handled
loudly rather than wedging. Recorded because a future change to panic recovery
turns this into `specs/issues/panic-path/`'s stranded-lock class, and the one `if`
that stops it lives in a different file from the flag.

The point for the verdict: **the core's typing is airtight up to the boundary, and
the boundary is 30 lines.** That is the right place for a reader to look, not 4738.

#### S5 — A drop bomb is structurally unable to fire on the one path it exists for

`waitq.rs:326-333`:

```rust
impl<M: SchedMsg, L: LeafLock<WaitList<M>>> Drop for Registration<'_, M, L> {
    fn drop(&mut self) {
        assert!(!self.armed,
            "wait registration dropped: it must be finished once the task runs");
    }
}
```

`Registration` lives on the blocked thread's own kernel stack, inside the blocking
site's frame. `Msg::Retire` finding the task parked goes `handle_retire` →
`parked.remove` → `BlockedTask::reap` → `DeadTask` → `dispose_dead` → `finalize`
(`cpu.rs:500-519`, `task.rs:927-937`). **The kernel does not unwind**, so that frame
is never run down: `Registration::drop` never executes, neither the assert nor the
`queue.dequeue` in `finish` (`:320-323`), and the `Arc<TaskShared>` stays in the
`WaitList` forever.

`specs/issues/kernel/` files the leak and correctly names the owed intrusive
`wait_node` as the fix. **What is not recorded, and is the reusable part: the type
that was supposed to prevent it cannot fire on that path.** The guard is armed by
`Drop`; the kill path is the one path where `Drop` does not run.

**Checked across the crate — this is the only drop bomb with that exposure.**
`WaitTicket` (`waitq.rs:413`) is held by a *running* thread that always reaches
`commit`/`cancel`, and `commit` has an explicit kill arm (`:383-387`). `TaskInner`
(`task.rs:568`) and `MailboxNode` (`mailbox.rs:173`) live in the scheduler-owned
record and the `Arc<TaskShared>`, both dropped by code that runs. `PostSlot`
(`:201`) is a same-function temporary.

**Both ways, at `task.rs:927-937` (`BlockedTask::reap`), where the reaper already
holds everything it needs:**

```rust
// current — the reaper cannot unlink the waiter; it does not know which queue
pub(crate) fn reap(self, cpu: CpuId, class: WaitClass, now: Nanos) -> DeadTask<X> {
    let mut task = self.0;
    task.charge_residency(now, Residency::Blocked(class));
    let from = task.0.shared.state();
    assert!(matches!(from, TaskState::Blocked(c) | TaskState::WakeQueued(c) if c == cpu), ..);
    assert!(task.0.shared.transition(from, TaskState::Dead));
    DeadTask(task)
}
```

```rust
// proposed — the link is in the Arc the reaper already has
pub(crate) fn reap(self, cpu: CpuId, class: WaitClass, now: Nanos) -> DeadTask<X> {
    let mut task = self.0;
    task.charge_residency(now, Residency::Blocked(class));
    task.0.shared.wait_node.unlink();          // no-op if not queued
    let from = task.0.shared.state();
    assert!(matches!(from, TaskState::Blocked(c) | TaskState::WakeQueued(c) if c == cpu), ..);
    assert!(task.0.shared.transition(from, TaskState::Dead));
    DeadTask(task)
}
```

```rust
pub struct TaskShared<M> {
    // ..
    wake_node: MailboxNode<M>,
    retire_node: MailboxNode<M>,
    /// Membership in at most one wait queue — the link itself, not a flag plus
    /// a queue-owned VecDeque entry. `waiting: AtomicBool` becomes derived.
    wait_node: WaitNode<M>,
}
```

**What goes away:** `WaitList<M>`'s `VecDeque<Arc<TaskShared<M>>>` (`waitq.rs:41`)
and the `Arc` clone per registration (`:129`); `WaitQueue::dequeue`'s
`retain(|w| w.key() != key)` (`:203`), which is **O(waiters) on every wake and every
cancel**, becomes O(1); `TaskShared.waiting: AtomicBool` (`task.rs:280`) and its
three accessors (`:430-440`) become the link's own state; and the `Registration`
guard stops being load-bearing, because cleanup no longer depends on the victim's
stack being run down.

**Blast radius:** `waitq.rs`, `task.rs`, and the `unsafe` for the link belongs in
`mailbox.rs`'s existing island next to `MailboxNode` — which is where the crate
already puts exactly this code, and which the loom package already models
(`loom/tests/loom_mailbox.rs`). A new loom model for the wait link is part of the
work, not an argument against it.

**The general statement, because it will recur:** *in a kernel that does not
unwind, `Drop` is a guarantee about the paths where a value is dropped, and "killed
by another CPU" is not one of them.* Any invariant that must hold across a kill
needs its guard in an object the killer can reach — not on the victim's stack.
That criterion is checkable by inspection and it partitions guards into two piles.

#### S6 — `RunQueue::pop_next` returns a sentinel vruntime for RT tasks, and it has already caused a bug

```rust
// queue.rs:108-119, current
pub fn pop_next(&mut self) -> Option<(u64, ReadyTask<X>)> {
    if let Some(task) = self.rt.pop_front() {
        return Some((0, task));          // RT tasks have no vruntime
    }
    ..
    Some((key.0, task))                  // fair band: a real vruntime
}
```

`CpuSched::pick` (`cpu.rs:850-863`) then calls `self.env.frontier.advance(vruntime)`
on whatever came back. The `0` is not a vruntime; it is "not applicable", spelled
as a number in the same type.

**This already bit, and the citation is in the tree.** `Frontier::advance`
(`fair.rs:36-43`):

> `fetch_max` is the only correct semantic on SMP: a plain `store(vrt)` lets a CPU
> picking a low-vrt task regress the frontier another CPU has already advanced,
> **and lets RT picks (vrt=0) reset it to zero on every preemption.**

The sentinel forced a defensive `fetch_max` in an unrelated module. The mitigation
is correct; the class is open, because any future reader of `pop_next`'s first
element gets a `0` that looks like a legitimate vruntime.

**Both ways, at `cpu.rs:850-870` (`pick`):**

```rust
// current
while let Some((vruntime, task)) = self.cpu.rq.pop_next() {
    if task.shared().kill_pending() { .. continue; }
    self.env.frontier.advance(vruntime);        // 0 for RT — relies on fetch_max
    ..
}
```

```rust
// proposed
while let Some((vruntime, task)) = self.cpu.rq.pop_next() {
    if task.shared().kill_pending() { .. continue; }
    if let Some(v) = vruntime { self.env.frontier.advance(v); }
    ..
}
```

```rust
/// Fair-band virtual runtime. Not a duration, not an instant, and an RT task
/// does not have one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct VRuntime(u64);

pub fn pop_next(&mut self) -> Option<(Option<VRuntime>, ReadyTask<X>)>;
pub fn insert(&mut self, vruntime: VRuntime, task: ReadyTask<X>);
```

The `VRuntime` half matters more than the `Option` half: `fair.rs` spells vruntime,
frontier, lag magnitude **and elapsed nanoseconds** all as `u64`, and
`charge(ns: u64)` adds real nanoseconds directly to a vruntime (`fair.rs:147-155`).
`Nanos` (`hw.rs:23`) already exists for instants and is used correctly; vruntime is
the one quantity in the fairness math with no type, and it is the one that gets
mixed with a duration on every charge.

**What goes away:** the RT clause of `Frontier::advance`'s comment stops being
expressible (the SMP-regression reason survives and keeps `fetch_max`); the sentinel
`0` at `queue.rs:110`.

**Blast radius:** `queue.rs`, `fair.rs`, `cpu.rs` (5 sites), plus the simulator's
I5/I13 checkers, which read vruntime.

**Sequencing note, not an objection.** `specs/issues/kernel/` sets three entry
criteria for the per-share-FIFO redesign, and criterion 2 says a change touching
`pop_surplus`'s neighbourhood can silently collapse I13's reach. This edit is in
that neighbourhood and changes no arithmetic and no ordering — so it wants the same
A/B on `SweepResult::thread_coverage_pct` that the redesign does, even though it is
a type change. That is a verification requirement, not a reason to defer.

### Examined and deliberately not flagged (toyos-sched)

- **`WaitList`'s unbounded `VecDeque`** — one `Arc` clone per waiter, bounded by
  live thread count, which is itself uncapped (`specs/issues/isolation/`,
  `SYS_SYSINFO`). Second-order under a filed entry. S5 removes the allocation
  anyway.
- **`TaskKey(pub u64)`, `CpuId(pub u32)`, `Nanos(pub u64)`** — public tuple fields,
  so any can be minted. `TaskKey` is compared, never dereferenced, so a forged one
  matches nothing; `CpuId` is validated at `CpuHandles::get` (`cpu.rs:1111-1115`)
  and at `pack` (`task.rs:175`); `Nanos` has `after`/`since` and no `Add`, so two
  instants cannot be summed. No bug nameable and no snippet reads better.
- **`unpack`'s `panic!("corrupt task state word")`** (`task.rs:205`) — the word is
  kernel-internal and never crosses a trust boundary. Fail-fast is correct.
- **`legal()`'s transition table** (`task.rs:212-238`) — the five linear wrapper
  types already encode the edges at the *value* level; `legal()` guards the
  *shadow*, which is a CAS-updated atomic word. Session types over a CAS loop would
  add code for a second layer of the same guarantee, and the snippet does not read
  better. Genuinely the "no safety or value" case.
- **`Doorbell` / `CpuHandle::load` relaxed atomics** — documented as heuristics
  (`cpu.rs:428-430`, `:1033`) and correct as heuristics: a stale read costs an extra
  pass, never a lost wake.
- **`RunQueue::insert`'s `assert!(previous.is_none())`** (`queue.rs:100-103`) — a
  `BTreeMap` key collision would silently drop a task; it needs `insert_seq` to wrap
  at 2^64. Correct as an assert.
- **`fair.rs`'s `NonZeroU32` for `runnable_threads`** (`:62`) — "Runnable with 0
  threads" is genuinely unrepresentable. Model of the pattern.
- **`TimerApplied::new` being `pub(crate)`** (`timer.rs:124`) — forgeable inside the
  crate, one call site (`cpu.rs:907`). Making it private needs `TimerPlan::apply` to
  move into `timer.rs`, which does not obviously read better. Noted, not proposed.
- **The per-process fairness degradation and its three entry criteria** — filed in
  full (`specs/issues/kernel/`). Untouched here; S6 says so explicitly.

---

## 3. `toyos-ps2/`

334 lines, zero `unsafe`, zero `unwrap`/`expect`/`panic!`, 19 host tests (15 in
`decode.rs`, 4 in `fuzz.rs`).

The question the brief asks — *is the framer an enum with unrepresentable illegal
transitions, or an integer?* — has a clean answer: **it is an enum, in both
decoders.** `key.rs:91-98` is `State { Base, Extended, Pause(u8) }`; both tables are
`[u8; 128]` indexed by `byte & 0x7F`, so the index is in range by construction and
there is no bounds check to get wrong. `mouse.rs:50-59` is
`State { Head, MaybeReset, Body { head, count, byte1 } }`, and `MaybeReset` exists
because `0xAA` is both the reset announcement and a legal head byte — the ambiguity
is a *state*, not a flag.

### P1 — `State::Body { count: u8 }` is an integer sub-state with two legal values

`mouse.rs:58`. Only `0` and `1` are ever written (`:105`, `:116`, `:121`), and the
match tests only `count == 0` (`:120`), so a `count` of anything else reads as
"packet complete".

**Both ways, at `mouse.rs:119-130`:**

```rust
// current
State::Body { head, count, byte1 } => {
    if count == 0 {
        self.state = State::Body { head, count: 1, byte1: byte };
        return MouseOutcome::None;
    }
    self.state = State::Head;
    MouseOutcome::Packet {
        buttons: head & 0x07,
        dx: delta(head, X_SIGN, byte1),
        dy: -delta(head, Y_SIGN, byte),
    }
}
```

```rust
// proposed
State::Dx { head } => {
    self.state = State::Dy { head, dx: byte };
    MouseOutcome::None
}
State::Dy { head, dx } => {
    self.state = State::Head;
    MouseOutcome::Packet {
        buttons: head & 0x07,
        dx: delta(head, X_SIGN, dx),
        dy: -delta(head, Y_SIGN, byte),
    }
}
```

The proposed version deletes the `if`, deletes the `count` field, and names the two
states after the bytes they are waiting for — `byte1` stops being a slot that is
sometimes meaningless. The two other writers (`:105`, `:116`) become
`State::Dx { head: byte }` and `State::Dy { head: 0xAA, dx: byte }`, which is where
the current code's `count: 1, byte1: byte` needs a reader to work out what happened.

**Bug permitted today:** none — the field is private with two writers in a 145-line
file. **Class prevented:** the frame length and the state counter drifting apart.
The module doc (`:9-10`) says *"No IntelliMouse extension"*, i.e. 3-byte frames
only; the day a 4-byte frame lands, `count` becomes a real counter and the arm that
tests `== 0` becomes wrong silently. Recommended on the reading test.

**Blast radius:** `mouse.rs` only, ~10 lines. The 19 tests are behaviour tests on
`feed`, so they are unchanged.

### Examined and deliberately not flagged (toyos-ps2)

- **HID usages and PS/2 scancodes both `u8`** — `KeyOutcome::Key { usage: u8 }`
  (`key.rs:84`) feeds `kernel::keyboard::handle_key(usage: u8, pressed: bool)`.
  Written both ways and the newtype does not read better: the decoder is the only
  producer of a usage, it *consumes* the scancode and returns an enum, so there is
  no function taking a usage that a scancode can reach; and `handle_key`'s
  `held: [u64; 4]` is 256 bits indexed by `usage / 64`, so every `u8` is in range
  and there is no bounds check to delete. `Motion` (`kernel/src/mouse.rs`) is already
  an enum, so absolute-vs-relative — the confusion that would matter — is already
  unrepresentable, and `PointerSource` is already a newtype. `buttons: u8` is the
  only bare bitfield left, and PS/2 button order is HID boot-mouse order unchanged,
  so the conversion is the identity. No bug, no reading improvement.
- **The `0xAA` reset ambiguity** — `specs/issues/kernel/` files it. A property of
  the wire under controller translation, not of this code, and `key.rs:152-155`
  documents it exactly.

### What this crate does that the others should copy

The decoders are pure functions of `(state, byte)` with no allocation, no locks and
no hardware, which is what makes 19 host tests possible on logic whose defect
density the module doc calls out explicitly (`lib.rs:5-9`). `delta()`
(`mouse.rs:140-145`) carries the reasoning for why `value as i8` would be wrong,
with the failure named — *"a fast flick would reverse direction"*.

---

## 4. `toyos-ld/`

The brief's hypothesis was that this is the richest scope for untyped-index
confusion. **Half right, and the half it got wrong matters.** The *index* side is
already newtyped: `SectionIdx(usize)` (`collect.rs:13`), `ObjIdx(usize)` (`:26`),
`SymbolRef::{Global(String), Local(ObjIdx, String)}` (`:32-35`) — with the comment
saying it exists so same-named locals from different objects cannot be confused —
`RelocType` as a 34-variant enum, `SymbolDef` as an enum. A section index cannot be
passed where a symbol index is expected, because symbols are not indices at all.

The *address* side has no types whatsoever, and it is where the crate's code volume
is. `ElfLayout` (`emit_elf.rs:12-39`) has **23 `u64`s in 28 lines**. `RelocOutput`
(`reloc.rs:7-35`) is **29 lines of which 17 are `///` comments** whose entire job is
saying which kind of integer each `u64` is. That is the tell the brief predicted,
and it is measurable: **17 comment lines exist because the type does not say it.**

This is a host build tool, so nothing here is a security finding — the input is
object files the build system just produced. The cost of a defect is a wrong
binary, which is worse than a crash: `specs/issues/isolation/`'s standing
judgement, *"a refusal is a limitation, a wrong answer that looks right is a
correctness defect."*

### L1 — `RelocOutput`'s eleven parallel vectors: −158 lines of duplicated emit loops

`reloc.rs:7-35` — 11 `pub(crate)` fields, ten of them `Vec<(u64, i64)>` or
`Vec<(u64, String)>`, each with a comment of the form *"(GOT slot vaddr, addend)"*.
Except `tpoff32s` (`:23-25`), which is *"(vaddr of imm32, addend)"* — a different
kind of address in an identically typed field, distinguished only by the comment.

The consumers are three **near-identical** blocks in `emit_elf.rs`, measured:

| block | lines | loops |
|---|---|---|
| `:1223-1298` (shared, before dynamic) | **76** | 9 |
| `:1309-1349` (`rela_before_dynamic`) | **41** | 5 |
| `:1395-1435` (PIE) | **41** | 5 |

**158 lines**, and blocks 2 and 3 are byte-identical to each other.

**Both ways, at `emit_elf.rs:1223-1298`** (showing 2 of the 9 loops; the other 7
differ only in `r_type` and which field is read):

```rust
// current
for &(offset, addend) in &relocs.relatives {
    w.write_relocation(true, &Rel {
        r_offset: offset, r_sym: 0,
        r_type: elf::R_X86_64_RELATIVE, r_addend: addend,
    });
}
for (got_vaddr, sym_name) in &relocs.glob_dats {
    let sym_idx = sym_to_writer_idx[sym_name];
    w.write_relocation(true, &Rel {
        r_offset: *got_vaddr, r_sym: sym_idx.0,
        r_type: elf::R_X86_64_GLOB_DAT, r_addend: 0,
    });
}
// .. seven more of these, then the whole thing twice more with five of them
```

```rust
// proposed — the same four lines replace all three blocks
for r in relocs.rela_entries() {
    let (r_sym, r_type, r_addend) = r.rela(&sym_to_writer_idx);
    w.write_relocation(true, &Rel { r_offset: r.at().0, r_sym, r_type, r_addend });
}
```

```rust
// reloc.rs — the data as it actually is
pub(crate) enum DynReloc {
    Relative    { at: Vaddr, addend: i64 },
    GlobDat     { at: Vaddr, symbol: String },
    /// Written directly into the image, not as RELA: a RELATIVE here would
    /// wrongly add the load base.
    TpoffFill   { at: Vaddr, value: i64 },
    Tpoff64     { at: Vaddr, addend: i64 },
    Tpoff64Sym  { at: Vaddr, symbol: String },
    /// `at` is the site of the imm32, not a GOT slot.
    Tpoff32     { at: Vaddr, addend: i64 },
    DtpMod64    { at: Vaddr, addend: i64 },
    DtpOff64    { at: Vaddr, addend: i64 },
    DtpMod64Sym { at: Vaddr, symbol: String },
    DtpOff64Sym { at: Vaddr, symbol: String },
}

impl DynReloc {
    fn at(&self) -> Vaddr { .. }
    fn symbol(&self) -> Option<&str> { .. }
    fn rela(&self, syms: &HashMap<String, SymbolIndex>) -> (u32, u32, i64) {
        match self {
            DynReloc::Relative { addend, .. }    => (0, elf::R_X86_64_RELATIVE, *addend),
            DynReloc::GlobDat { symbol, .. }     => (syms[symbol].0, elf::R_X86_64_GLOB_DAT, 0),
            DynReloc::Tpoff64 { addend, .. }     => (0, elf::R_X86_64_TPOFF64, *addend),
            DynReloc::Tpoff64Sym { symbol, .. }  => (syms[symbol].0, elf::R_X86_64_TPOFF64, 0),
            DynReloc::Tpoff32 { addend, .. }     => (0, elf::R_X86_64_TPOFF32, *addend),
            DynReloc::DtpMod64 { addend, .. }    => (0, elf::R_X86_64_DTPMOD64, *addend),
            DynReloc::DtpOff64 { addend, .. }    => (0, elf::R_X86_64_DTPOFF64, *addend),
            DynReloc::DtpMod64Sym { symbol, .. } => (syms[symbol].0, elf::R_X86_64_DTPMOD64, 0),
            DynReloc::DtpOff64Sym { symbol, .. } => (syms[symbol].0, elf::R_X86_64_DTPOFF64, 0),
            DynReloc::TpoffFill { .. }           => unreachable!("written directly, not as RELA"),
        }
    }
}
```

**What goes away, counted:**

- **158** lines of emit loops → **12** (three 4-line loops), plus **14** for `rela`
  and **12** for the enum: net **−132**.
- `RelocOutput`'s **29** lines (17 of them comments naming integer kinds) → **1**
  field. The four comments carrying real information — `TpoffFill`'s "not RELATIVE",
  `Tpoff32`'s "not a GOT slot" — move onto the variants they describe, where they
  cannot drift.
- **7** local accumulator `Vec`s in `apply_relocs` (`reloc.rs:249-255`) → **1**;
  the 11-field construction at `:573-584` → **1** line.
- The two four-way `.chain()` groups (`emit_elf.rs:771-774`, `:806-809`, 8 lines) →
  `.filter_map(DynReloc::symbol)`, 2 lines.
- `rela_count`'s 6-line nine-term sum (`:920-925`) → one `.count()`.

**Total: roughly −170 lines, and every "which kind of integer is this" comment in
the file.** Adding a relocation kind becomes a non-exhaustive-match compile error
instead of ten places to remember.

**Refuted on inspection, and the reason is the finding.** Blocks 2 and 3 emit only 5
of the 9 kinds — they omit the four DTV ones — while `rela_count` (`:921-924`) sums
**9**. That reads like a reserve/write mismatch. It is not: all four DTV push sites
(`reloc.rs:298,299,305,306,360,361`) are inside `if params.is_shared`, and blocks 2
and 3 run under `rela_before_dynamic = !is_shared && ..` (`:963`) and
`!is_shared && !rela_before_dynamic` respectively, so those vectors are provably
empty there. **The code is correct today because of an invariant that lives in
`reloc.rs` and is depended on in `emit_elf.rs`, with no comment in either file.**
One shared loop makes the question not arise.

**Blast radius:** `reloc.rs` (16 push sites, the struct, the construction),
`emit_elf.rs` (three blocks, two chain groups, `rela_count`). Self-contained; does
not depend on L2 or L3.

### L2 — `InputSection.vaddr` means three different things by output format, and one relocation engine consumes all three

`collect.rs:84`: `pub(crate) vaddr: Option<u64>`.

- `layout_elf` sets an **absolute virtual address** (`BASE_VADDR`, or
  `0xFFFF800000000000` for the kernel).
- `layout_macho` sets an absolute vmaddr.
- `layout_pe` sets a **relative virtual address**. `emit_pe.rs:28-33`:
  `let text_rva = PE_SECTION_ALIGNMENT; let mut cursor = text_rva as u64; .. sec.vaddr = Some(cursor);`
  — no image base is ever added. The struct comment (`:22-23`) says so.

`resolve_symbol` (`reloc.rs:41-84`) and `apply_one_reloc_x86` (`:116-...`) are
shared by all three. `apply_relocs_pe` (`:1000-1035`) calls the same helper and
compensates afterwards by pushing `reloc_vaddr as u32` into a PE base-relocation
table, gated on the helper's `Ok(true)` "this was absolute" return.

**Bug permitted.** The overflow checks are unit-blind: `check_i32` (`:87-97`) and
`check_u32` (`:99-110`) bound the *written* value, so `R_X86_64_32S` against a
kernel symbol at `0xFFFF800000000000` correctly errors in ELF mode and silently fits
in PE mode, where the same computation yields a small RVA. And "which values need a
base fixup" is carried by a `bool` return convention across a shared function
rather than by the type of the address.

**Both ways, at `reloc.rs:116-134` — and note the two adjacent same-typed
parameters:**

```rust
// current
fn apply_one_reloc_x86(
    data: &mut [u8],
    reloc: &InputReloc,
    sym_addr: u64,
    reloc_vaddr: u64,
    got: &HashMap<SymbolRef, u64>,
    dyn_got: &HashMap<SymbolRef, u64>,
) -> Result<bool, LinkError> {
    ..
    RelocType::X86Pc32 | RelocType::X86Plt32 => {
        let value = sym_addr as i64 + reloc.addend - reloc_vaddr as i64;
```

`sym_addr` and `reloc_vaddr` are swappable at all four call sites (`:483`, `:505`,
`:923`, `:1032`) with no compile error. A PC-relative relocation is literally
`sym − site`; reversing it is a sign flip that produces a linking binary and a
crashing program.

```rust
// proposed
/// Where a symbol resolved to, in the output image's coordinate system.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct SymAddr(u64);
/// Where the relocation is being applied, same coordinate system.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct SiteAddr(u64);

fn apply_one_reloc_x86(
    data: &mut [u8],
    reloc: &InputReloc,
    sym: SymAddr,
    site: SiteAddr,
    got: &HashMap<SymbolRef, u64>,
    dyn_got: &HashMap<SymbolRef, u64>,
) -> Result<bool, LinkError> {
    ..
    RelocType::X86Pc32 | RelocType::X86Plt32 => {
        let value = sym.0 as i64 + reloc.addend - site.0 as i64;
```

The arithmetic body gains `.0`; the signature stops admitting the swap. `Vaddr`
serves both `InputSection.vaddr` and `ElfLayout`'s address fields, and
`resolve_symbol -> Option<SymAddr>` makes the producer say what it produces.

**What goes away:** the swap; the four-way ambiguity of `vaddr`; and — under L3's
`file_offset` — the eight open-coded `(vaddr − base)` conversions. `ElfLayout`'s
**23** `u64`s become a mix of `Vaddr` and `u64` sizes, so "which of these is an
address" stops being a question the reader has to answer from the field name.

**Honest accounting of what it costs.** Newtypes here *add* `.0` at arithmetic
sites; this is not a line-count win the way L1 is. It is recommended on the bug
test, not the reading test — with the reading test carried by the signature, which
is where the mistake is made.

**Blast radius:** `collect.rs` (one field), `reloc.rs` (signatures + `.0` at the
arithmetic sites), `emit_elf.rs`, `emit_pe.rs`, `emit_macho.rs`. Roughly 200
mechanical edits. **There is no `toyos-ld/tests` directory** (verified), so
verification is `cargo run -- --build-only` producing byte-identical bootloader,
kernel and userland binaries — which is a strong check, and is the reason to land
L3 first: it exercises the same harness at a tenth the size.

### L3 — `(vaddr − base) as usize` is used as a file offset at eight sites

`emit_elf.rs:667`, `:1061`, `:1073`, `:1119`, `:1123`, `:1127`, `:1146`, `:1157`,
`:1507`, `:1518`, `:1543`. Five of those feed `Writer::pad_until`, which writes zero
padding up to the offset.

The identity holds today. `write_sections_data` (`:657-671`) filters NOBITS
sections and empty data before applying it, so the one section kind that has a
vaddr and no file bytes never reaches the subtraction — which is why the filter at
`:661` is load-bearing and looks like a tidiness check.

**Both ways, at `emit_elf.rs:657-671`:**

```rust
// current
fn write_sections_data(w: &mut Writer, sections: &[InputSection], base: u64,
                       vaddr_min: u64, vaddr_max: u64) {
    let mut indices: Vec<usize> = (0..sections.len())
        .filter(|&i| {
            let Some(vaddr) = sections[i].vaddr else { return false };
            !sections[i].data.is_empty() && !sections[i].kind.is_nobits()
                && vaddr >= vaddr_min && vaddr < vaddr_max
        })
        .collect();
    indices.sort_by_key(|&i| sections[i].vaddr.unwrap());
    for i in indices {
        let file_off = (sections[i].vaddr.unwrap() - base) as usize;
        w.pad_until(file_off);
        w.write(&sections[i].data);
    }
}
```

```rust
// proposed — the identity lives in one method that can say "no"
impl ElfLayout {
    /// The file offset a vaddr maps to. `None` for a section with no file
    /// bytes: layout_elf places every PROGBITS section contiguously from
    /// `base`, and NOBITS sections are not in the file at all.
    fn file_offset(&self, sec: &InputSection) -> Option<FileOff> { .. }
}

fn write_sections_data(w: &mut Writer, layout: &ElfLayout, sections: &Sections,
                       range: Range<Vaddr>) {
    let mut placed: Vec<(FileOff, &InputSection)> = sections.iter()
        .filter(|s| s.vaddr.is_some_and(|v| range.contains(&v)))
        .filter_map(|s| Some((layout.file_offset(s)?, s)))
        .collect();
    placed.sort_by_key(|&(off, _)| off);
    for (off, sec) in placed {
        w.pad_until(off.0 as usize);
        w.write(&sec.data);
    }
}
```

The proposed version has one `filter_map` where the current has a filter that must
remember `is_nobits` and `is_empty`, and it has **no `.unwrap()`** where the current
has two. The rule is stated once, in the method, instead of once per caller who
remembers it.

**Bug permitted:** a future section kind with a vaddr and no file bytes, or a layout
that leaves a gap, turns `pad_until` into megabytes of zero padding or a `usize`
underflow — visible as a corrupt output binary, not as an error. The mitigation
today is a filter in one function that any new emit path must remember to
replicate; there are already eight sites that do not have it.

**What goes away:** 2 `.unwrap()`s and the remembered filter in
`write_sections_data`; 8 open-coded subtractions collapse to one method.

**Blast radius:** `emit_elf.rs`, ~30 lines. Independent of L1 and L2, and the
cheapest way to prove the byte-identical-output harness before L2.

### L4 — `SectionIdx` adds a typed lane without closing the untyped one

`collect.rs:16-23` implements `Index<SectionIdx>`/`IndexMut<SectionIdx>` for
`Vec<InputSection>`. `Vec<T>` also implements `Index<usize>`, and both apply — so
`state.sections[7]` compiles exactly as well as `state.sections[SectionIdx(7)]`.
Worse, the impls are on `Vec`, not on slices, so any function taking
`&[InputSection]` loses the typed index entirely. `write_sections_data`
(`emit_elf.rs:657`) is exactly that and indexes with a bare `usize` at `:660`,
`:661`, `:665`, `:667`, `:669`.

```rust
// proposed
pub(crate) struct Sections(Vec<InputSection>);

impl Index<SectionIdx> for Sections { type Output = InputSection; .. }
impl IndexMut<SectionIdx> for Sections { .. }
// and no Index<usize> impl at all
```

`LinkState.sections: Sections`; functions taking `&[InputSection]` take `&Sections`.

**Bug permitted:** using an index from a different collection — `buckets.rx` is a
`Vec<SectionIdx>`, `state.relocs` is indexed by position, and the `object` crate's
`SectionIndex` is a third space — where a section index is expected. The newtype
documents intent; it does not exclude the mistake, which is the whole difference
between "unrepresentable" and "checked by review".

L3's rewrite of `write_sections_data` already takes `&Sections`, so the two land
naturally together.

**Blast radius:** ~40 index sites across five files, plus `Sections` needing
`iter`/`len`/`push`/`enumerate` passthroughs.

### L5 — `tpoff(sym_addr: u64, tls_start: u64, tls_memsz: u64)`

`reloc.rs:86-88`. Three same-typed parameters computing
`sym − (tls_start + tls_memsz)`. Swapping the second and third compiles and produces
a TLS layout wrong by the difference between a segment's start and its size — which
appears as userland reading the wrong thread-local, not as a link error. Two call
sites in `emit_elf.rs`, one in `reloc.rs`.

```rust
// proposed — the two that can be swapped stop being the same type
pub(crate) fn tpoff(sym: SymAddr, tls: TlsBlock) -> i64;
pub(crate) struct TlsBlock { pub start: Vaddr, pub memsz: u64 }
```

`ElfLayout` already has `tls_start` and `tls_memsz` adjacent (`emit_elf.rs:19-21`),
so the struct is a field grouping it already implies. Named separately from L2
because it is the highest consequence-per-character signature in the crate.

### Examined and deliberately not flagged (toyos-ld)

- **`SymbolRef::Local(ObjIdx, String)`** (`collect.rs:32-35`) — the comment says it
  exists so same-named locals from different objects (`.str.63`) cannot be confused.
  The finding this audit would otherwise have made, already made and already fixed.
- **`RelocType`, `SectionKind`, `Arch`, `SymbolDef`, `ElfEmitMode`** — proper enums
  with exhaustive matches; `classify_sections`'s `unreachable!()` (`lib.rs:700-704`)
  names the impossible arms rather than using a wildcard.
- **The `Collected → LaidOut<L> → Vec<u8>` typestate** (`lib.rs:44-59`) — each stage
  consumes the previous, and `LaidOut<ElfLayout>` / `LaidOut<PeLayout>` /
  `LaidOut<MachOLayout>` make "emit PE from an ELF layout" not typecheck. The same
  pattern as `bcachefs`'s `Mounted<IO, Mode>`.
- **`resolve_symbol`'s four copies of `unwrap_or_else(|| panic!("symbol {name:?} ..
  has no vaddr"))`** (`reloc.rs:52-80`) — duplication, not a type problem, and it
  collapses to one site under L2's `-> Option<SymAddr>` with a single `?`. Worth
  doing as part of L2; not a finding on its own.
- **`read_uleb128`/`read_sleb128`** (`emit_elf.rs:51-79`) — index `data[offset]`
  with no bounds check on `.eh_frame` bytes from an input object; a malformed CIE
  panics the linker. Host tool, our own compiler's output, and a panic is a failed
  build. No snippet reads better with a `Result` threaded through the CIE parser.
- **`align_up(addr, align)`** (`lib.rs:665-667`) — `(addr + align - 1) & !(align - 1)`
  overflows near `u64::MAX`; reachable only from a caller-chosen `base_addr`, and
  `link_static`'s kernel base is a constant.

---

## 5. `toyos-cc/`

9964 lines, 241 `unwrap`/`expect`/`panic!`, 1 `#[must_use]`, no `unsafe`, no
`tests/` directory (verified — it is exercised through `toyos-build`'s
dev-dependency and by compiling doomgeneric and tinycc).

CLAUDE.md says this crate is *not meant to grow*. That is recorded here as context
for sequencing. It is **not** used below as a reason to reject anything: the one
change proposed makes the code smaller and deletes a representable-but-meaningless
state.

Its core representations are already Rust-shaped: `CType` is a 20-variant enum
(`types.rs:15-33`), `Signedness` is an enum rather than a `bool` (`:3-7`), the AST
is enums throughout, `StorageClass`/`TypeSpec`/`DeclSpecifier` are enums. There is
no integer state machine, and **zero `static mut` in any of the five crates**
(grepped).

### C1 — `CType::Function(Box<CType>, Vec<ParamType>, bool, bool)`

`types.rs:29` — the only `bool, bool` in the crate (grepped: one hit):

```rust
// current
Function(Box<CType>, Vec<ParamType>, bool, bool), // return type, params, variadic, unspecified_params
```

```rust
// proposed
Function(Box<CType>, Vec<ParamType>, FnParams),

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FnParams {
    /// `f(int, ...)`
    Variadic,
    /// `f(int)`
    Fixed,
    /// `f()` — no prototype; parameters unspecified.
    Unspecified,
}
```

Three states instead of four, and the fourth the two bools admit —
`variadic && unspecified`, which C has no syntax for — stops existing. The trailing
comment goes away because the variant names say it.

**Bug permitted:** swapping the two at a construction site compiles and produces a
function type that is variadic when it should be unspecified-parameter, or the
reverse. In C that is the difference between `printf`-family argument passing and
K&R `f()`, i.e. a wrong `%al` for the SysV variadic ABI — which is exactly what
`toyos-ld`'s `create_x86_64_stubs` (`toyos-ld/src/lib.rs:626-640`) emits
`mov $8, %al` to paper over.

**Call site, `codegen/expr.rs:1122`:**

```rust
// current
CType::Function(ret, params, v, _) => (ret.as_ref().clone(), params.clone(), *v),
```

```rust
// proposed
CType::Function(ret, params, p) => (ret.as_ref().clone(), params.clone(), p.is_variadic()),
```

34 `CType::Function(` sites in total; most use `..`, and three destructure
positionally (`codegen/expr.rs:1122`, `:1125`, `codegen/stmt.rs:807`) — each of
which currently carries a `_` for the bool it does not want.

**Blast radius:** `types.rs` plus the three positional destructures.

### Examined and deliberately not flagged (toyos-cc)

- **The 241 panic sites** — written both ways in outline and the `Result` version is
  strictly larger: a diagnostic type threaded through 20 modules of parser and
  codegen, with `?` at every one of 241 sites and an error path at every caller. It
  adds code and does not read better. A compiler that dies on malformed C loses a
  build, and the inputs are two known programs plus tinycc's own sources. Rejected
  on the reading and size tests, not on effort.
- **`FieldInfo { byte_offset: usize, bit_offset: u32, bit_width: Option<u32> }`**
  (`types.rs:60-66`) — the textbook `ByteOffset`/`BitOffset` newtype case. Written
  both ways: the two already have *different* Rust types (`usize` vs `u32`), the
  struct has a single consumer (`codegen/bitfield.rs`, 121 lines), and
  `bit_width: Option<u32>` already distinguishes bitfield from non-bitfield
  honestly. The newtype version is longer and reads the same. No bug nameable.
- **`CType::Array(Box<CType>, Option<usize>)`** — `None` for an incomplete array
  type is `Option` used correctly, not a sentinel.
- **`EnumDef { variants: Vec<(String, i64)> }`** — a bare tuple where a named struct
  would read better, marginally. Not worth its own entry.

---

## Summary

| # | crate | finding | permits a bug? | code delta | class already bit? |
|---|---|---|---|---|---|
| B1 | bcachefs | six `value[..8]` panics on interior nodes from disk | yes — kernel panic, VFS lock stranded | −6 decodes, −11 params, −8 branches, −7 decrements, −1 dead fn | yes — `bccab15`, same file, write side only |
| B2 | bcachefs | `root_level` from disk drives unbounded recursion | yes — kernel stack overflow | shared with B1 | yes — `specs/issues/isolation/` |
| B3 | bcachefs | partial allocation read as complete; write lands outside its extent | **yes — live data corruption on `/home`** | −1 `unreachable!`, 3 callers converge | yes — `specs/issues/isolation/`, ignored failure return |
| B4 | bcachefs | `with_capacity` from a `u16` on disk, 387× the physical max | yes — kernel panic | +3 | yes — `specs/issues/isolation/` |
| B5 | bcachefs | superblock fields never validated against the device | yes — arbitrary metadata writes | 24 `BlockNum::new` split two ways | yes — `specs/issues/isolation/` |
| B6 | bcachefs | `read_extents` sizes from on-disk file size (not a live kernel path) | latent | +2 | — |
| B7 | bcachefs | `to_byte_offset` unchecked multiply | latent | +3, or 0 under B5 | — |
| S1 | toyos-sched | invariant T is a type on the idle path, convention on the run path | not today; ledger claims CT | +2 variants | B5's own observed bug |
| S2 | toyos-sched | kick obligation is a `must_use` lint | not today | **−25**, `Kick` enum deleted | — |
| S3 | toyos-sched | `TaskShared`'s state machine is `pub` | no live misuse | +1 gated impl | — |
| S4 | toyos-sched | `!Sync` re-derived by a runtime flag at the boundary | no — correct today | 0 | adjacent to `specs/issues/panic-path/` |
| S5 | toyos-sched | the wait-registration drop bomb cannot fire on the kill path | **yes — the filed leak, plus O(n) dequeue** | −`VecDeque`, −`AtomicBool`+3 accessors, O(n)→O(1) | yes — `specs/issues/kernel/` files the leak, not the cause |
| S6 | toyos-sched | sentinel vruntime `0` for RT tasks | yes — cited in `Frontier::advance`'s comment | ~0 | yes, in-tree citation |
| L1 | toyos-ld | eleven parallel `Vec<(u64, ..)>` and three duplicated emit blocks | latent; adding a kind is silently incomplete | **−170, and all 17 which-integer comments** | — |
| L2 | toyos-ld | `vaddr` means VA, vmaddr or RVA by format; shared reloc engine | yes — unit-blind overflow checks; two swappable params | +`.0` at arithmetic sites | — |
| L3 | toyos-ld | `(vaddr − base) as usize` as a file offset, eight sites | latent — corrupt output, not an error | −2 `unwrap`, 8 sites → 1 method | — |
| L4 | toyos-ld | `SectionIdx` adds a lane without closing `Index<usize>` | yes — cross-collection index | ~0 | — |
| L5 | toyos-ld | `tpoff(u64, u64, u64)` | yes — silent wrong TLS | −1 param | — |
| C1 | toyos-cc | `Function(.., bool, bool)` | yes — wrong variadic ABI | −1 state, −1 comment | — |

**Suggested order.** Sequencing only; nothing here is deprioritised for size.

1. **B4, B1, B2** — a crafted disk should not be able to panic the kernel, and B1/B2
   are one edit.
2. **B3** — the only live data-corruption path in the audit.
3. **B5** — makes 1 and 2 structural rather than pointwise.
4. **L1** — largest deletion in the audit, self-contained, and it exercises the
   byte-identical-output check that L2 needs.
5. **L3, L5, C1** — small, independent, each deletes something.
6. **L2 + L4** — together, once L1 and L3 have proved the verification harness.
7. **S5** — already owed by the scheduler spec; needs a loom model for the new link.
8. **S2, S1, S6, S3** — S6 wants the `thread_coverage_pct` A/B that
   `specs/issues/kernel/` criterion 2 demands of anything near `pop_surplus`.
