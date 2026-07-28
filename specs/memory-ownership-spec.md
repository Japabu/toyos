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
2. **CI grep** for fully-qualified `alloc::boxed::`/`alloc::vec::` — catches the
   one deliberate bypass, and such a line is obviously wrong on sight.
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
- **It does not attribute `String`.** `alloc::string::String` takes zero generic
  parameters and hard-codes `Global` — verified. **OPEN**: whether to implement
  `String<A>` in the rust fork, build a ToyOS-owned `KString<A>` over
  `Vec<u8, A>`, or eliminate heap strings from the kernel. Under evaluation
  separately; the leading concern is that `library/alloc/src/string.rs` is the
  most cross-platform file in std and CLAUDE.md forbids editing such files.
- **It does not make "must be consumed" a compile error.** `#[must_use]` is
  inadequate — verified: binding, `let _ =`, `drop()`, and burial in a collection
  all pass a `#[must_use]` type silently. True linear types are RFC-only and the
  most recent serious attempt died in October 2025, blocked on unwind semantics.
  The drop-bomb idiom the kernel already uses (`WaitTicket`, `scheduler.rs`)
  remains the state of the art, and it is a runtime check.

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
