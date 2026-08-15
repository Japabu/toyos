//! Two processes, two `write`s per line, and not one line that belongs to
//! both.
//!
//! **The defect this is aimed at is a `write` syscall being the unit of
//! interleaving.** `println!` is a `LineWriter`: it issues `flush_buf()` and
//! then `inner.write(rest)`, so a line reaches the kernel in two pieces with an
//! arbitrary gap between them, and anything else writing the console in that
//! gap lands inside the line.
//! Two splices were recorded against it before it was closed, one of them a
//! measured 1 run in 10 for `desktop_audio_client` on CI;
//! `specs/log-architecture-spec.md` §4.4 keeps both measurements, because the
//! issue file that held them is closed and its numbers are still numbers.
//! `ConsoleObject`'s line buffer is what closes it
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

/// Bytes the third writer says, in two `write`s, never ending them with a
/// newline.
///
/// **The other half of the same buffer.** A line leaves on the `\n` that ends
/// it; the one moment a partial line stops being "not finished yet" and becomes
/// "all there will ever be" is the last handle to the console going away, and
/// `ConsoleObject::drop` is what flushes it (`specs/log-architecture-spec.md`
/// §4.4). Without that flush these bytes are dropped on the floor — a buffer
/// that loses a dying process's last words, which is the opposite of what it is
/// for — and a hundred of them arriving whole is also proof the buffer
/// accumulated across two `write`s to get there.
const MIDLINE: usize = 100;

/// The byte the third writer repeats. Not `A` or `B`, so the whole-line count
/// cannot see it, and a hundred of them in a row is nothing an ordinary console
/// line contains.
const MIDLINE_BYTE: u8 = b'C';

/// Stdout, by the slot every process starts with.
const STDOUT: RawHandle = RawHandle(1);

fn main() {
    let mut args = std::env::args();
    let _ = args.next();
    match args.next().as_deref() {
        Some("C") => exit_mid_line(),
        Some(tag) => {
            let byte = tag.as_bytes().first().copied().unwrap_or(b'?');
            write_lines(byte);
        }
        None => parent(),
    }
}

/// Say half of something, say the other half, and exit without ever ending it.
///
/// No newline anywhere, so nothing in the write path puts these bytes on the
/// wire: what does is this process exiting.
fn exit_mid_line() {
    let partial = [MIDLINE_BYTE; MIDLINE];
    let (head, tail) = partial.split_at(MIDLINE / 2);
    for piece in [head, tail] {
        match syscall::write(STDOUT, piece) {
            Ok(n) if n == piece.len() => {}
            other => {
                eprintln!(
                    "console-atomicity: the mid-line writer wrote {other:?} of {}",
                    piece.len()
                );
                exit(1);
            }
        }
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
    // **The mid-line writer runs after the other two are gone**, so the bytes
    // its exit flushes cannot land inside a line the count above is reading —
    // they are on the wire on their own, which is what lets the host look for
    // them as a run rather than as a line.
    match Command::new(SELF_PATH).arg("C").spawn().and_then(|mut c| c.wait()) {
        Ok(status) if status.code() == Some(0) => {}
        Ok(status) => {
            eprintln!("console-atomicity: the mid-line writer exited {:?}", status.code());
            exit(1);
        }
        Err(e) => {
            eprintln!("console-atomicity: the mid-line writer would not run: {e}");
            exit(1);
        }
    }
    // After all three, so the count the host checks against is a claim about a
    // run that finished rather than one still going. It follows the mid-line
    // writer's unterminated bytes on the wire, which is why the host finds this
    // declaration with a substring search rather than a whole-line one.
    println!("console-atomicity: writers=2 lines={LINES} width={WIDTH} midline={MIDLINE}");
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
