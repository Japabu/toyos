# Plan: Dynamic Syscall Library (`libtoyos.so`)

Move all syscall implementations into a dynamically linked `libtoyos.so` that exposes a clean,
typed C API. Std patches become thin wrappers. Syscall numbers and asm live only in the library.

```
Before:  userland → std (inline asm + syscall numbers) → kernel
After:   userland → std (extern "C" calls) → libtoyos.so (asm + numbers) → kernel
```

## Step 1: Create `libtoyos/` crate

`#![no_std]` cdylib exporting typed `extern "C"` functions for every syscall:

```
toyos_write(buf, len) -> isize            toyos_open(path, path_len, flags) -> u64
toyos_read(buf, len) -> isize             toyos_close(fd)
toyos_alloc(size, align) -> *mut u8       toyos_read_file(fd, buf, len) -> u64
toyos_free(ptr, size, align)              toyos_write_file(fd, buf, len) -> u64
toyos_realloc(ptr, sz, align, new) -> *   toyos_seek(fd, offset, whence) -> u64
toyos_exit(code) -> !                     toyos_fstat(fd) -> u64
toyos_exec(path, len, out, out_len) -> u64  toyos_fsync(fd)
toyos_random(buf, len)
toyos_clock() -> u64
toyos_screen_size() -> u64
```

Each wraps the raw `syscall` asm internally. Syscall numbers are private constants.
Minimal `#[panic_handler]`, no allocator.

## Step 2: Update target spec

`toolchain/patches/.../base/toyos.rs`: `dynamic_linking: true` (was `false`).
PIC/PIE/rust-lld already configured.

## Step 3: Update std patches

**`sys/mod.rs`** — Remove inline asm `syscall()`. Add `#[link(name = "toyos")]` extern block
declaring all typed imports.

**`sys/stdio/toyos.rs`** — `Stdin::read` calls `toyos_read()`, `Stdout::write` calls
`toyos_write()`. Remove broken `read_buf` override (default calls `self.read()`, fixing
`stdin().read_line()`). Remove `SYS_READ`/`SYS_WRITE` constants.

**`sys/alloc/toyos.rs`** — Call `toyos_alloc/free/realloc`. Remove `SYS_ALLOC/FREE/REALLOC`.

**`sys/random/toyos.rs`** — Call `toyos_random`. Remove `SYS_RANDOM`.

**`sys/fs/toyos.rs`** — Call `toyos_open/close/read_file/write_file/seek/fstat/fsync`.
Remove all `SYS_*` constants and the local `syscall()` wrapper.

**`sys/process/toyos.rs`** — Call `toyos_exec`. Remove `SYS_EXEC` and local wrapper.

**`sys/entry.rs`** — Call `toyos_exit`.

**`pal/unsupported/time.rs`** — Call `toyos_clock`.

**`pal/unsupported/os.rs`** — Call `toyos_exit`.

No syscall numbers or asm remain in any std patch.

## Step 4: Rebuild toolchain (one-time)

`cd toolchain && cargo run`. `#[link]` is just metadata in rlibs — `libtoyos.so`
is NOT needed at this stage, only when linking the final userland binary.

## Step 5: Build `libtoyos.so`, install to sysroot

Build with `cargo +toyos build --target x86_64-unknown-toyos` in `libtoyos/`.
Copy `.so` to sysroot: `<stage2>/lib/rustlib/x86_64-unknown-toyos/lib/`.
Automate in `toolchain/src/main.rs` after the toolchain link step.

## Step 6: Update `bootable/build.rs`

Copy `libtoyos.so` to initrd so the kernel can load it at runtime.

## Step 7: Extend ELF loader for dynamic linking

In `kernel/src/elf.rs`:

**New DT_* tags**: DT_NEEDED(1), DT_SYMTAB(6), DT_STRTAB(5), DT_STRSZ(10),
DT_SYMENT(11), DT_JMPREL(23), DT_PLTRELSZ(2).

**New relocation types**: R_X86_64_GLOB_DAT(6), R_X86_64_JUMP_SLOT(7).

**Algorithm**:
1. Parse program's DT_NEEDED → library name from DT_STRTAB
2. Load library from `/initrd/<name>` via VFS (parse ELF, alloc, copy segments, RELATIVE relocs, map_user)
3. Build name→address map from library's `.dynsym` + `.dynstr`
4. Process main binary's DT_RELA + DT_JMPREL: for GLOB_DAT/JUMP_SLOT, look up symbol → write resolved address into GOT

Eager binding only. Linear `.dynsym` search (~17 symbols). No lazy PLT, no transitive deps.

## Step 8: Update `input-test`

Replace raw syscall `read_line_raw()` with `stdin().lock().read_line()` to validate the chain.

## Files

| File | Action |
|------|--------|
| `libtoyos/Cargo.toml` | Create |
| `libtoyos/src/lib.rs` | Create: all typed syscall wrappers |
| `toolchain/patches/.../base/toyos.rs` | `dynamic_linking: true` |
| `toolchain/patches/.../sys/mod.rs` | extern block with typed imports |
| `toolchain/patches/.../stdio/toyos.rs` | toyos_read/write, drop read_buf |
| `toolchain/patches/.../alloc/toyos.rs` | toyos_alloc/free/realloc |
| `toolchain/patches/.../random/toyos.rs` | toyos_random |
| `toolchain/patches/.../fs/toyos.rs` | toyos_open/close/read_file/etc |
| `toolchain/patches/.../process/toyos.rs` | toyos_exec |
| `toolchain/patches/.../entry.rs` | toyos_exit |
| `toolchain/patches/.../pal/unsupported/time.rs` | toyos_clock |
| `toolchain/patches/.../pal/unsupported/os.rs` | toyos_exit |
| `toolchain/src/main.rs` | build libtoyos + copy to sysroot |
| `kernel/src/elf.rs` | dynamic linking (DT_NEEDED, GLOB_DAT, JUMP_SLOT) |
| `bootable/build.rs` | copy libtoyos.so to initrd |
| `userland/input-test/src/main.rs` | stdin().read_line() |
