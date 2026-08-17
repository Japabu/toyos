# ToyOS

An operating system written from scratch in Rust — bootloader, kernel, drivers,
userland, and the compiler and linker that build them.

```
git clone https://github.com/Japabu/toyos
cd toyos
cargo run
```

One command, and a complete OS boots. No Make, no CMake, no Docker, no LLVM to
install, no cross-toolchain to assemble, no system linker. Everything that
boots is built by a toolchain in this repository.

Rust and QEMU, plus the two things `rustc`'s own bootstrap needs on every
platform — [Prerequisites](#prerequisites) says exactly what they are and why
nothing that boots goes near them.

The name has no meaning. This is not a hobby project.

![Doom running on ToyOS](doom.jpg)

## What it is

Most operating systems are read. ToyOS is meant to be *run* — cloned, changed,
booted, tested, and booted again, in seconds, on a laptop.

Two things follow from that, and they are the point of the project:

**The toolchain is ours.** Our own linker lays out the UEFI bootloader, the
kernel and every userland program. Our own C compiler builds Doom. The Rust
target and its `std` live in our fork of the compiler. Nothing you install
touches anything that boots.

**Kernel decisions live outside the kernel.** The scheduler's policy, the xHCI
port state machine, the HD Audio codec graph, FAT32, GPT, keyboard layouts, the
PS/2 wire protocol — each is an ordinary crate with ordinary tests that run on
your machine in milliseconds, with no VM and no hardware. Several forbid
`unsafe` outright. The kernel calls them; it does not contain them. That is
what makes an OS this size possible to change with confidence.

The north star is **building ToyOS from within ToyOS**: its own compiler, its
own linker, on its own hardware, with no external toolchain anywhere in the
chain.

## Milestones

✅ done · 🔨 in progress · ⬜ planned

### It boots and it runs

| | |
|---|---|
| ✅ | Boots under QEMU, UEFI only — no BIOS and no 32-bit anywhere |
| ✅ | Boots on real hardware: a ThinkPad T14 Gen 2, off a USB stick |
| ✅ | Full SMP — every core brought up, per-CPU run queues, work balanced across them |
| ✅ | A desktop: compositor, windows, mouse, keyboard, copy and paste |
| ✅ | **Doom**, with sound effects and General MIDI music |
| ✅ | **Doom on the laptop** — its own panel, its own keyboard, HDA sound on metal |
| 🔨 | The desktop on the laptop, with the console handing over to it |
| ⬜ | Suspend and resume |
| ⬜ | ARM64 |

### The toolchain is ours

| | |
|---|---|
| ✅ | **Our own linker** — the UEFI bootloader as PE32+, the kernel and all userland as position-independent ELF |
| ✅ | **Our own C compiler** — preprocessor through codegen, Cranelift instead of LLVM |
| ✅ | **A real Rust target** with a real `std`: threads, `dlopen`, unwinding, symbolized backtraces |
| ✅ | The Rust ecosystem, mostly unmodified — crates.io crates compile and run as published |
| ✅ | `rustc` itself built for ToyOS and shipped inside the image |
| 🔨 | Compiling a Rust program *inside* ToyOS |
| ⬜ | `cargo` inside ToyOS |
| ⬜ | Building ToyOS from within ToyOS |
| ⬜ | No LLVM anywhere in the chain, bootstrap included |

### The kernel

| | |
|---|---|
| ✅ | Demand paging, shared memory across processes, 2 MiB pages only |
| ✅ | Processes and threads, position-independent binaries, `dlopen`/`dlsym` |
| ✅ | An event-driven fair-share scheduler, built to scale past 128 cores |
| ✅ | IPC — named services, and pipes backed by shared-memory rings |
| ✅ | A VFS with mount points, and a mount that states whether userland may write it |
| ✅ | Scheduler policy as a standalone crate, with a deterministic simulator and an interleaving fuzzer |
| 🔨 | One blocking primitive for the whole kernel |
| ⬜ | Typed capability handles instead of raw descriptors |
| ⬜ | NX and W^X, for userland and for the kernel's own mappings |
| ⬜ | An ACPI/AML interpreter of our own |

### Hardware

| | |
|---|---|
| ✅ | UEFI framebuffer — write-combined, damage-tracked, composed in RAM |
| ✅ | NVMe |
| ✅ | xHCI and USB mass storage, including reading the stick it booted from |
| ✅ | USB HID, and PS/2 keyboard, TrackPoint and touchpad |
| ✅ | USB hotplug — plug a device in after boot and it appears; pull it and it goes |
| ✅ | virtio: GPU, block, network, sound |
| ✅ | An IOMMU that translates, and names the device when DMA faults |
| 🔨 | Intel HD Audio, as a userspace driver |
| ⬜ | Intel Wi-Fi |
| ⬜ | The Intel display engine — vblank, planes, brightness |
| ⬜ | Battery and power management |

### Storage and files

| | |
|---|---|
| ✅ | **Our own FAT32**, read and write |
| ✅ | **Our own GPT parser** — no allocation, no `unsafe` at all |
| ✅ | A read/write filesystem for user data, on NVMe or USB |
| ✅ | One log file per boot, named for the wall clock, on its own partition |
| ⬜ | Formatting a disk from inside ToyOS |
| ⬜ | Snapshots and checksums |

### Network

| | |
|---|---|
| ✅ | TCP, UDP and DNS, served by a userspace daemon over a virtio NIC |
| ✅ | An SSH server — public keys only, and never in the default boot |
| ⬜ | A wired NIC on real hardware |
| ⬜ | Wi-Fi |
| ⬜ | A network gate with the same teeth as the audio one |

### Isolation

| | |
|---|---|
| ✅ | Drivers in userspace — display, network, audio and SSH each own their device |
| ✅ | Crash a daemon and the kernel is fine |
| ✅ | Memory protection actually enforced, so a guard page faults |
| 🔨 | Devices userland may claim, and devices it may not |
| ⬜ | Capabilities instead of ambient authority |
| ⬜ | Daemons a supervisor restarts when they die |
| ⬜ | A user model — an account that holds fewer rights than the machine |

### Working on it

| | |
|---|---|
| ✅ | **One command** from a clean clone to a booting OS |
| ✅ | The whole OS booted across many machine shapes, in parallel, as an ordinary `cargo test` |
| ✅ | An audio gate that compares captured device output against a recorded baseline, statistically |
| ✅ | Kernel decisions in plain crates, tested on the host in milliseconds |
| ✅ | Reading a machine with no serial port — panics, logs and a blocked-task dump painted on its own panel |
| ✅ | LLDB against a running kernel |
| ⬜ | A Windows host |
| ⬜ | Continuous integration on clean machines |

## Along the way

**Our own C compiler.** `toyos-cc` is preprocessor, lexer, parser, type system
and Cranelift as its backend. It compiles the platform-independent translation
units of doomgeneric — 56,726 lines of C descended from id Software's Doom —
into `x86_64-unknown-toyos` objects in about four seconds, and that archive is
the `/bin/doom` in the desktop image. It also takes cases from TinyCC's own
`tests2` corpus all the way to running ToyOS processes, comparing each one's
output against TinyCC's expectations.

**Our own linker.** `toyos-ld` links everything that runs on ToyOS, plus
everything that runs before it: the UEFI bootloader as PE32+, the kernel and
every userland program as position-independent ELF. No LLVM linker touches
anything that boots. The largest thing it lays out is the Rust compiler itself,
built for ToyOS, in a single shared object.

**A real Rust target.** `x86_64-unknown-toyos` lives in ToyOS's fork of the
compiler, with a prebuilt `std` in the sysroot. One `rustc` invocation turns an
ordinary program using `HashMap`, threads and `println!` into a ToyOS binary.
Threads, thread-locals across `dlopen`, `catch_unwind`, `Drop` during unwind,
and backtraces symbolized to demangled names all work. The port changes nothing
in `core` or `alloc`.

**The Rust ecosystem, mostly unmodified.** The default boot image links a few
hundred third-party crates compiled for ToyOS, the large majority of them
exactly as published. Doom opens its window through `winit`, presents frames
through `softbuffer`, and plays through `cpal`; its music is General MIDI
rendered by `rustysynth`. The handful of crates that needed patches live as
`toyos` branches of their own repositories, never vendored — `forks.toml` is
the manifest, and `git log <base>..toyos` in any of them is exactly the ToyOS
delta.

**Booting on real hardware.** The first boot on a physical machine — a
ThinkPad T14 Gen 2, from a USB stick — reached CPU bring-up, x2APIC, I/O APIC,
ACPI, full PCI enumeration and NVMe identification in 590 ms, on the first
attempt.

![ToyOS booting on a ThinkPad T14](first-boot.jpg)

The screenshot is the laptop's own panel. The machine has no serial port, so
the kernel renders its log and its panics to the framebuffer, and pages them
with PageUp and PageDown polled straight off the keyboard controller after
every CPU has halted. It is reporting a real bug: the page cache sized an index
from the disk's block count, which fit in QEMU's test image and wanted 238 MB
on a 244 GB drive.

**Reading the stick it booted from.** On the ThinkPad, ToyOS's own xHCI and USB
mass-storage drivers enumerate the stick UEFI booted it from, parse its GPT,
and mount both FAT32 partitions. The running kernel then appends its log to a
file there, through its own FAT32 write path, so a machine with no serial port
can be read afterwards on any other computer. The GPT parser is `no_std`,
allocation-free and `forbid(unsafe_code)`, and answers exactly one question:
where is the partition with this GUID.

**A test suite that boots the OS, not a mock of it.** `cargo test` builds the
toolchain, kernel, bootloader and initrd, then boots the whole system across a
dozen concurrent QEMU guests — fast enough to run on every change, which is the
only property that matters. Many distinct machine shapes exist, because device
*shape* is what finds bugs: a device that is absent, one enumerated in a hostile
order, a 4Kn disk, two identical controllers, a port that flaps. Ground truth is
taken from the host side of the device, never from what the guest claims it did.
The audio
gate compares captured device output against a recorded baseline statistically,
and the panic-console tests decode the framebuffer glyph by glyph against the
same font the kernel blits.

## Prerequisites

- Rust, with rustup
- QEMU
- A C compiler on `PATH` as `cc`, and a Python 3

The last line is `rustc`'s and not ToyOS's, and nothing that boots touches it.
`rustc` links every **host** binary through `cc`, which rustup does not
install. And `rust/x`, the entry point to rustc's own bootstrap, is a shell
script whose whole job is to find a Python to run `bootstrap.py` with — so a
clean clone needs one, and so does every toolchain change.

Nothing in the OS goes near either. `bootloader/`, `kernel/` and `userland/`
all link with `toyos-ld`, and no image contains a C toolchain or a Python. On
macOS both arrive with the Xcode Command Line Tools; on Debian and Ubuntu they
are `build-essential` and `python3`.

`cargo run` names anything it needs and cannot find, before it does anything
else — including `df`, `ps` and `find`, which cost one feature each rather than
the build. The full accounting of everything this project depends on that it
did not write, with a verdict on each, is
`specs/assessments/dependency-audit-2026-08-08.md`.

Linux and macOS. Windows is a goal, not a claim — the build system still
assumes Unix in places, and nothing should be advertised until a clean Windows
machine proves `cargo run` works on it.

## How to run

```
cargo run                  # build everything, boot QEMU
cargo run -- --build-only  # build everything, boot nothing
cargo test                 # boot the OS and run the integration suite
```

The first run initializes submodules and bootstraps the custom Rust toolchain.
Later runs rebuild only what changed; a `std`-only change is a few seconds.

## Running it on real hardware

Every build produces a bootable disk image — GPT-partitioned, UEFI, ready to
write to a USB stick and boot on a physical machine.

```
cargo run -- --build-only                 # target/bootable.img
cargo run -- --diag-boot --build-only     # target/bootable-diag.img
cargo run -- --console-boot --build-only  # target/bootable-console.img
```

Three images rather than one, because a machine that misbehaves needs to be
asked different questions. Same kernel and same bootloader in all three; only
the boot configuration differs.

| Image | What it is |
|---|---|
| `bootable.img` | The whole system — compositor, daemons, desktop. What `cargo run` boots under QEMU, and the same bytes you flash. |
| `bootable-diag.img` | Nothing in it *can* claim the framebuffer, so the kernel's log and its panics stay readable on the panel. The only way to read a machine that wedges before userland, and on a laptop with no serial port the only way at all. |
| `bootable-console.img` | A shell on the raw framebuffer, no compositor. For asking the machine questions instead of reflashing it. |

### Writing it to a USB stick

The image is a whole disk, not a filesystem — write it to the device, not to a
partition on it. **Check the device name twice.** This destroys whatever is
already there.

macOS:

```
diskutil list                              # find the stick
diskutil unmountDisk /dev/diskN
sudo dd if=target/bootable.img of=/dev/rdiskN bs=4m
diskutil eject /dev/diskN
```

Linux:

```
lsblk                                      # find the stick
sudo dd if=target/bootable.img of=/dev/sdX bs=4M status=progress conv=fsync
sync
```

Then boot the machine from it. Secure Boot has to be off — ToyOS's bootloader
is signed by nobody.

The image carries two partitions: the EFI system partition the firmware boots
from, and a FAT32 one labelled `TOYOS-LOG`, where the kernel writes a log file
per boot named for the wall clock. It has its own partition for a mundane
reason — macOS will not auto-mount an EFI-typed one, so a log written there was
unreadable on the machine that needed to read it. Pull the stick after a boot
and the log is sitting there on any computer.

**Build a flashable image from a committed tree.** `cargo` builds your working
directory, and a checkout usually holds work in progress. An image flashed from
one is not a version of anything.

## Design

Read `CLAUDE.md` for the principles the codebase is held to, and `specs/` for
the design of each subsystem, the investigations behind the decisions, and the
list of everything currently known to be broken.

The short version: zero legacy, zero technical debt, fail fast, and prefer
making a mistake unrepresentable over checking for it at runtime.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Some of what the repository carries is not ours and is under other terms:
`userland/doom` is GPL-2.0, `assets/` holds a font, a set of icons, a wallpaper
and id Software's Doom shareware IWAD, `ovmf/` holds EDK II firmware builds,
and `tests/testcases/` holds TinyCC's test corpus. Third-party crates keep
their own upstream licenses. **[NOTICE](NOTICE) is the list**, item by item,
with the licence texts in [licenses/](licenses).

One of them constrains what you may do with a build: the shareware IWAD may be
redistributed freely but not sold and not modified, so **an image carrying it
may not be sold**. NOTICE says which images carry it.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project shall be dual licensed as above, without any
additional terms or conditions. No CLA.
