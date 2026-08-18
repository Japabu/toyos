---
status: open
kind: defect
opened: 2026-08-18
---

# The sites that still carry a boundary-crossing number in a plain integer

`toyos-untrusted`'s `Untrusted<T>` exists and the virtqueue used ring uses it:
`Virtqueue::used_ring_id` and `::used_ring_len` (`kernel/src/drivers/virtio.rs`)
are the only reads of a used ring in the kernel and hand back `Untrusted<u32>`,
so `Virtqueue::parse_used` is the only path to an index or a length and it names
its bound at both. The type has no arithmetic, no `Deref`, no `From`, no cast
and no accessor; the four `compile_fail` doctests on `Untrusted` are what that
sentence means, and `src/sourcegate.rs` bans the one shape typing cannot stop
(`at_most(u64::MAX)` and its four siblings).

**The rest of the class has not adopted it.** Until a site does, its bound is a
thing an author has to remember rather than a thing the compiler asks for. Each
of these is mechanical — find the read, wrap it there, name the bound at the use
— and none needs a design decision. **Each is cited by file and enclosing
symbol**, so whoever takes this does not have to re-derive which value is meant.

## The sites, hardest consequence first

- **`kernel/src/arch/syscall.rs`, the dispatch itself — partly done.**
  `SYS_SET_THREAD_NAME`'s `a2` was the named pattern: it `min`'d against
  `THREAD_NAME_LEN` and silently set the truncated prefix of a name too long
  to fit, the one clamp in the file where the rest already refuses. Converted:
  `a2` is now `Untrusted::new(a2).at_most(THREAD_NAME_LEN)`, refused with
  `InvalidArgument` rather than truncated. Behaviour change, gated by
  `tests/toyos-rust-tests/src/bin/abuse_thread_name.rs`
  (`test_rs_abuse_thread_name`), which reads the name back through
  `SYS_SYSINFO` and checks a refused call left it exactly as it was rather
  than silently truncated — not through `toyos_abi::syscall::set_thread_name`'s
  own return value, which it has always thrown away and which this pass left
  alone: touching `toyos-abi/src` claims the shared sysroot
  (`src/CLAUDE.md`'s buildlock section — "a doc-comment change... costs a
  sysroot claim exactly like a layout change"), and CLAUDE.md's own rule is
  that an ABI change lands on its own PR first. `set_thread_name`'s doc
  comment ("up to 28 bytes, truncated") is now stale and is that PR's to fix;
  nothing in the tree calls it today.
  **Still open**: the file's other ~31 `as usize` casts. The ones that size an
  allocation before a copy — `SYS_SPAWN`'s `endow_count`, `labels_len` and
  `slot_map_count`, `SYS_NAMESPACE_BUILD`'s `keep_n`/`add_n`/`names_len`,
  `SYS_HANDLE_SEND`/`SYS_HANDLE_RECV`'s count — are already named refusals
  against `MAX_ENDOWMENTS`/`MAX_LABELS_LEN`/`MAX_NAMESPACE_ENTRIES`/
  `MAX_TRANSFER_HANDLES` (`env_len` against `MAX_USER_STR` likewise, and is
  gated by `abuse_spawn_argv.rs`), so converting them is consistency rather
  than a behaviour fix — mechanical, per the original note, but left here
  because most have no dedicated over-bound test today and this pass did not
  want to touch the syscall dispatch's other paths without one.

- **`kernel/src/drivers/xhci/mod.rs`, the event ring — done, with one
  exception.** `Layout::device`'s hand-rolled `(slot_id as usize).checked_sub(1)?`
  is now `Untrusted::new(slot_id).map(|v| v.wrapping_sub(1)).index(self.dev_blocks)`
  — `wrapping_sub` is sound here only because `index` is the exit (slot 0 wraps
  to `u8::MAX`, never `< dev_blocks`, so it is refused by the same comparison
  as every slot past the pool). Behaviour-preserving: same `None` for the same
  inputs, and `xhci_many_devices` (`log.matches("beyond the pool")`) already
  gates the refusal path.
  **The `0x1F` scratchpad masking near `Layout::new` resisted the type.** It
  extracts two 5-bit HCSPARAMS2 fields — a fixed-width hardware bitfield, not a
  value with an independently-known bound to compare against. The "bound" and
  the mask are the same operation, so the only available exit would be
  `.at_most(0x1F)` immediately after a `.map()` that had already produced a
  value `<= 0x1F` by construction: a check whose `Err` arm is unreachable by
  construction, which is worse than the plain mask it would replace. Left as a
  mask, as `& 0x1F` extracting a spec-defined bit field is throughout this
  driver and the rest of the tree.

- **`kernel/src/drivers/nvme.rs`, the completion queue — converted.**
  `wait_completion` now takes the `cid` this driver put in the command
  (`(cmd.cdw0 >> 16) as u16`, read back out of the command it just submitted,
  not trusted again) and answers `Result<u16, Refused>`:
  `Untrusted::new(cq.cid).exactly(expected)`. `submit_and_wait` and all four
  callers (`admin`, `identify_namespace` through it, `read_sectors`,
  `write_sectors`) now match on the result and log-and-fail exactly as they
  already did for a bad status word. `Untrusted::exactly`'s own behaviour is
  covered by `toyos-untrusted`'s host test suite
  (`exactly_refuses_by_naming_both_numbers`).
  **No dedicated boot-level gate** — unlike the virtqueue used ring, this
  queue has no actuator today, and building one (`#[cfg(feature =
  "boot-actuators")]` writer + self-test + `actuator.rs` entry +
  `MACHINE_TESTS` registration) means a `tests/test-durations` entry, which
  this session watched block PR #118's own merge on exactly that mechanism
  (`ci/durations`: "committed UNMEASURED profile marker(s) are provisional and
  may not land"). Given the issue's own framing — "sound today only because
  every submission is synchronous, a property of the caller" — this is
  hardening against a future asynchronous queue rather than a live defect, so
  the landing risk was judged not worth it this pass. An
  actuator + self-test mirroring `drivers/virtio.rs`'s `used_selftest` is the
  future work if the owner wants it exercised on every boot.

- **`kernel/src/drivers/acpi.rs` — already fixed, not by this type.** The two
  underflowing subtractions F5 named are gone: `Table::open` (added since the
  assessment) validates a table's declared length against its floor and a
  `MAX_TABLE_LEN` ceiling and its checksum before anything reads out of it, so
  `xsdt.len - size_of::<SdtHeader>()` and `madt.len - size_of::<Madt>()` are
  now total by construction — the module doc says so in the past tense
  ("This is the second underflow that was here"). `TableError` is already a
  named `Result` refusal, exactly what `Untrusted` would add. Applying
  `Untrusted` on top would be ceremony over an already-safe abstraction, so
  nothing changed here. No longer belongs on this list.

- **`bcachefs/src/btree.rs`'s `collect_all`.** The residual
  `untrusted-input-panics` records. **This one is not a wrapper fix**: it
  materialises every entry in the tree before anything counts them, so what it
  needs is a count primitive that lets `BcacheFsAdapter::list` refuse *before* it
  allocates, the way `TmpFs` already does. `/home` is writable by userland, so it
  is a live path. It stays the bcachefs owner's.

## What this type does not answer

Recorded so nobody tries to make it. Two open entries in this area are **not**
this class, and wrapping something would touch neither:

- **`specs/issues/isolation/netd-trusts-ring-closed-flags.md`** — a *predicate* a
  peer writes, not an index. `RingHeader::flags` lives in the page `SYS_PIPE_MAP`
  maps writable, and netd reads `is_reader_closed`/`is_writer_closed` as facts
  about its peer. There is no bound to compare a bit against. The fix is to stop
  reading a publication as a channel: the kernel's own `readers`/`writers` counts
  are the side that knows, and they already surface as EOF on a read and
  `BrokenPipe` on a write.
- **`specs/issues/isolation/netd-writable-virtqueue.md`** — a *mapping*, not a
  value. `virtio_net::init` registers the whole 2 MiB `DmaPool` page as one
  shared region, so the RX descriptor table at offset 0 and the TX virtqueue at
  `0x3000` are inside what netd maps writable. No read-side type helps while
  netd can rewrite the descriptors the device DMAs into. The fix is the one
  `virtio_sound::init` demonstrates one file over: two pools, with the descriptor
  tables in the one that is never registered. `NicInfo` addresses everything by
  offset already, so it needs no ABI change.

Both are still open in their own files, and neither is blocked on anything here.

Also untouched and deliberately so:
**`specs/issues/isolation/kernelslice-over-user-memory.md`** — its own text says
M2 (#159) closes the aliasing, and that converting the borrow first would
describe a hazard M2 removes.

## What this type is not about

It carries no *time* and no *rate*, and nothing in it reasons about how long
anything takes. The TCG-versus-KVM p90 divergence PR #119 measured (4–8× in p90,
20× in max for scheduler pass cost on the dev host) does not reach any bound
here: every one is a byte count, a table length or a register encoding, and each
is compared against a number the driver itself published in the same function.
Recorded so that nobody re-checks it.
