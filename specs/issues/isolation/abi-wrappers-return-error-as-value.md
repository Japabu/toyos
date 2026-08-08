---
status: assigned
kind: defect
opened: 2026-08-01
---

# Two ABI wrappers return an error word as a value, and a fork blocks each

`syscall::pipe()` and `syscall::tls_alloc_block()` cannot express failures the
kernel already returns. Both fixes are one line of ABI each and both are
**blocked on an edit outside the monorepo**, so the wrappers carry a doc comment
saying they are dishonest until someone has the quiet-tree window.

`pipe()` — `sys_pipe` answers `ResourceExhausted` on three paths (`syscall.rs:835-849`:
no pipe pages, and either `fds.insert` hitting `MAX_FDS`). Computed:
`ResourceExhausted.to_u64() = 0xfffffffffffffff8`, which the wrapper splits into
`read = Fd(-1)`, `write = Fd(-8)`. In-tree that surfaces as a **soundd panic**:
`soundd/src/main.rs:427-428` does `syscall::pipe()` then
`pipe_id(..).expect("pipe_id failed")`, so a client that exhausts the fd table
kills the audio daemon. `net.rs` survives by accident — its next call is
`pipe_id` too, but `map_err`'d. Fix: `pub fn pipe() -> Result<PipeFds, SyscallError>`.
**Fork edit owed:** `mio`, branch `toyos`, `src/sys/toyos/waker.rs:13` —
`let pipe = toyos_abi::syscall::pipe();` becomes `let pipe = toyos_abi::syscall::pipe()
.map_err(|_| io::Error::other("pipe"))?;` (`Waker::new` already returns
`io::Result<Waker>`). Eight other in-tree call sites gain a `?`.

`tls_alloc_block()` — the kernel returns `InvalidArgument` for `module_id == 0`
or a module outside the process's list, and `ResourceExhausted` past
`DTV_INITIAL_CAPACITY` (`arch/syscall.rs:1720-1789`). The doc comment claimed
"Panics in the kernel", which stopped being true at the hardening pass, and
claimed a *physical* address where the kernel returns a **virtual** one — both
corrected in place. Consequence: `__tls_get_addr_slow` adds `offset` to a value
near `u64::MAX` and returns the wrap as a pointer; computed, `InvalidArgument`
plus an offset of 16 is `0xb`. Fix: wrap in `check`.
**std edit owed:** `rust/library/std/src/sys/pal/toyos/tls.rs:29-31` — the
variable is even named `block_phys`. `__tls_get_addr`'s ABI is that it returns
an address and there is no caller to return an error to, so the right answer is
`rtabort!`, which is what the current code is reaching for and constructing the
wrong pointer instead.

Batch them: one quiet-tree window covers both, and the audit's F9 (`get_env`,
`waitpid`) is the same window again.
