---
status: open
kind: finding
opened: 2026-08-03
---

# `./x.py` typed by hand in `rust/` takes no lock, and bootstrap still recreates a directory it already has

`a8c78ef` took the fix this entry asked for and preferred against: the
bootstrap is now serialised across builders rather than made
re-entrant. `toolchain::ensure` decides under the shared build lock and runs
`x.py` under the exclusive one, so two builders' bootstraps cannot overlap and
neither can remove `stage1-std/<target>/dist/deps` while the other's `rustc` is
creating a temp file in it. Every observed instance came through the build
system, so the signature below should be gone.

**What is still reachable**: `./x.py build` typed by hand in `rust/`, which
takes no lock. If the signature reappears, that is the first thing to ask, and
the original preference — bootstrap not recreating a directory it already has —
is still the better fix for it. The record below is kept because recognising it
is the expensive part.

---

`69bca9a` removed the `rustup toolchain link` window — the symlink being
unlinked and recreated on every build, so a concurrent `rustc` proxy landing in
it died with `'rustc' is not installed for the custom toolchain 'toyos'`. That
fix is real and that signature should be gone. **It is not this one**, and the
risk is precisely that the link fix reads as having closed the class.

This window is inside the std bootstrap, and its signature is:

```
error: couldn't create a temp dir: No such file or directory (os error 2)
  at path "<repo>/rust/build/<host>/stage1-std/<target>/dist/deps/rmetaXXXXXX"
error: could not compile `core` (lib) due to 1 previous error
Build completed unsuccessfully in 0:00:43
thread 'main' panicked at src/toolchain.rs:215:5:
std rebuild failed
```

The target varies — seen on both `x86_64-unknown-toyos` and
`x86_64-unknown-none`, which is the tell that it is about the *directory* and
not about any one build. One builder's bootstrap removes and recreates
`stage1-std/<target>/dist/deps` while another's `rustc` is trying to create a
temp file inside it, so the loser dies compiling `core` — the first crate
through, which makes it look like a broken checkout rather than contention.

Recognising it: the path in the error **exists a moment later**. Listing it
after a failure showed `dist/deps` present with a fresh timestamp, because the
winner had finished recreating it. That asymmetry is the same one that
identified the link race (a probe succeeding between failures) and it is the
cheapest check.
