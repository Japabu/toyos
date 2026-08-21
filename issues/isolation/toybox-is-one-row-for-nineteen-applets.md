---
status: open
kind: finding
opened: 2026-08-20
---

# `[programs.toybox]` is one authority row for every applet in the binary

Noticed while giving `/bin/shutdown` the `power` right
(`toyos-abi/src/handle.rs`'s `Rights::POWER`).

A manifest row's granularity is the **binary**, and `/bin/toybox` is nineteen
programs behind nineteen symlinks. `/bin/init`'s `declared` resolves
`/bin/shutdown` through `std::fs::read_link` to `/bin/toybox` and endows the
`toybox` row (`userland/init/src/main.rs`), so every applet in the image is
spawned holding the union of what any of them needs.

This is not new — `system.toml`'s `toybox` row already hands `ls` a connector to
the compositor and one to soundd, because `screen` and `tone` need them. What is
new is the size of the largest member of that union: `power` ends the machine,
so `/bin/echo` in the shipped image now holds a capability that cuts the power,
where before it held two connectors it does not use.

## What it is not

It is not the defect `shutdown-needs-no-capability` was. Before that change
*every process in the machine* could halt it with an argument-less syscall;
now the set is one program's row plus `/bin/init`. The remaining exposure is a
program a person deliberately runs from a shell, not a daemon endowed one
connector.

## What would fix it

A `[programs]` key whose row is chosen by the **invoked path** rather than by
the binary the symlink resolves to. `declared` already tries the caller's own
path first (`row(path)` before `read_link`), so a row keyed `shutdown` with
`path = "/bin/shutdown"` would win — but nothing builds such a row today: a
`[programs]` key names a crate the build compiles, and `bin/shutdown` is a
`[symlinks]` entry with no crate behind it. Making the build able to declare an
authority row over an existing binary is the work, and it is worth doing only
if the union keeps growing.
