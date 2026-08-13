# Debugging

**A backtrace is named from the binary's own file** — `.symtab`/`.strtab` are read off whatever backs the executable, so a program run from a disk gets the same report as one from the initrd. There is no DWARF debug info (`toyos-ld` drops every debug section): a frame carries a name, never a line number.

**LLDB via QEMU** — all binaries are PIE; addresses change every boot. Parse serial for `Kernel memory located at: 0x...` and load symbols with `--slide`; userland pid and base address are logged at `spawn:`. Use `breakpoint set -r <pattern>` for Rust symbols (`-n` fails on `::` paths). `cargo run` in the background, then `gdb-remote 1234`. `--debug` pauses the kernel before init via `DEBUG_WAIT`, enables QEMU's `-d int,cpu_reset` log at `/tmp/toyos-qemu-debug.log`, and parks QEMU on triple fault so the faulting CPU state stays inspectable.

**QMP** — socket at `/tmp/toyos-qmp.sock`, script at `.claude/qmp.py`: `"ls /bin"` types a string and Enter, `--raw ret` a single key, `--raw n --ctrl` a chord, `--screenshot <path>` captures the screen.

**Reading a frozen guest without `cargo run`** — any harness test with `BootOptions { qmp: true }` leaves a socket under `$TMPDIR/toyos-tests-<pid>/lane-<n>/`. `human-monitor-command` with `info registers -a` gives every vCPU's `RIP`, `RFL` and `HLT` — how a halted-awaiting-interrupt machine is told from a wedged one. Take that capture before injecting anything: a keystroke revives a halted CPU, so Ctrl+Alt+D over the same socket both confirms the diagnosis and destroys the evidence for it.

**Ctrl+Alt+D is the blocked-task dump, Ctrl+Alt+F1 the panic console's pager** — what to press on a machine that stopped without panicking, and what to ask the owner for. Both are detailed in `kernel/CLAUDE.md`.

**Audio** — `cargo run -- --smp N --dump-audio` captures device output to `/tmp/toyos-audio.wav` (parse to EOF — RIFF sizes stay 0 unless the guest shuts down cleanly). `cargo test -- audio` runs the glitch regressions. soundd prints wake/underrun/latency stats every ~2 s while clients exist; doom prints `[music]` telemetry every ~5 s.
