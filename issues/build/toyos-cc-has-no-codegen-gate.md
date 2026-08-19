---
status: open
kind: finding
opened: 2026-08-09
---

# toyos-cc's host suite asserts on four shapes of emitted code, and nothing else

Until 2026-08-09 it asserted on none. The eleven host tests were seven about
attribute refusals and four about determinism, and determinism compares one run
against another run and encodes no expected bytes at all. So the suite could
answer "did it compile" and "is it reproducible" and could not answer "is this
the program the source asked for".

Two miscompilations lived in that gap. A call through a static-local function
pointer dereferenced the callee's own address and jumped to whatever its first
eight bytes spelled; a candidate fix for an unrelated defect turned every
struct assignment into an eight-byte store of the *source's address*, and the
156-file corpus, the eleven host tests and the determinism suite were green
across it.

`toyos-cc/tests/emission.rs` closes those two by name: a struct assignment
reaches `memcpy` with the struct's size, a scalar assignment does not reach it
at all, an aggregate parameter is copied, and a static-local function pointer is
called with byte-identical code to a file-scope one. Four shapes.

What is not answered is whether that becomes a *gate* — a systematic
correspondence between source and emitted code, rather than four cases somebody
had a reason to write down. The corpus is the only broad instrument and it is a
proxy: it asks whether a program printed the right thing when it ran, which is
worth more than any assertion here and covers only what its 156 files happen to
do. Everything else this compiler emits is checked by nothing.
