---
status: open
kind: defect
opened: 2026-08-05
---

# `tests/testcases/pp_tcc/` is read by nothing

Twenty-five preprocessor cases with `.expect` files, committed and attributed in
`tests/testcases/LICENSE`, and named in no `.rs` anywhere in the tree —
`compile::testcases_dir()` returns only `tests/testcases/tinycc`.

The other half of this entry is closed. A C test whose compilation failed used
to be dropped with a line to stderr and nothing else; `NOT_RUN`
(`tests/toyos.rs`) is now one declared list over the whole corpus, every entry
attempted to its declared stage on every run, and a discovered case that stops
building reds the run by name.
