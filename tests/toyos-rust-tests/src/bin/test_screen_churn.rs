//! Scroll a known pattern past the console, so the host can say exactly what
//! the panel must show afterwards.
//!
//! Every line names its own number and ends in a marker, so a glyph that
//! survives from an earlier line is identifiable as such rather than merely
//! wrong.
//!
//! Four dimensions, all of them chosen because they produce cells a scroll has
//! to clear rather than overwrite:
//!
//! - **Length varies**, past the width of the panel as well as under it. The
//!   cells at risk are the ones past the end of a line that replaces a longer
//!   one, and a workload of uniform lines never produces any. A line wider
//!   than the panel also takes the soft-wrap path, which is the only way one
//!   logical line scrolls the screen more than once.
//! - **`span` is the caller's**, because only the caller knows the panel. At
//!   two panel widths, [`STRIDE`] coprime with the column count makes any
//!   `span / 2` consecutive lines end in every column of the panel exactly
//!   once — which is the coverage the at-risk cells live on, and a property
//!   rather than a hope.
//! - **Batch size varies**, and drifts against the row count. A block flushed
//!   at once is many scrolls collapsed into a single paint — a path that only
//!   exists since the emulator stopped painting as it parsed.
//! - **Where the run starts is the caller's**, so several runs against one
//!   console walk disjoint stretches of the sequence instead of repeating its
//!   opening at a different alignment.

use std::io::Write;

/// Coprime with the column count of every panel this runs on, so consecutive
/// lines never share a length and a long line lands on every screen row over a
/// run.
const STRIDE: usize = 37;

fn body_width(i: usize, span: usize) -> usize {
    5 + (i * STRIDE) % span
}

fn line(i: usize, span: usize) -> String {
    let fill = char::from(b'a' + (i % 26) as u8);
    let mid: String = std::iter::repeat(fill).take(body_width(i, span)).collect();
    format!("L{i:04} {mid} E{i:04}")
}

/// Every argument is required: the caller is a test that knows all four, and a
/// default here would be a second workload nobody reads.
fn arg(n: usize) -> usize {
    std::env::args()
        .nth(n)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("usage: test_screen_churn <start> <count> <chunk> <span>"))
}

fn main() {
    let start = arg(1);
    let count = arg(2);
    // Lines per flush. Zero means one batch for the whole run.
    let chunk = arg(3);
    let span = arg(4);
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(8 * 1024 * 1024, out.lock());

    for i in start..start + count {
        writeln!(out, "{}", line(i, span)).expect("write");
        if chunk != 0 && (i - start) % chunk == 0 {
            out.flush().expect("flush");
        }
    }
    out.flush().expect("flush");

    println!("CHURN-DONE {start} {count}");
}
