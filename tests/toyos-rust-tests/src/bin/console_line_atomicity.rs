//! Two processes, two `write`s per line, and not one line that belongs to
//! both.
//!
//! **The defect this is aimed at is a `write` syscall being the unit of
//! interleaving.** `println!` is a `LineWriter`: it issues `flush_buf()` and
//! then `inner.write(rest)`, so a line reaches the kernel in two pieces with an
//! arbitrary gap between them, and anything else writing the console in that
//! gap lands inside the line.
//! `specs/issues/diagnostics/serial-console-has-no-line-atomicity.md` has four
//! recorded splices and a measured 1 run in 10 for `desktop_audio_client` on
//! CI. `ConsoleObject`'s line buffer is what closes it
//! (`specs/log-architecture-spec.md` §4.4), and the buffer is per holder — so
//! two *processes* is the shape that tests it and two threads would not.
//!
//! The two writes are made by hand rather than through `println!` because the
//! split has to be the subject rather than an implementation detail of `std`:
//! the line is a fixed width, the gap is exactly in the middle, and the newline
//! is on the second write, which is the piece the buffer is waiting for.
//!
//! The verdict is the host's — a line is mixed or it is not, and only the
//! console capture can say. What this binary owes the host is that both writers
//! ran, said how much, and agreed about it.

use std::process::{exit, Command};

use toyos_abi::syscall;
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_console_line_atomicity";

/// Lines each writer emits.
///
/// **A count and not a duration**, so the gate's verdict does not move with the
/// host: the assertion is zero mixed lines out of `2 * LINES`, and a run that
/// produced fewer lines than that has failed the non-vacuity check rather than
/// passed a weaker version of the same test. A thousand each is two thousand
/// chances for a splice against a defect measured at one boot in ten.
const LINES: usize = 1000;

/// Bytes in one line, newline included.
///
/// Two hundred, which is comfortably inside `MAX_CONSOLE_LINE`'s 1024 — the
/// claim under test is that a whole line is one unit, and a line past that
/// bound is deliberately emitted in pieces of it, which is a different
/// sentence.
const WIDTH: usize = 200;

/// Stdout, by the slot every process starts with.
const STDOUT: RawHandle = RawHandle(1);

fn main() {
    let mut args = std::env::args();
    let _ = args.next();
    match args.next() {
        Some(tag) => {
            let byte = tag.as_bytes().first().copied().unwrap_or(b'?');
            write_lines(byte);
        }
        None => parent(),
    }
}

/// Spawn the two writers and wait for both.
///
/// Two processes and not two threads: the buffer is per console object and a
/// process gets its own, so two threads of one process share one buffer and
/// would prove nothing about the property the object exists to have.
fn parent() {
    let mut children = Vec::new();
    for tag in ["A", "B"] {
        match Command::new(SELF_PATH).arg(tag).spawn() {
            Ok(child) => children.push((tag, child)),
            Err(e) => {
                eprintln!("console-atomicity: writer {tag} would not start: {e}");
                exit(1);
            }
        }
    }
    for (tag, mut child) in children {
        match child.wait() {
            Ok(status) if status.code() == Some(0) => {}
            Ok(status) => {
                eprintln!("console-atomicity: writer {tag} exited {:?}", status.code());
                exit(1);
            }
            Err(e) => {
                eprintln!("console-atomicity: writer {tag} would not be waited for: {e}");
                exit(1);
            }
        }
    }
    // After both, so the count the host checks against is a claim about a run
    // that finished rather than one still going.
    println!("console-atomicity: writers=2 lines={LINES} width={WIDTH}");
}

/// One writer: `LINES` lines of one repeated byte, each in two `write`s.
fn write_lines(tag: u8) {
    let mut line = [tag; WIDTH];
    line[WIDTH - 1] = b'\n';
    let (head, tail) = line.split_at(WIDTH / 2);
    for _ in 0..LINES {
        // Refused rather than retried: a short write here is the kernel taking
        // half a line, which is the defect and not an error to paper over.
        // `try_write`'s console arm accepts the whole buffer by construction.
        for piece in [head, tail] {
            match syscall::write(STDOUT, piece) {
                Ok(n) if n == piece.len() => {}
                other => {
                    eprintln!(
                        "console-atomicity: writer {} wrote {other:?} of {}",
                        tag as char,
                        piece.len()
                    );
                    exit(1);
                }
            }
        }
    }
}
