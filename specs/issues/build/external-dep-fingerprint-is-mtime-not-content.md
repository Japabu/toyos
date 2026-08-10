---
status: open
kind: finding
opened: 2026-08-10
---

# The external-dependency fingerprint keys on an mtime, so an identical rebuild cleans four target trees

`src/build.rs`'s `external_fingerprint` records, for every `.rlib`/`.rmeta` in
the sysroot and for the `toyos-ld` binary, a `name:len:mtime` triple. When it
differs from the `.deps-stamp` a crate target holds, `invalidate_stale` runs
`cargo clean` on `kernel/`, `bootloader/`, `userland/`, the Rust test-binary
crates and the `x86_64-unknown-toyos` half of every out-of-workspace program.

That is right in substance — cargo cannot see the sysroot, and a std that moved
under an unchanged `rustc -vV` is exactly the invisible staleness the stamp
exists to catch. What is wrong is the *key*: `toyos-ld` is rebuilt in every CI
job, from the same sources to the same bytes, and the rebuild alone changes the
mtime and cleans four target trees.

Measured, `specs/ci-plan.md` §12.5, run `31385467644`: **8 `external deps
changed: cleaning` lines in the warm arm as well as the cold one**, so a
restored `target/` for the kernel, the bootloader, userland and
`tests/toyos-rust-tests/*` is deleted the moment it arrives. The cache still
bought 237 s — every third-party crate — and this is what is left underneath it.

The repair is to fingerprint content rather than `len:mtime`, at least for the
linker binary. `toyos-ld` is a few MB and hashing it is milliseconds against a
clean it decides that is minutes long; the sysroot's rlibs are the expensive
half and do not need it, because the artifact CI unpacks is byte-identical and
`tar` restores mtimes, so their triple is already stable across jobs of one
toolchain tag.

**Why it is a finding and not a change made beside the cache.** It is not free:
`external_fingerprint` runs on every build on the dev host too, and the current
key errs towards cleaning, which is the safe direction. Making it more precise
means the clean stops happening in cases where it happens today, and the cases
have to be named before the change rather than after. The arm that settles it is
the same one the cache had — `probe-cache.yml`'s warm job, with the count of
`external deps changed: cleaning` lines it already prints, against a tree
carrying the content hash.
