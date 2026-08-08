---
status: open
kind: defect
opened: 2026-08-05
---

# A C test whose compilation fails is skipped, not red

`compile_c_tests` (`tests/toyos.rs`) wraps each compile in `catch_unwind` and
drops the ones that panic, printing a line to stderr and nothing else. Eleven of
the 121 discovered tests are in that state on 2026-08-05 — `78_vla_label`,
`79_vla_continue`, `83_utf8_in_identifiers`, `85_asm_outside_function`,
`89_nocode_wanted`, `94_generic`, `95_bitfields`, `95_bitfields_ms`,
`96_nodata_wanted`, `98_al_ax_extend`, `99_fastcall` — and none of them is in
`C_SKIP`, so nothing in the tree records that they are meant to be failing.

The consequence for anyone changing the compiler: a change that breaks a C test
*at compile time* moves it into this list rather than turning the suite red.
`82_attribs_position` did exactly that during the `__attribute__` work and only
the stderr line caught it. The check that works is to diff the skipped list
across the change; the fix would be to make the list a fixture the suite asserts
against.

`tests/testcases/pp_tcc/` (25 preprocessor cases with `.expect` files) is read by
nothing at all — `compile::testcases_dir()` returns only `tests/testcases/tinycc`
and no other Rust file mentions `pp_tcc`.
