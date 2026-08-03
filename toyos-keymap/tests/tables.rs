//! Every level of every key of every layout, against a checked-in dump.
//!
//! A golden file rather than assertions per key: the tables are data, and the
//! thing worth gating is that none of it moves without someone saying so. The
//! dump for `us`, `de` and `swiss-german-mac` was diffed against the tables as
//! they stood in `kernel/src/keyboard.rs` before this crate existed, so the
//! file is a record of unchanged behaviour and not just of current behaviour.

use toyos_keymap::{Key, FIRST_USAGE, ISO_USAGE, LAST_USAGE, LAYOUTS, LEVELS};

fn cell(k: Key) -> String {
    match k {
        Key::None => "<none>".to_string(),
        Key::Dead(d) => format!("<dead {d:?}>"),
        Key::Chars("\r") => "<0d>".to_string(),
        Key::Chars("\x1b") => "<1b>".to_string(),
        Key::Chars("\x08") => "<08>".to_string(),
        Key::Chars("\t") => "<09>".to_string(),
        Key::Chars(s) => s.to_string(),
    }
}

fn row(name: &str, label: String, e: &toyos_keymap::KeyEntry) -> String {
    let levels: Vec<String> = (0..LEVELS).map(|i| cell(e.level(i))).collect();
    format!("{name}\t{label}\t{}\n", levels.join("\t"))
}

pub fn dump() -> String {
    let mut out = String::new();
    for layout in LAYOUTS {
        out.push_str(&row(layout.name, "iso".to_string(), &layout.iso_key));
        for usage in FIRST_USAGE..=LAST_USAGE {
            let e = layout.entry(usage).expect("in range");
            out.push_str(&row(layout.name, usage.to_string(), e));
        }
        assert!(layout.entry(ISO_USAGE).is_some(), "{} has no ISO key", layout.name);
    }
    out
}

#[test]
fn golden() {
    let actual = dump();
    let want = include_str!("layouts.golden");
    if actual != want {
        let path = std::env::temp_dir().join("toyos-keymap-layouts.actual");
        std::fs::write(&path, &actual).expect("write actual dump");
        panic!(
            "layout tables changed; if that is intended, copy {} over tests/layouts.golden",
            path.display()
        );
    }
}
