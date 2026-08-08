---
status: open
kind: rejected
opened: 2026-08-08
---

# Every toolchain build runs Python, and every host link runs `cc`

**The owner ruled on 2026-08-08: *"its required by rusts toolchain i guess we can
be transparent about that."*** Both are named in the README's Prerequisites
section and by `check_prerequisites` in `src/main.rs`, which is now two lists —
`REQUIRED`, which exits (`git`, `rustup`, `qemu-system-x86_64`, `cc`), and
`ALSO_USED`, which names what is absent and continues (a Python, `df`, `ps`,
`find`). The README's opening no longer claims Rust and QEMU are the whole
setup.

**The entry stays open because declaring is not removing.** The hole is the
same size: `bootstrap.py` still cannot run inside ToyOS, and the second option
below — a Rust bootstrap in the `rust/` fork — is still the only thing that
closes it.

Two details the fix had to get right. The preflight looks for *any* of
`python3 python py python2 uv`, because that is what `rust/x` searches and a
machine with only `python` builds fine; and it scans `PATH` rather than running
`--version`, because asking macOS for `py` opens the Command Line Tools
installer. `cc` is stated with its scope attached wherever it appears — no guest
binary links through it — because *"ToyOS needs a C compiler"* is false and
reads as a far larger claim than the truth.

`specs/dependency-audit-2026-08-08.md` §3–§4 is the full inventory; this is the
entry that says the two largest holes in *"Rust and QEMU, one command"* are real.

`src/toolchain.rs:749` picks `./x` when `rust/x` exists, which it does. That file
is a `/bin/sh` script whose whole job is `SEARCH="python3 python py python2 uv"`,
and it execs `x.py` → `src/bootstrap/bootstrap.py` (55,550 bytes). So a clean
clone cannot build a toolchain without Python 3. It is upstream's bootstrap and
not our code, which is why it is stated rather than blamed — but the bar has no
upstream exemption, and `bootstrap.py` can never run inside ToyOS.

Separately, and measured with `rustup run toyos rustc --print link-args` on a
trivial host binary: rustc invokes `"cc"` and sets
`SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk`. rustup installs
neither. Every *host* binary goes through it — the build system, the harness,
`toyos-ld`, `toyos-cc`, rustc stage2. **No guest binary does**: both
`.cargo/config.toml`s under `bootloader/` and `kernel/` set
`linker = "toyos-ld"`, so nothing that boots is touched.

`src/main.rs:7`'s preflight checks `git`, `rustup` and `qemu-system-x86_64` and
says nothing about either of these. The cheap half of the fix is to make the
preflight and the README say what the machine actually needs.
