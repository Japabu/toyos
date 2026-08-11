//! The preprocessor's fatal errors, as seen by a caller.
//!
//! `toyos-cc/src` panics — 98 sites and no `-> Result` — and a caller that
//! wants to survive a bad translation unit catches the unwind. Three of these
//! errors called `process::exit` instead, which no caller can catch: the
//! harness compiles 156 files in one process and the first `#include
//! <stdatomic.h>` took the whole run down with no verdict.

mod common;

use common::refusal_named;

fn says(source: &str, filename: &str, needle: &str) {
    let msg = refusal_named(source, filename)
        .unwrap_or_else(|| panic!("expected {source:?} to be refused"));
    assert!(msg.contains(needle), "refusal of {source:?} does not mention {needle:?}: {msg}");
}

#[test]
fn hash_error_is_catchable_and_names_the_line() {
    says("int a;\n#error no thank you\n", "e.c", "#error no thank you");
    says("int a;\n#error no thank you\n", "e.c", "e.c:2");
}

#[test]
fn a_missing_quoted_include_is_catchable_and_names_the_file() {
    says("#include \"not-here.h\"\n", "q.c", "cannot find include file: not-here.h");
}

#[test]
fn a_missing_system_include_is_catchable_and_names_the_file() {
    says("#include <stdatomic.h>\n", "s.c", "cannot find system include file: stdatomic.h");
}
