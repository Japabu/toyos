---
status: open
kind: defect
opened: 2026-08-08
---

# The boot capture landed, and two doc comments still tell agents it did not

The capability is there and is used: `QemuInstance::boot_log` (`tests/common/qemu.rs:1251`,
accessor at `:1548-1558`) holds everything `wait_for_ready` saw on the way to the
ready marker (`:2494-2583`, accumulated into `seen` on both arms and returned at
`:2468-2472`), and `tests/common/serial.rs:41-43` wraps it as `Serial::boot` with
`must_say`/`must_not_say`/`must_be_clean`/`alive`. Counted with
`grep -rn 'boot_log()' tests/ | wc -l` and its two siblings: **68** `boot_log()`
call sites, **13** `Serial::boot`, **131** `must_say`/`must_not_say`/`must_be_clean`
outside the helper. `tests/common/faults.rs:221-242` asserts on
`VirtIO net: NOT INITIALISED at PCI` and four more boot lines; `metal_sim_scanout_wc`
reads its PAT and MTRR lines out of a shared boot's seeded console
(`tests/toyos.rs:3530`, `:3841-3893`). It arrived in `8025f57` (2026-07-31) and
`a1ef357` (2026-08-01), both ancestors of `main`.

**What is left is narrower and real.** `TestResult.serial` still starts at
`===TEST_START` — `tests/common/qemu.rs:1747-1748` sets `in_test` there and
nothing is appended before it — so gate A's `serial`, built at
`tests/toyos.rs:1283` as `result.serial + &qemu.drain_serial(500ms)`, carries no
boot prefix, and `audio::check_suspend_structure` (called at `tests/toyos.rs:1409`)
therefore still cannot see a device started before the window opened. That is
one line to fix now that the capture exists: prepend `qemu.boot_log()`.

**And two comments now mislead about the harness, which is why this reads as
open.** `tests/common/audio.rs:519-531` says the harness "never joins the reader
thread's full log … Catching that needs the boot capture the harness currently
throws away" — it does not throw it away. `tests/toyos.rs:1022-1024` says a
device started before `===TEST_START` is "invisible here as it is everywhere
else" — it is not invisible everywhere else. One genuine blind spot survives by
design: `BootOptions::mute` sets `boot_log` to `String::new()`
(`tests/common/qemu.rs:2468-2470`), and `Serial::alive()` exists so an assertion
over an empty capture fails rather than passing vacuously.
