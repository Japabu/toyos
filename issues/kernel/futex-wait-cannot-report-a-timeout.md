---
status: open
kind: defect
opened: 2026-08-16
---

# `SYS_FUTEX_WAIT` answers 0 for a timeout and 0 for a wake

`toyos-abi/src/syscall.rs`'s `futex_wait` declares its contract in one line:

> Block if `*addr == expected`. Returns 0 on wake, 1 on timeout.

`kernel/src/process.rs`'s `futex_wait` cannot produce the second value. Both of
its arms return `0` — one commented "blocked and woken", the other "value
mismatch, returned immediately" — and neither is the timeout the ABI names. One
layer down, `scheduler::futex_wait` returns a bare `true` unconditionally, so
there is nothing for the caller to distinguish a timeout with either: the
`completion::wait_until` it wraps answers `Ok(())` for a satisfied predicate and
`Ok(())` again for an expired deadline, which is the shape every other timed
caller wants and the one this one cannot use.

**Pre-existing, and older than the completion cutover.** It is recorded here
because the sibling half of the same syscall pair *was* this branch's to fix —
`futex_wake` returned 0 for every call in the machine, and
`futex_wake_counts` is the gate that now holds it — and the two were found
together. Nothing in this chunk touched the wait's return.

**What it costs today is small and not zero.** No in-tree caller reads the
value: `userland/libc/src/pthread.rs` discards it, and the std fork's
condvar/rwlock paths re-derive their own predicate after every return, which is
what `scheduler-core-spec.md` invariant 10 requires of them anyway. What it
breaks is any future caller that treats a timed `futex_wait` as answering
whether its own deadline was reached — a `pthread_cond_timedwait` that reports
`ETIMEDOUT`, most obviously — and it breaks it silently, because the honest
answer and the wrong answer are the same number.

Closing it means one of two things, and the choice belongs with whoever lands
`pthread_cond_timedwait`:

- `completion::wait_until` reports whether it returned on the predicate or on
  the deadline, which is a signature change with eleven call sites; or
- `futex_wait` re-reads the word itself after the wait and answers from that,
  which is cheaper and weaker — the word may have changed and changed back.
