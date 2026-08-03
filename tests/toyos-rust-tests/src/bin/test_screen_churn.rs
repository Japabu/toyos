//! Scroll a known pattern past the console, so the host can say exactly what
//! the panel must show afterwards.
//!
//! Every line names its own number and ends in a marker, so a glyph that
//! survives from an earlier line is identifiable as such rather than merely
//! wrong.
//!
//! The cells at risk are the ones past the end of a line that replaces a
//! longer one, so the workload is built to produce them densely and at every
//! column rather than to print a great many lines and hope. A line is a sweep
//! of the panel's width plus a whole number of extra panels:
//!
//! - **[`STRIDE`] is coprime with `cols`**, so any `cols` consecutive lines
//!   end in every column of the panel exactly once. That is the column
//!   coverage, and it is a property of the construction rather than of how
//!   long the run is.
//! - **[`WRAPS`] carries the extra panels**, so every eight lines hold one
//!   that fits a row, one that wraps once and one that wraps twice. A short
//!   run cannot lose the soft-wrap path — which a plain sweep of two panel
//!   widths does, silently, because a partial period of it never reaches its
//!   own top.
//! - **Consecutive lines differ in length**, and 41% of them are shorter than
//!   the one before: that transition is what leaves cells behind.
//! - **Batch size varies** and drifts against the row count. A block flushed
//!   at once is many scrolls collapsed into a single paint.
//! - **Where the run starts is the caller's**, so several runs against one
//!   console walk disjoint stretches instead of repeating the opening at a
//!   different alignment.

use std::io::Write;

/// Coprime with the column count of every panel this runs on.
const STRIDE: usize = 37;

/// Extra panel widths per line, cycled. Mean 0.5, so the average line is a
/// panel and a half — the run is not paid for twice over to reach a case that
/// one line in eight already reaches.
const WRAPS: [usize; 8] = [0, 1, 0, 2, 0, 1, 0, 0];

fn body_width(i: usize, cols: usize) -> usize {
    5 + (i * STRIDE) % cols + cols * WRAPS[i % WRAPS.len()]
}

fn line(i: usize, cols: usize) -> String {
    let fill = char::from(b'a' + (i % 26) as u8);
    let mid: String = std::iter::repeat(fill).take(body_width(i, cols)).collect();
    format!("L{i:04} {mid} E{i:04}")
}

/// Every argument is required: the caller is a test that knows all four, and a
/// default here would be a second workload nobody reads.
fn arg(n: usize) -> usize {
    std::env::args()
        .nth(n)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("usage: test_screen_churn <start> <count> <chunk> <cols>"))
}

fn main() {
    let start = arg(1);
    let count = arg(2);
    // Lines per flush. Zero means one batch for the whole run.
    let chunk = arg(3);
    let cols = arg(4);
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(8 * 1024 * 1024, out.lock());

    for i in start..start + count {
        writeln!(out, "{}", line(i, cols)).expect("write");
        if chunk != 0 && (i - start) % chunk == 0 {
            out.flush().expect("flush");
        }
    }
    out.flush().expect("flush");

    println!("CHURN-DONE {start} {count}");
}
