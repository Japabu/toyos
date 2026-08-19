---
status: open
kind: finding
opened: 2026-08-16
---

# An ABI item only a fork calls looks caller-less to every scan we run

`rg` over this repository is how "nothing calls this any more" is established,
and **the fork estate is not in this repository**. Sixteen forks consume
`toyos-abi` and `toyos`; `rust/Cargo.toml:95` resolves `toyos-abi` to
`../toyos-abi`, this tree's copy, for everything the rust submodule builds. So
a deletion here can break a fork branch that no gate in this repository, and no
build this repository runs, ever compiles.

**`SYS_STACK_INFO` (61) is the live case.** The 2026-08-15 mechanism
consolidation audit, item A9, recorded it as CONFIRMED caller-less, and inside
the tree it is: at `821c40b` the only hits
are the declaration, the wrapper, the kernel arm, and prose. Outside it:

```
$ rg -n 'stack_info' ~/.cargo/git/checkouts/stacker-dd045e8025e5c69e/c25842a/src/backends/toyos.rs
3:    let (base, _size) = toyos_abi::syscall::stack_info()?;
```

`c25842a` is the revision `rust/Cargo.lock:5409` pins
(`git+https://github.com/Japabu/stacker?branch=toyos#c25842ac264c7121e33c5ad81f93dc7bba22cca2`),
and stacker declares `toyos-abi` under
`[target.'cfg(target_os = "toyos")'.dependencies]` — so the call compiles when,
and only when, rustc is built *for* ToyOS. `rust/bootstrap.toml` has
`host = ["aarch64-apple-darwin"]` today, which is why nothing here goes red: the
backend is real, wired, and currently uncompiled. Retiring the syscall would
leave the fork branch unbuildable at the moment the self-hosting build it exists
for is first attempted, and the break would surface nowhere near this commit.

`SYS_STACK_INFO` was therefore **not** retired with the rest of A9's vestige
sweep (`SYS_SHM_UNMAP`, `CENSUS_TOTAL`, `CENSUS_BREAKDOWN`,
`IORING_OP_POLL_REMOVE` all had no caller in the estate either, checked the same
way, and went). Retiring it is two repositories in a fixed order — the fork
first — plus an answer to the question underneath: whether a self-hosted rustc
gets its stack bounds from a syscall or from something the thread already knows.
That is the owner's, not an agent's.

**What is owed here is not the syscall; it is the blind spot.** Every "zero
callers" claim about an ABI item in this tree is worth exactly the trees it
searched, and none of ours search:

- `~/.cargo/git/checkouts/` — the pinned revisions cargo actually builds, and
  the cheapest complete sweep there is;
- the fork branches themselves, which move independently of those pins.

`forks.toml` is the estate's inventory and could be the list a check walks.
Until something walks it, an ABI deletion is verified by hand or not at all.
