---
status: open
kind: defect
opened: 2026-08-18
---

# `Rights::LOG`'s doc names `/bin/console` as a holder, and no manifest gives it one

`toyos-abi/src/handle.rs`, `Rights::LOG`:

> `/bin/logd` holds it because writing `/log` is its job, `/bin/console`
> because it paints the panel, and `test-runner` because a gate reads what the
> kernel said.

The middle clause is false. Verified 2026-08-18 against all twelve boot
configs: `logread` appears on `logd` in every one and on `test-runner` in the
seven that have one, and on nothing else. `console/system.toml` gives
`/bin/console` no `syscap` row at all — it seeds its scrollback from the
previous boot's `/log` files and reads no cursor, and the decision recorded at
the time was that a right with no caller is a capability handed out for a plan.

`src/build.rs`'s `every_boot_config_runs_logd` is what holds the tree to that,
and it would red if the doc comment were made true by a manifest edit rather
than the other way round. So the ABI's own description of who holds the right
disagrees with the gate that enforces it.

This file also recorded a second defect in the same crate: `log.rs` carried a
citation of a log architecture document that had been deleted. **That half is
closed** — the sweep that removed every spec citation from the tree took it with
the rest, and no `toyos-abi` doc comment points at a document any more.

**What is left is one doc comment, and it was not fixed here.** `toyos-abi/src`
is sysroot source: `pr::abi_lands_alone` refuses a branch that mixes it with
work that depends on it, and a worktree whose `toyos-abi` differs from `main`'s
holds the sysroot claim against every other worktree on the host. One doc
comment is not worth that window on its own — it belongs to whatever ABI change
lands next.
