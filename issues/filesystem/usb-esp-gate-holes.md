---
status: open
kind: defect
opened: 2026-08-08
---

# Three of the six USB/ESP gate holes the teeth audit demonstrated are still open

The USB/ESP gate-teeth audit named each hole, gave the mutation that proved it,
and left a ranked work list of eleven items. It was written against `8d7044c`,
which `git rev-list --count 8d7044c..HEAD` puts 738 commits behind as of this
entry, and three of its six holes have closed since. What follows is that list
as it stands, so a reader does not re-investigate all six:

**Closed.**

- The ESP fsck gate's blindness to any value in the `..` entries of `/EFI` and
  `/toyos` — closed by rewrite. `tests/common/volumes.rs:36`
  now judges with `toyos-fat32-check`, which has `Complaint::DotCluster` /
  `DotDotCluster` / `DotInRoot` and derives from neither our writer nor our
  reader (`volumes.rs:35`), and the gate is **silence rather than sameness**: a non-empty complaint
  list is refused before the guest runs (`volumes.rs:275-281`) and after
  (`:342-348`). That is the audit's own ranked item 5.
- `tests/common/usb.rs`'s needle that could never fire — now
  `" designated, blocks="` at `usb.rs:292-297`, with a comment naming the old
  defect.
- `healthy=true` as an asserted constant — now
  `xhci::storage_online(self.index) == Some(true)` (`usb_storage.rs:73-75`) down
  to `MscDevice::online()` (`xhci/wait/msc.rs:102-104`).

**Open, unchanged.**

- **`usb_storage_gate`'s read half is certified by one in-guest comparator that
  nothing certifies.** `first_bad` (`kernel/src/usb_gate.rs:59-65`) is still the
  only comparator and `:118-131` the only verdict on the host's blocks; nothing
  prints a digest the harness could recompute. Audit ranked item 1, not built.
- **`xhci_no_interrupt`'s "nothing claimed a device" tooth passes on any absent
  line.** `tests/toyos.rs:6851-6857` is still `parse_xhci_binds(boot.text())`
  followed by `if !binds.is_empty()` — a negative over a parser, which a renamed
  log line satisfies vacuously.
- **The stamp guard has no test.** `usb_gate.rs:100-104` refuses a disk whose
  stamped block count disagrees with the device's, and `grep -rn "is stamped for" tests/`
  returns nothing: no profile stages a mis-stamped image.

Of the audit's eleven ranked items, 3, 5 and 10 are built — item 3 is
`Profile::UsbDiskCrowd` (`tests/common/qemu.rs:307`, shape at `:936`), which also
closed the harness gap that a `Shape` carried one disk triple (`usb_disks:
&'static [UsbDisk]` at `:641`). Items 1, 2, 4, 6, 7, 8, 9 and 11 are not; a
`usb-short-read` kernel feature now exists (`usb_gate.rs:147-155`,
`xhci/mod.rs:1818-1824`) which reaches a short read but not a failed device.
Nothing in this list is duplicated by `usb-mass-storage-not-implemented` — that
entry records driver gaps, these are test gaps.
