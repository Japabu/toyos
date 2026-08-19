---
status: open
kind: defect
opened: 2026-08-11
---

# `userland/libc`'s `stat` cannot see a directory at all, and its `lstat` claims there are no symlinks

`stat_impl` (`userland/libc/src/posix_io.rs`) is `open` + `fstat` + `close` and
nothing else. `syscall::open` refuses every directory — empty or occupied — so a
C program's `stat("/bin")` fails where std's `stat` falls back to `readdir` and
answers. `access()` a few lines below has the same shape and the same hole.

`lstat` is `stat` under a comment saying ToyOS has no symlinks. It has them:
`SYS_SYMLINK` and `SYS_READLINK` are real, `TmpFs::create_symlink` implements
them, and `std_fs` creates one and asks `symlink_metadata` about it. So `lstat`
follows a link it is defined not to follow, and the comment excusing that is the
reason nobody looked.

Nothing in the tree noticed because the toybox tools are Rust and reach the
kernel through std. It is the C side — tinycc, doomgeneric — that would, and it
would look like a missing file rather than a missing feature.

Found while landing the std half of the same question
(`sys::fs::toyos::stat` grew the `readdir` fallback that makes an empty
directory stat as one); this is the layer that never had it.
