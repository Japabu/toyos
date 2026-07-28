# ToyOS Memory Ownership — Concept

> Status: **concept**, not yet a staged spec. Written 2026-07-28. Every empirical
> claim below was tested, not recalled; the experiments are named inline so they
> can be re-run. Open questions are marked **OPEN** and are genuinely open.

## 1. The two ideas, and why they are one

The request was two things:

1. **Justified programming** — every kernel allocation must declare its purpose
   and which part of the system it belongs to.
2. **Process ownership** — a process owns its memory; when it dies, its
   allocations go with it.

These are the same mechanism seen twice. Both say: *every allocation names its
owner*. An owner is either a **kernel subsystem** (static, immortal, known at
compile time) or a **process** (dynamic, mortal, known at runtime). Justification
is what the name is *for*; ownership is what the name *does* when it dies.

Making that one mechanism is the whole design. Two parallel systems — one for
attribution, one for lifetime — would drift, and the drift is where leaks live.

## 2. The bug class this exists to kill

Not hypothetical. A full survey of the kernel's allocation and teardown paths
(2026-07-28) found the following, all cited to code:

**`try_recover_from_panic` never frees anything.**
`kernel/src/arch/idt/exceptions.rs:298` poisons the tid, zombifies, and
reschedules. It never calls `teardown_resources` or `fd::close_all`. It is
reached from the `#[panic_handler]` in syscall context
(`kernel/src/main.rs:139`), from ring-0 faults attributed to user code
(`exceptions.rs:287`), and — because there is **no `#[alloc_error_handler]`
anywhere in the workspace** — from *any heap OOM in syscall context*.

Because `Descriptor` has no `Drop` impl (`kernel/src/fd.rs:15-80`), dropping the
fd table is not the same as closing it. A process that dies this way
**permanently** leaks:

| Leaked | Why | Consequence |
|---|---|---|
| `IoUringInstance` + its 2 MiB region | `io_uring::destroy` only called from `fd::close`/`close_all` (`fd.rs:258,286`) | memory gone; the ring stays reachable from watcher lists, and `post_cqe` **asserts** on CQ overflow (`io_uring.rs:185`) → later kernel panic |
| Listener name registration | `listener::remove` only from `fd::close`/`close_all` (`fd.rs:255,283`) | **no process can ever `listen` on that name again** (`listener.rs:71`) |
| `file_cache` refcount per open file | `file_cache::release` only from `fd::close*` (`fd.rs:245,274`) | file permanently non-evictable |
| Device claim (kbd/mouse/fb/nic/audio) | `device::release_descriptor` only from `fd::close*` (`fd.rs:252,280`) | device owned by a dead pid **forever** |
| Shared regions **and the whole address space** | `shared_memory::cleanup_process` only from `teardown_bookkeeping` (`process.rs:824`); `SharedRegion.mapped_in` holds a *strong* `PageTables` Arc (`shared_memory.rs:56`) | every 2 MiB page the process owned stays alive indefinitely |

**Related defects of the same class:**

- `Descriptor::clone` (`fd.rs:61-80`) copies `IoUring(RingId)` and
  `Listener(String)` by value with **no refcount**, unlike `PipeReader`/
  `PipeWriter` which are correctly refcounted. Reachable from `sys_dup`,
  `sys_dup2`, and `build_child_fds` (`loader.rs:242`). A child that inherits a
  listener fd and exits calls `listener::remove(name)` and **destroys the
  parent's listener**.
- `SharedToken` is `#[derive(Clone, Copy)] struct SharedToken(u32)`
  (`shared_memory.rs:15`) — 7 creation sites, 4 cleanup sites, no destructor.
- `file_cache::init(max_pages)` is **never called**, so `max_pages` stays
  `usize::MAX` and `evict_if_needed` returns immediately (`file_cache.rs:32,292`).
  The file cache is unbounded on the kernel heap.
- `peak_memory` is written by two mutually-overwriting paths — demand-paging
  (`process.rs:1276`) and mmap (`syscall.rs:936,951`) — so it is not a peak of
  anything.
- Nothing charges kernel-heap bytes to a process at all. `SYS_SYSINFO`'s
  per-process `memory` (`syscall.rs:1051`) counts only user-visible page
  allocations and excludes kernel stacks, page tables, and every heap allocation
  made on the process's behalf.

**The through-line: every one of these is a handle that is not a value with a
destructor.** That is the bug class. Not "we forgot a cleanup call" — the design
*requires* remembering, across three exit paths, one of which forgets.

## 3. Design

Two allocator kinds. Nothing else allocates.

```rust
/// Immortal kernel state. ZST — the owner is entirely in the type.
Kernel<O: Owner>

/// Process-owned. One word. Charged; freed when the last allocation drops.
Proc
```

### 3.1 The owner taxonomy — justification with zero runtime cost

```rust
pub trait Owner {
    const SUBSYSTEM: Subsystem;      // enum: Sched, Mm, Vfs, Net, Ipc, Drivers, …
    const PURPOSE: &'static str;     // "run queue", "page table", "pipe ring"
}

pub struct Kernel<O: Owner>(PhantomData<O>);   // ZST, Copy
```

`Kernel<O>::allocate` bumps `STATS[O::SUBSYSTEM as usize]` — a const index into a
static array — and forwards to dlmalloc. The purpose string is an associated
const, so it costs nothing at runtime and appears only where it is printed.

Declaring an owner is one macro line:

```rust
owner!(PageTables, Mm, "page table");
owner!(RunQueue,   Sched, "per-CPU run queue");
```

Use site:

```rust
let root: Box<PageTablePage, Kernel<PageTables>> = Box::new_in(page, Kernel::new());
```

**Verified free.** Const-asserted in the experiment
(`scratchpad/allocexp`): with a ZST allocator, `Box<T, A>` is one word,
`Arc<T, A>` is one word, `Vec<T, A>` is three, and `Option<Box<T, A>>` keeps its
niche. Note this is *true but not documented* — `alloc::boxed`'s layout guarantee
is written for `Box<T>` with `Global` and never extended to a custom ZST `A`. So
the spec **requires** a `const { assert!(size_of::<…>() == size_of::<usize>()) }`
next to each alias, turning an undocumented guarantee into a build-time check.

**Why an associated const and not a const generic.** `Purpose<"page table">` does
work today, but only behind `unsized_const_params`, which is gated `incomplete`
since 1.82 with no timeline; the stabilization-track subset
`min_adt_const_params` (#154042, new in 1.96) **excludes `&'static str`** — I
verified rustc says so explicitly. Associated consts on unit structs are
equivalent in power here and need no gate at all.

### 3.2 Process ownership

```rust
pub struct ProcAccount {
    charged: AtomicUsize,
    peak: AtomicUsize,
    pages: Lock<Vec<PhysPage, Kernel<ProcArena>>>,
}
pub struct Proc(Arc<ProcAccount>);       // one word, Clone
```

Every allocation made *on behalf of* a process uses `Proc`. It charges
`layout.size()` on allocate and credits it on deallocate.

Freeing is **refcount-driven, not exit-driven**: the account dies when the last
allocation drawn from it dies. This is deliberate and matches the capability
spec's settled policy (§9.3: *"Revoke never frees; freed-while-referenced stays
unrepresentable even under revocation"*). Exit-driven freeing would reintroduce
exactly the ordering hazard that spec exists to eliminate.

**Why `Arc` and not a branded lifetime.** `Vec<T, &'p ProcAccount>` genuinely
works and the borrow checker genuinely prevents escape — verified: smuggling one
into a `static` fails with *"assignment requires that `'1` must outlive
`'static`"*. It is the more beautiful design. It is also unusable here: kernel
structures for a process must live in the global process table and be reachable
from per-CPU run queues, and a brand cannot cross a `static` or a CPU boundary
without re-branding. `Arc<ProcAccount>` costs one word and works everywhere.
(`generativity` 1.2.1 remains the right tool if a future subsystem is genuinely
scope-shaped.)

### 3.3 The invariant that makes leaks loud

Refcounting alone does not *prevent* a global structure from holding process
memory forever — that is precisely what the io_uring and listener leaks do today.
So the design does not pretend to prevent it. It makes it **fatal and named**:

```rust
// at reap, after ProcessData drops
assert_eq!(account.charged(), 0,
           "pid {} exited owing {} bytes to {:?}", pid, n, account.top_subsystem());
```

Every leak in §2 becomes a panic at reap that names the subsystem responsible,
instead of silent permanent growth. This is the ToyOS answer — *fail fast, scream
loudly* — applied to memory. It converts an invisible class of bug into the
loudest possible one, which is the only reason the class survived this long.

## 4. Enforcement: how "unjustified" becomes a compile error

The obvious approach fails, and it is worth recording why so nobody retries it:
**omitting `#[global_allocator]` does not work.** rustc emits `error: no global
memory allocator found` whenever `alloc` is in the crate graph — verified in
three cases, including a binary that *only* used tagged allocation and never
touched `Global`.

Rust-for-Linux's answer was to stop linking `alloc` entirely (Linux 6.13, commit
`392e34b6bc22`), defining their own `Allocator`, `KBox`, `KVec`. That makes
`Global` a name-resolution error — total enforcement — at the cost of owning
reimplementations of every collection forever.

**ToyOS gets the same enforcement for ~50 lines.** A type alias does not inherit
the underlying type's default parameters:

```rust
// kernel prelude — note: NO default for A
pub type Box<T, A> = alloc::boxed::Box<T, A>;
pub type Vec<T, A> = alloc::vec::Vec<T, A>;
pub type BTreeMap<K, V, A> = alloc::collections::BTreeMap<K, V, A>;
```

```
Box<u32>  →  error[E0107]: type alias takes 2 generic arguments but 1 was supplied
```

Verified. `Box::new` is inherent to `Box<T, Global>`, so it does not resolve
through the alias either. Three layers, in order of strength:

1. **Compile error** (the aliases) — catches every idiomatic call site.
2. **CI grep** for fully-qualified `alloc::boxed::`/`alloc::vec::`/`alloc::string::`
   /`alloc::collections::` — catches the one deliberate bypass, and such a line is
   obviously wrong on sight. The string arm is not hypothetical: `main.rs:230` and
   `arch/syscall.rs:813` are exactly this shape today.
3. **Runtime trap** — the `#[global_allocator]` that must exist panics on use.
   Verified to compile and link fine, so the backstop is free.

A custom rustc lint would make layer 2 total, and ToyOS is uniquely able to write
one since it forks rustc. It is **not** recommended: it buys the difference
between "compile error plus a grep" and "compile error", at the price of a lint
that must survive every rustc rebase.

## 5. Accounting this unlocks

Free once every allocation names an owner, and all currently missing:

- `meminfo` — kernel heap broken down by subsystem and purpose. Today the only
  heap statistic that exists is dlmalloc's *segment* page count; live bytes are
  not tracked at all.
- Per-process kernel memory, correctly — replacing `peak_memory`, which is
  written by two paths that overwrite each other.
- A real pressure signal and the basis for quotas and an OOM policy, which
  CLAUDE.md lists as a known gap ("No physical memory fairness").
- Leak detection as a **test**: run the suite, assert every reaped process
  reached zero charged bytes.

This is strictly better than what the field does. Linux's allocation profiling
(`CONFIG_MEM_ALLOC_PROFILING`, 6.10) attributes per **callsite** — it tells you
which line allocated, never which process should pay, and helper functions steal
their callers' attribution unless hand-annotated. memcg charges the *current*
task, so kernel work done on another cgroup's behalf is misattributed, and on
cgroup deletion charges are reparented upward, destroying the history. Fuchsia's
own docs concede that kernel memory for handles and objects is *attributed to
nobody*. Rust-for-Linux types which **pool** an allocation comes from (`Kmalloc`
/ `Vmalloc` / `KVmalloc`) and passes GFP flags as a runtime bitfield — nothing in
their design encodes who owns or pays, and they had every incentive to.

seL4 is the one system that gets this structurally right, by having **no kernel
heap at all**: every object is retyped from an `Untyped` capability the caller
holds, so authority to allocate *is* the accounting. ToyOS has a heap and wants
one; §3.3's charge-and-assert is the closest achievable analogue — every byte
traces to an owner, checked at reap rather than proved in Isabelle.

## 6. What this does not do

Stated plainly, because a design that overclaims here is worse than none:

- **It does not prevent a leak.** It charges one, and makes it fatal at reap.
  Prevention needs the leaked thing to be a value with a destructor — which is
  the capability-handles spec, not this one. **These two specs must land
  together**; §2's bug table is the intersection of both.
- **It does not bound memory.** Quotas, pressure signals, and an OOM killer are
  downstream of the accounting, not part of it.
- **It does not attribute `alloc::String`.** `String` takes zero generic
  parameters and hard-codes `Global`. **Decided 2026-07-28 — see §6.1.** Not by
  forking std: by a ToyOS-owned `KString<A>`, done inside Stage 2.
- **It does not make "must be consumed" a compile error.** `#[must_use]` is
  inadequate — verified: binding, `let _ =`, `drop()`, and burial in a collection
  all pass a `#[must_use]` type silently. True linear types are RFC-only and the
  most recent serious attempt died in October 2025, blocked on unwind semantics.
  The drop-bomb idiom the kernel already uses (`WaitTicket`, `scheduler.rs`)
  remains the state of the art, and it is a runtime check.

### 6.1 Strings: `KString<A>`, not `String<A>` — decided

The question "should ToyOS implement the allocator parameter on `String`?" was
evaluated by four parallel investigations and three independent judges (upstream
status, kernel usage, fork cost, alternatives). All three judges reached **B —
build `KString<A>`** independently. The decisive facts, each re-verified here:

**1. `String<A>` would attribute exactly ONE allocating site in the whole
kernel.** `String::new()` does not allocate — `rust/library/alloc/src/string.rs`
is `pub const fn new() -> String { String { vec: Vec::new() } }`, capacity 0.
`new_in` is the only constructor the upstream work hands us. Of the kernel's four
`String::new()` sites, three (`vfs.rs:145` ×2, `:154`, `:227`) return empty
strings that are never grown; only `process.rs:777` reaches the allocator.

**2. And that one site should be deleted, not tagged.** `process.rs:777`
allocates a scratch formatting buffer *while holding the process-data lock*, on
the process-exit path — where an OOM routes into `try_recover_from_panic`, which
frees nothing (§2). ToyOS already owns the right primitive: `SerialWriter`
(`drivers/serial.rs:202-282`) is a 1 KiB stack buffer implementing
`core::fmt::Write` that spills to the log ring rather than truncating, already
used by 273 `log!` sites with zero allocation.

**3. Upstream will not rescue option A.** rust#149328 is real, reviewed
("The code LGTM" — Amanieu), crater-clean (5 regressions in 829,643 crates) and
was `bors r+`'d on 2026-03-02 — but it is still open, `mergeable: false`, stuck
on a rustdoc URL-stability question ToyOS has no standing to resolve. More
importantly its scope is wrong for us: `library/alloc/src/fmt.rs` is **absent**
from its 168 changed files, and `From<&str>`, `FromIterator`, `ToString` and
`Cow` stay on `Global` — verified against the PR's own file list. So `format!`
(23 sites) and `String::from` (33 sites) remain untagged **even after it
merges**. Every *stored* kernel string is built with `String::from`.

**4. `string.rs` has no dispatch site to join.** The rule the fork actually
enforces is CLAUDE.md's third bullet — add a target arm to an existing
platform-dispatch site, never change cross-platform semantics. `string.rs`
contains zero `target_os`/`target_family`/`cfg(unix)`/`cfg(windows)` predicates,
so there is nothing to join; and it saw 106 upstream commits since 2024-01-01.
The fork's `library/alloc` + `library/core` delta is currently **empty**, and
should stay that way.

```rust
pub struct KString<A: Allocator> { buf: Vec<u8, A> }   // UTF-8 by construction
```

Same layout as `String` plus the tag. Needs only `Vec::new_in`,
`with_capacity_in`, and `extend_from_slice` — no std change of any kind.
`impl core::fmt::Write` needs only `write_str`, so `write!`/`format_args!` keep
working. ~23 stored sites change, in the same eight files Stage 2 already
rewrites for `Vec`/`BTreeMap`/`Arc` — which is why this belongs *inside* Stage 2
rather than as its own project.

Three honest costs. (a) ~100–150 lines of ToyOS-owned code duplicating a std
type: real debt, denominated in our maintenance rather than rebase conflicts.
(b) One `unsafe` (`str::from_utf8_unchecked` in `as_str`) with a `debug_assert`,
in a project that just held `toyos-sched` to `deny(unsafe_code)`. (c) **LLDB
regression**: `rust/src/etc/rust_types.py:42` matches only `alloc::…::String`, so
`KString` renders as a raw byte `Vec` in the debugger ToyOS lives in. Budget a
ToyOS-owned summary provider next to `.claude/qmp.py`.

Do these first — they *delete* strings rather than tag them, and each fixes a
separate bug:
- `Descriptor::Listener(String)` (`fd.rs:57`) → `ListenerId`. `listener.rs:56`
  already is the intern table and all six consumers immediately re-look-up the
  id, two of them cloning the name per accept-poll. Also fixes the
  un-refcounted `Descriptor::clone` in §2.
- `bcachefs_adapter` stores each filename twice (map key + `OpenFileInfo.name`)
  purely so `close_file` can find the key — a free 50% cut.
- `user_ptr.rs:149` (`user_str`) applies **no length check**, and no `MAX_PATH`
  constant exists anywhere. This is what actually bounds `Vfs.created_dirs`,
  which takes unbounded userland paths from `mkdir` and is never freed on
  process exit.

**Unresolved boundary:** `bcachefs/src/fs.rs:529` `list() -> Vec<(String, u64)>`
and `:506` `read_link() -> Option<String>` are the kernel's largest transient
string producers and scale with the self-hosting workload. bcachefs is
ToyOS-owned so it can change, but returning `KString<A>` drags `allocator_api`
into it. Prefer a visitor over `&str` — removing the allocation beats tagging it.
**Decide this before writing `KString`: it determines which crate it lives in.**

**Revisit trigger.** Re-open only when rust#149328 is merged **and** its file
list includes `library/alloc/src/fmt.rs` or an allocator-generic
`impl From<&str>`. Both halves required — a merged #149328 as written still
attributes zero stored kernel strings. If both land, alias `KString` to
`String<A>` and delete it; nothing in the design changes either way, which is
why it is safe to build now.

## 7. Rejected alternatives

| Rejected | Why |
|---|---|
| Drop `alloc` entirely (RfL model) | Total enforcement, but thousands of lines of collections owned forever. Prelude aliases get the same compile error for ~50 lines. |
| Custom rustc lint | Available to us uniquely; unnecessary given the aliases, and must survive every rustc rebase. |
| Branded lifetimes / `generativity` | Genuinely prevents escape (verified) but cannot cross a `static` or CPU boundary — fatal for a global process table and per-CPU queues. |
| Const-generic purpose strings | Needs `unsized_const_params`, gated `incomplete` with no timeline; `min_adt_const_params` excludes `&'static str`. Associated consts are equivalent and gate-free. |
| A runtime tag word per allocation | Costs memory per allocation and is strictly weaker than putting the owner in the type. |
| Storage API | Dead. Pre-RFC stalled 2023-05, no RFC ever filed. |
| Exit-driven freeing | Reintroduces the ordering hazard the capability spec eliminates. Refcount drain instead. |

## 8. Staged migration (sketch — to be expanded before implementation)

Always green, one gate per stage: build clean + zero warnings, `cargo test`, and
the audio glitch gate (with the caveat that Gate A's recorded baseline is
currently known-optimistic — see CLAUDE.md).

0. `kernel/src/mem/owner.rs` — `Owner`, `Subsystem`, `Kernel<O>`, `owner!`, the
   prelude aliases, the layout const-asserts. Nothing uses it yet.
1. Convert the 14 `Box::new` sites. Smallest possible blast radius, proves the
   ergonomics.
2. Convert `Vec`/`BTreeMap`/`Arc` per subsystem, one commit each, biggest first
   (`vfs.rs` 47 sites, `loader.rs` 31, `bcachefs_adapter.rs` 21).
3. Add `#[alloc_error_handler]`. Today OOM panics into the leakiest path.
4. `ProcAccount` + `Proc`; route the four PMM chokepoints through it.
5. The reap-time assert. Expect it to fire — that is the point; each firing is
   one of §2's leaks.
6. `meminfo`, per-process memory in `ps`, leak assertion in the test suite.

Stages 4–6 interlock with the capability-handles spec; sequencing is **OPEN**
until that spec's stage plan is revisited.

## 9. Open questions

1. **`String`.** See §6. Under evaluation.
2. **Sequencing against capability-handles.** §2's bug table needs both specs.
   Which lands first, and does either need re-staging?
3. **Per-CPU heap arenas.** Everything today goes through one dlmalloc behind one
   spinlock (`mm/alloc.rs:51`) and one PMM bitmap behind another
   (`mm/pmm.rs:196`). Owner-typed allocation makes per-CPU or per-subsystem
   arenas expressible for the first time. Out of scope here; worth its own study
   once contention is measured rather than assumed.
4. **The 2 MiB single-allocation cap** (`mm/alloc.rs:12`) is enforced by panic.
   Is that still the right policy once allocations are owner-typed?
