---
status: open
kind: finding
opened: 2026-08-20
---

# `SYS_SHUTDOWN` takes no handle and no right, so any process ends the machine

Found by the capability audit of 2026-08-20
(`issues/kernel/the-capability-end-state-is-twelve-answers.md`, questions 2 and
5).

The dispatch arm takes no argument at all: it syncs the filesystems, waits for
`/bin/logd` to make the last records durable, drains the console inline and
calls `acpi::shutdown`, which does not come back
(`kernel/src/arch/syscall.rs:328`-`349`). There is no handle to resolve, no
`Rights` to check, and nothing anywhere in the tree that says the absence is
deliberate — `toyos::system::shutdown` is a four-line wrapper
(`toyos/src/system.rs:28`) and `/bin/shutdown` is a two-line toybox applet over
it (`userland/toybox/src/shutdown.rs`), reachable through the
`bin/shutdown -> /bin/toybox` symlink `system.toml` puts in every image.

## Why it stands out

Every other machine-wide authority in this kernel rides a bit on the one
`SysCap` the kernel mints at boot for `/bin/init`
(`kernel/src/loader/mod.rs:938`), and each one says why at its own site:
minting a device claim needs `Rights::DEVICE` (`kernel/src/arch/syscall.rs:1627`),
entering the real-time band needs `Rights::RT` (`:1655`), turning a pid into a
`Process` handle needs `Rights::MANAGE` (`:1602`), and *reading the log* needs
`Rights::LOG` because it "is every process's business and no process's right by
default" (`:1683`). Powering the machine off is a larger authority than all four
and is the only one that is free.

It is not path authority either, so it does not fall under the ruling that keeps
the filesystem, `SYS_DLOPEN` and `SYS_SPAWN` ambient: there is no name to
resolve and no mount to gate.

## What it costs

`system.toml` gives `sshd` the `launcher` connector, so a remote session starts
programs through `/bin/init`, and every one of them — like every program the
compositor launches, and every shell child — can halt the machine with one
syscall taking no argument. A daemon that has been endowed exactly one
connector, on the argument that a connector is all the authority it needs, holds
this too.

## What would fix it

One more bit on `SysCap`, checked the way the four beside it are: resolve the
handle, demand the right, refuse otherwise. `/bin/init` holds the full cap and
would endow it by `system.toml` to whatever is meant to be able to power the
machine down. The bit is free — `Rights::ALL` is `0x3ff`
(`toyos-abi/src/handle.rs:92`) and bits 10..31 are unused — and `Rights`'s own
`Debug` table has to gain the name in the same change, which
`every_right_in_all_has_a_name_and_every_name_is_in_all` already gates
(`toyos-abi/src/handle.rs:207`).

Not done here: whether `SYS_SHUTDOWN` should be rights-bearing is one of the
four rulings `issues/kernel/the-capability-end-state-is-twelve-answers.md` puts
before the owner. Changing the arm would answer it silently, which is what that
track exists to stop.
