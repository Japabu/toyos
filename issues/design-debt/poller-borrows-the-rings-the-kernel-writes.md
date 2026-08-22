---
status: open
kind: finding
opened: 2026-08-22
---

# `toyos::Poller` is the kernel's inbox borrow, mirrored

`toyos/src/poller.rs` reaches the inbox page — the one `SYS_INBOX_SETUP` maps —
through Rust references at five sites:

| site | shape | what it names |
|---|---|---|
| `watch_raw`, ring | `&RingHeader` | `SUBMISSION_RING_OFF` |
| `watch_raw`, entry | `&mut Submission` | one submission slot |
| `pending` | `&RingHeader` | `SUBMISSION_RING_OFF` |
| `wait`, ring | `&RingHeader` | `COMPLETION_RING_OFF` |
| `wait`, entry | `&Completion` | one completion slot |

The kernel writes the same bytes: `inbox::post_completion` stores a whole
`Completion` and publishes the tail, and `claim_submission` reads a submission
slot and advances the submission head. So these are references over memory a
second writer can change while they exist, which is the argument
`kernel/src/user_ptr.rs`'s `UserBytes` header makes, in the other direction.

The two entry borrows are the sharp ones. `Submission` and `Completion` are all
integers and therefore `Freeze`, so `&Completion` carries `noalias` and
`readonly` and `&mut Submission` carries `noalias` — LLVM is entitled to assume
the kernel is not touching those bytes. The `&RingHeader`s are a data race on
`ring_size` in principle, though a milder one than the kernel side had: the
kernel writes that field once, in `create`, before the page is mapped, and never
again.

Nothing is known to be broken. Every field read through the two header borrows
is an atomic, the slot indices really are disjoint from the kernel's while both
sides keep the protocol, and `head`/`tail`'s acquire/release edges are what
order the entry accesses against each other.

**The kernel half of exactly this was fixed on 2026-08-22** (`kernel/src/inbox.rs`):
the ring headers are reached one `&AtomicU32` at a time via `AtomicU32::from_ptr`
— sound over shared memory, because an atomic's `UnsafeCell` is what withdraws
`noalias`/`readonly` — and an entry is a `read_volatile`/`ptr::write` of the
whole struct, so no reference over that page exists on the kernel side any more.
The same two moves apply here unchanged, and this file exists because the SDK
was out of that task's scope rather than because the question is different.

`issues/build/ring-rs-shared-slice-over-a-userland-writable-page.md` asks the
same thing about `toyos-abi/src/ring.rs`'s pipe rings, which is a third
mechanism with the same shape.
