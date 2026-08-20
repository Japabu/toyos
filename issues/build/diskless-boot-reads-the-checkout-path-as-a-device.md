---
status: open
kind: defect
opened: 2026-08-20
---

# `diskless_boot` reds on a worktree whose path contains "nvme"

`tests/common/faults.rs`'s `diskless_boot` proves the absence of a controller
the only way absence can be proved — off the argv — and it does it with

```rust
if argv.iter().any(|a| a.contains("nvme")) {
```

Every element of the argv, including four filesystem paths that have nothing to
do with any device. Measured 2026-08-20 in a worktree at
`/Users/jan/Dev/jan/toyos-nvmewait`, where the whole failure is the two pflash
arguments:

```
FAIL diskless_boot: the diskless profile still has an NVMe device:
  [..., "-drive", "if=pflash,format=raw,unit=0,file=/Users/jan/Dev/jan/toyos-nvmewait/ovmf/OVMF_CODE-pure-efi.fd,readonly=on",
        "-drive", "if=pflash,format=raw,unit=1,file=/Users/jan/Dev/jan/toyos-nvmewait/ovmf/OVMF_VARS-pure-efi.fd,readonly=on",
        "-drive", "if=none,id=stick,format=raw,file=/nonexistent",
        "-device", "intel-iommu,...", "-device", "nec-usb-xhci,id=xhci",
        "-device", "usb-storage,bus=xhci.0,drive=stick,id=bootstick,bootindex=0", ...]
```

There is no NVMe device in that argv. `toyos-nvme`**wait** is what matched.

**It reproduces alone**, which is what makes it worth reading twice: the
harness's own `ALONE:` re-run said "red again, the same failure both times — the
defect is real", and it is real, about the harness rather than about the guest.
A path is not a device, and a substring test over one is the same class as
`msix::Unusable`'s and `Untrusted`'s: a value from outside the question being
compared against a needle nobody bounded. **CI never sees it** — the runner
checks out at a path with no `nvme` in it — so this is a red only an agent
working on NVMe can produce, at exactly the moment its own diff is the first
suspect.

The shape of the fix is the argv's own structure and not a better needle: QEMU's
device arguments are the values that follow `-device` and `-drive`, and it is
those the profile is claiming nothing about. `qemu::profile_argv` builds them, so
either the check walks the pairs or the builder answers "which devices did I
declare" directly. `class_function(&log, "0108")` in `tests/common/iommu.rs` is
the other way to ask — the guest's own PCI enumeration — and is immune to this
entirely.

Filed, not fixed: found while landing the NVMe operation deadline, whose branch
had no business touching the diskless profile's assertion.
