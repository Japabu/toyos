---
status: open
kind: finding
opened: 2026-08-10
---

# A transferred connector cannot be merged into an inherited namespace

`Command::provide(name, connector)` says "give the child this connector under
this name, on top of whatever else it holds". The launcher can do that: init
builds the child's namespace itself, from the manifest row plus the extras the
caller transferred (`userland/init/src/main.rs`, `build_namespace`).

The **direct** path cannot. `SYS_SPAWN` gives the child a duplicate of the
caller's own namespace and nothing else; adding a name to it would mean building
a new namespace that keeps everything the base has plus one more, and
`SYS_NAMESPACE_BUILD` has no spelling for "keep everything" — `Builder::keep`
takes the names to keep, and no syscall enumerates a namespace.

So a caller that transfers a connector to a program the manifest does **not**
declare — the one case where the launcher answers `MSG_NOT_DECLARED` and the SDK
falls back — hands over a name the child cannot resolve.

## Why nothing in the tree hits it

The callers with extras are the terminal and the console, and both transfer
`surface` to `/bin/shell`, which is a `[programs]` key and goes through the
launcher. The shell then transfers `surface` to *its* children, declared or not
— and for the undeclared ones inheritance already carries it, because init
merged `surface` into the shell's own namespace when it launched the shell.

Until 2026-08-10 the SDK refused this case outright (`io::ErrorKind::NotFound`),
which made every test binary typed at a shell fail to start: `screen_console_scroll`,
`console_locale_detect`, `desktop_locale_detect` and `desktop_audio_client` all
stalled on `test_rs_…: not found`. The fallback is unconditional now, which is
what `specs/capability-endowment-spec.md` §6 clause 2 says it should be.

## What closing it would take

A "keep everything in base" spelling for `SYS_NAMESPACE_BUILD` — `NamespaceBuild`
has a `_pad: u32` that could become a flags word without moving a field — and a
`Builder::keep_all`, after which `sys/process/toyos.rs`'s direct path builds
`keep_all(parent) + add(extras)` and endows it as `svc`. That is inheritance plus
the extras with no coincidence in it.
