---
status: open
kind: finding
opened: 2026-08-03
---

# `/log` is userland's to write, this boot's kernel log included

`/boot` had no permission model of any kind, and the attack `esp_files` still
replays is the proof: a guest binary running `fs::write("/boot/toyos/kernel.elf", "TEETH")`
truncated the kernel image to five bytes.

A mount now states whether userland may change it (`vfs::UserAccess`, given at
every `mount` call and defaulted nowhere) and `/boot` says no. The six syscalls
that can change a volume — `open` for write/create/truncate/append, `unlink`,
`rename`, `mkdir`, `rmdir`, `symlink` — ask `Vfs::user_may_modify` and answer
`PermissionDenied`; reads are untouched. `esp_files` runs the original attack
and each of the other five, and the host half of `esp_filesystem` reads the
build artifacts back out of the image the device received and requires them
byte-identical.

**Three residuals, in order of how much they cost.**

1. **`/log` is `ReadWrite`, `kernel.log` included.** A process can truncate the
   kernel's own log, or fill the volume. It is deliberate — `toybox`'s file
   tools write there, and the worst loss is the diagnostic rather than a
   machine that will not boot — but "the kernel's own volume is not userland's
   to write" is only half done while it holds.
2. **It is a mount-level policy, not a capability.** There is no way to say
   "this process may write `/boot`", so a future installer has nothing to ask
   for. `specs/capability-handles-spec.md` is where that lives.
3. **The FAT32 write path's guest-side coverage moved to `/log`** — same
   adapter, same driver, so nothing is lost, but `esp_files` no longer proves
   anything about writes reaching *the ESP*, because it may not make any.
