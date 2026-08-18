---
status: open
kind: track
opened: 2026-07-29
---

# The initrd is still the root filesystem, and it costs 85 MiB for the machine's life

The boot image was meant to become one bcachefs root partition on the boot
medium, with the initrd deleted. Every other part of that landed — symbolication
reads `.symtab` off the file that backs the executable rather than off a memory
image, one named `[profile.toyos]` with an overflow-checks gate, `hosted-rustc`
off, the asset sweep — and this part did not. `kernel/src/main.rs` still mounts
the initrd as root, the bootloader still hands over `initrd_addr`, and the image
still carries only `TOYOS-BOOT` and `TOYOS-LOG`.

Until it lands, the initrd's reserved region stays reserved for the machine's
whole life.

**Blocked on the page cache owning exactly one device**
(`issues/build/page-cache-owns-one-device.md`), which is also why a machine
booting off an internal disk gets neither `/boot` nor `/log`
(`issues/kernel/internal-disk-boot-has-no-boot-mount.md`). The work is therefore
a page-cache change first: a `BlockIO` over an arbitrary `BlockDevice` at a
partition offset with a cache of its own. Only then the third GPT partition, the
root GUID in the kernel arguments, and the deletion of the initrd adapter and
its slice-backed `BlockIO`. `issues/kernel/boot-image-split-r2-would-fail.md`
records what the refactor breaks as written.

Two facts worth keeping when the image is next re-costed:

- **No binary this project produces has a `.debug_*` section** — `toyos-ld`
  drops them — so `.symtab`/`.strtab`, 32.2 % of every shipped binary, *is* the
  whole debug weight and cannot be the part left behind.
- The boot medium is USB mass storage, which makes every `cargo test` the only
  coverage the USB storage path has. Swapping the dev loop to `virtio-blk`
  deletes that coverage silently.
