//! Scroll a known pattern past the console, so the host can say exactly what
//! the panel must show afterwards.
//!
//! Every line names its own number and ends in a marker, so a glyph that
//! survives from an earlier line is identifiable as such rather than merely
//! wrong.
//!
//! Three dimensions, all of them chosen because they produce cells a scroll
//! has to clear rather than overwrite:
//!
//! - **Length varies**, past the width of the panel as well as under it. The
//!   cells at risk are the ones past the end of a line that replaces a longer
//!   one, and a workload of uniform lines never produces any. A line wider
//!   than the panel also takes the soft-wrap path, which is the only way one
//!   logical line scrolls the screen more than once.
//! - **Batch size varies**, and drifts against the row count. A block flushed
//!   at once is many scrolls collapsed into a single paint — a path that only
//!   exists since the emulator stopped painting as it parsed — and the seed
//!   the console starts with is one batch of a thousand lines.
//! - **The last section is one batch**, which is that seed's shape.

use std::io::Write;

/// The body width for line `i`. Coprime stride so a long line lands on every
/// screen row over a run, and a range that crosses the panel's width so both
/// the wrapped and unwrapped paths are taken.
fn body_width(i: usize) -> usize {
    5 + (i * 37) % 500
}

fn line(i: usize) -> String {
    let fill = char::from(b'a' + (i % 26) as u8);
    let mid: String = std::iter::repeat(fill).take(body_width(i)).collect();
    format!("L{i:04} {mid} E{i:04}")
}

fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(400);
    // Lines per flush. Zero means one batch for the whole run, which is the
    // shape of the seed the console starts with: a thousand lines, most of
    // them wider than the panel, arriving as a single write.
    let chunk: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(7);
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(8 * 1024 * 1024, out.lock());

    for i in 0..count {
        writeln!(out, "{}", line(i)).expect("write");
        if chunk != 0 && i % chunk == 0 {
            out.flush().expect("flush");
        }
    }
    out.flush().expect("flush");

    println!("CHURN-DONE {count}");
}
