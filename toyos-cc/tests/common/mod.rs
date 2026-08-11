//! Driving the library from a host test, and reading back what it said.
//!
//! Shared because the alternative is a second copy of `refusal` that drifts
//! from the first.

#![allow(dead_code)]

use std::panic;

pub fn options() -> toyos_cc::CompileOptions {
    toyos_cc::CompileOptions {
        target: Some("x86_64-unknown-toyos".to_string()),
        ..Default::default()
    }
}

/// Compile `source`, returning the panic message if it was refused.
pub fn refusal(source: &str) -> Option<String> {
    refusal_named(source, "attr.c")
}

/// The same, for a case whose diagnostic quotes the file name.
pub fn refusal_named(source: &str, filename: &str) -> Option<String> {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let source = source.to_string();
    let filename = filename.to_string();
    let result = panic::catch_unwind(move || {
        toyos_cc::compile(&source, &filename, &options());
    });
    panic::set_hook(prev);
    result.err().map(|e| panic_message(&e))
}

pub fn panic_message(e: &Box<dyn std::any::Any + Send>) -> String {
    e.downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_string())
}

pub fn accepts(source: &str) {
    if let Some(msg) = refusal(source) {
        panic!("expected {source:?} to compile, got: {msg}");
    }
}

pub fn refuses(source: &str, needle: &str) {
    let msg = refusal(source).unwrap_or_else(|| panic!("expected {source:?} to be refused"));
    assert!(msg.contains(needle), "refusal of {source:?} does not mention {needle:?}: {msg}");
}

/// Compile to object bytes, panicking with the compiler's own message on refusal.
pub fn object(source: &str) -> Vec<u8> {
    toyos_cc::compile(source, "emit.c", &options())
}
