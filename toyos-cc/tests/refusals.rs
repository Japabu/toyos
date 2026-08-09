//! The doors `__attribute__` refusals do not watch.
//!
//! `attributes.rs` is the same rule one level up: a construct that changes
//! layout or linkage and that toyos-cc does not implement is refused **by
//! name**, because dropping it produces a different program with no diagnostic
//! anywhere. Three constructs reached the back of the compiler and were thrown
//! away in silence — file-scope `asm(…)`, `asm("name")` after a declarator, and
//! `#pragma pack` — leaving a struct laid out differently from what the source
//! said, or a symbol undefined under a name nothing mentions.
//!
//! The asymmetry the pragma half has to respect: C99 6.10.6 *requires* an
//! unrecognised `#pragma` to be ignored, so a hard error on every unknown one
//! is itself a defect. Hence a named refusal list, a named inert list, and
//! silence for everything else.

mod common;

use common::{accepts, refuses};

#[test]
fn file_scope_asm_is_refused_rather_than_discarded() {
    refuses("asm(\".globl vide\\nvide: ret\\n\");", "file-scope asm");
    refuses("__asm__(\"nop\");", "file-scope asm");
    refuses("int a; asm(\"nop\"); int b;", "file-scope asm");
}

#[test]
fn a_declarator_rename_is_refused_rather_than_dropped() {
    refuses("int alias_name(void) asm(\"real_name\");", "asm(\"name\")");
    refuses("int a __asm__(\"b\");", "asm(\"name\")");
    refuses("void f(void) { int a __asm__(\"b\"); }", "asm(\"name\")");
}

/// Inline asm in a function body was never silent — it stops in codegen — and
/// stays here so the whole asm story is in one place.
#[test]
fn inline_asm_in_a_body_still_stops() {
    refuses("void f(void) { asm(\"nop\"); }", "inline asm");
}

#[test]
fn a_pragma_that_changes_layout_or_linkage_is_refused_by_name() {
    for (source, name) in [
        ("#pragma pack(push,1)\nstruct S { char a; int b; };", "pack"),
        ("#pragma pack(1)\nstruct S { char a; int b; };", "pack"),
        ("#pragma pack()\nstruct S { char a; int b; };", "pack"),
        ("#pragma ms_struct on\nstruct S { int a : 3; };", "ms_struct"),
        ("#pragma gcc_struct\nstruct S { int a : 3; };", "gcc_struct"),
        ("#pragma weak f\nvoid f(void) {}", "weak"),
        ("#pragma GCC visibility push(hidden)\nint f(void) { return 0; }", "GCC visibility"),
    ] {
        refuses(source, name);
        refuses(source, "is not implemented by toyos-cc");
    }
}

/// `_Pragma` is the operator spelling of the same directive, so a payload the
/// list above refuses must not get in through it.
#[test]
fn the_pragma_operator_is_refused_by_name() {
    refuses("_Pragma(\"pack(push,1)\")\nstruct S { char a; int b; };", "_Pragma");
    refuses("void f(void) { _Pragma(\"pack(1)\"); }", "_Pragma");
}

#[test]
fn a_pragma_with_no_effect_here_still_compiles() {
    accepts("#pragma GCC diagnostic ignored \"-Wall\"\nint a;");
    accepts("#pragma GCC diagnostic push\n#pragma GCC diagnostic pop\nint a;");
    accepts("#pragma GCC system_header\nint a;");
    accepts("#pragma once\nint a;");
    accepts("#pragma push_macro(\"X\")\n#pragma pop_macro(\"X\")\nint a;");
}

/// GNU's `#define f(x...)` bound `x` to the first argument and dropped every
/// argument after it. That is the same silence, one directive along.
#[test]
fn a_named_variadic_macro_parameter_is_refused_by_name() {
    refuses("#define F(y...) y\nint a = F(1, 2);", "named variadic parameter");
    accepts("#define G(y, ...) y\nint a = G(1, 2);");
    accepts("#define H(...) 0\nint a = H(1, 2);");
}

/// C99 6.10.6 requires this one, and it is the reason the refusal is a list of
/// names rather than a default.
#[test]
fn an_unrecognised_pragma_is_ignored() {
    accepts("#pragma nobody_has_ever_heard_of_this\nint a;");
    accepts("#pragma comment(option, \"-Wall\")\nint a;");
    accepts("#pragma STDC FP_CONTRACT OFF\nint a;");
}
