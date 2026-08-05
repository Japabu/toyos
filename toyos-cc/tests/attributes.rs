//! What toyos-cc does with a GNU attribute it was handed. The layouts these
//! produce are checked by running them, in `tests/testcases/tinycc/201_*`;
//! what is here is the half a running program cannot show — which attributes
//! are refused, and what the refusal says.

use std::panic;

fn options() -> toyos_cc::CompileOptions {
    toyos_cc::CompileOptions {
        target: Some("x86_64-unknown-toyos".to_string()),
        ..Default::default()
    }
}

/// Compile `source`, returning the panic message if it was refused.
fn refusal(source: &str) -> Option<String> {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let source = source.to_string();
    let result = panic::catch_unwind(move || {
        toyos_cc::compile(&source, "attr.c", &options());
    });
    panic::set_hook(prev);
    result.err().map(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "<non-string panic>".to_string())
    })
}

fn accepts(source: &str) {
    if let Some(msg) = refusal(source) {
        panic!("expected {source:?} to compile, got: {msg}");
    }
}

fn refuses(source: &str, needle: &str) {
    let msg = refusal(source).unwrap_or_else(|| panic!("expected {source:?} to be refused"));
    assert!(msg.contains(needle), "refusal of {source:?} does not mention {needle:?}: {msg}");
}

#[test]
fn layout_attributes_are_applied_to_a_struct_or_union() {
    accepts("struct __attribute__((packed)) S { char a; int b; }; struct S s;");
    accepts("struct S { char a; int b; } __attribute__((packed)); struct S s;");
    accepts("typedef struct { char a; int b; } __attribute__((packed)) T; T t;");
    accepts("union __attribute__((packed)) U { char a; int b; }; union U u;");
    accepts("struct __attribute__((aligned(16))) A { int a; }; struct A a;");
    accepts("struct __attribute__((packed, aligned(4))) B { char a; int b; }; struct B b;");
    accepts("struct __attribute__((__packed__)) C { char a; int b; }; struct C c;");
}

#[test]
fn a_layout_attribute_outside_a_struct_definition_is_refused() {
    refuses("__attribute__((packed)) struct S { char a; int b; };", "a declaration specifier");
    refuses("struct S { char a; int b; }; struct S s __attribute__((packed));", "a declarator");
    refuses("int *__attribute__((aligned(8))) p;", "a pointer");
    refuses("void f(void) { l: __attribute__((packed)); }", "a label");
    refuses("enum __attribute__((packed)) E { X };", "an enum");
    refuses(
        "struct S { char a; int b; }; struct __attribute__((packed)) S s;",
        "`struct S`",
    );
    refuses("union U { char a; int b; }; union __attribute__((packed)) U u;", "`union U`");
}

#[test]
fn packed_bitfields_are_refused_rather_than_laid_out_wrong() {
    refuses(
        "struct __attribute__((packed)) S { int a : 3; int b : 30; }; struct S s;",
        "packed bitfield layout is not implemented",
    );
}

#[test]
fn aligned_takes_a_positive_power_of_two() {
    refuses("struct __attribute__((aligned(3))) S { int a; };", "power of two");
    refuses("struct __attribute__((aligned)) S { int a; };", "without an alignment");
}

#[test]
fn attributes_with_no_effect_here_are_accepted() {
    accepts("__attribute__((unused)) static int a;");
    accepts("__attribute__((__unused__)) static int a;");
    accepts("__attribute__((maybe_unused)) static int a;");
    accepts("__attribute__((noinline)) int f(void) { return 0; }");
    accepts("__attribute__((__noreturn__)) void f(void);");
    accepts("extern int f(const char *, ...) __attribute__((format(printf, 1, 2)));");
    accepts("extern void f(void) __attribute__((stdcall));");
    accepts("extern void f(void) __attribute__((fastcall));");
    accepts("extern void f(void) __attribute__((cdecl));");
    accepts("void f(void) { l: __attribute__((__unused__)); }");
    accepts("int f(void) { return 0; } int g(void) { return ((__attribute__((noinline)) int(*)(void))f)(); }");
}

#[test]
fn an_attribute_that_changes_behaviour_is_refused_by_name() {
    for (source, name) in [
        ("void f(void) { int x __attribute__((cleanup(g))); }", "cleanup"),
        ("__attribute__((constructor)) void f(void) {}", "constructor"),
        ("__attribute__((destructor)) void f(void) {}", "destructor"),
        ("extern int a __attribute__((alias(\"b\")));", "alias"),
        ("__attribute__((weak)) void f(void) {}", "weak"),
        ("int a __attribute__((section(\".foo\")));", "section"),
        ("struct __attribute__((ms_struct)) S { int a : 3; };", "ms_struct"),
        ("struct __attribute__((gcc_struct)) S { int a : 3; };", "gcc_struct"),
        ("typedef int i32 __attribute__((mode(SI)));", "mode"),
        ("__attribute__((always_inline)) int f(void) { return 0; }", "always_inline"),
        ("__attribute__((visibility(\"hidden\"))) int f(void) { return 0; }", "visibility"),
        ("int f(void) __attribute__((used));", "used"),
    ] {
        refuses(source, name);
        refuses(source, "is not implemented by toyos-cc");
    }
}

#[test]
fn compat_h_no_longer_strips_attributes() {
    let text = toyos_cc::preprocess_source(
        "__attribute__((packed)) __attribute((packed)) __declspec(align(8))",
        "attr.c",
        &options(),
        true,
    );
    for kept in ["__attribute__", "__attribute", "__declspec"] {
        assert!(text.contains(kept), "preprocessor swallowed {kept}: {text:?}");
    }
}
