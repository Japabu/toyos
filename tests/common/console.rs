//! The console's line atomicity, counted rather than sampled.
//!
//! `specs/log-architecture-spec.md` §4.4 and §9.5. The guest runs two writers
//! that each build a fixed-width line out of two `write` syscalls; this file
//! reads the console capture and counts the lines that belong to both of them.
//! The verdict is **zero**, not a probability — the mechanism under test is a
//! per-holder line buffer in the kernel, and a buffer that works works every
//! time.
//!
//! Three assertions and not one, because they catch different halves of the
//! same mechanism: the count catches one userland writer inside another's line;
//! `Serial::interleaved` catches a *kernel* record inside a userland line,
//! which is what both recorded splices were made of (§4.4 keeps their
//! measurements — the issue file that held them is closed); and the run of
//! unterminated bytes catches the buffer failing to flush what a process that
//! exited mid-line had said, which is the one case a newline never reaches.

use std::path::Path;
use std::time::Duration;

use super::qemu::{BootOptions, QemuInstance};
use super::serial::Serial;

/// The guest binary's name in the `run <name>` protocol.
const WRITER: &str = "test_rs_console_line_atomicity";

/// A liveness guard and never the verdict: two thousand 200-byte lines is a
/// fraction of a second of virtio-console, and this only catches a guest that
/// stopped answering.
const CEILING: Duration = Duration::from_secs(60);

/// What the guest's binary declares, so the host is not carrying a second copy
/// of the numbers.
struct Declared {
    writers: usize,
    lines: usize,
    width: usize,
    /// Bytes the third writer said in two `write`s and never ended with a
    /// newline, which only its own exit can put on the wire.
    midline: usize,
}

pub fn console_line_atomicity(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // **Two CPUs, because one writer preempting another is the stimulus.** At
    // `--smp 1` the two processes still interleave — they are preempted, not
    // parallel — but at two the gap between one writer's two `write`s can be
    // filled by a genuinely concurrent one, which is the harder case and the
    // one a laptop has.
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions { smp: 2, ..Default::default() },
    );
    let result = qemu.run_test(WRITER, CEILING);
    if let Some(err) = &result.error {
        return Err(format!("{err}\nstdout:\n{}", tail(&result.stdout)));
    }
    if result.exit_code != Some(0) {
        return Err(format!(
            "the writers exited {:?}\n{}",
            result.exit_code,
            tail(&result.stdout)
        ));
    }
    let declared = declared(&result.stdout)?;

    let mut pure = [0usize; 2];
    let mut mixed: Vec<&str> = Vec::new();
    let mut short: usize = 0;
    for line in result.stdout.lines() {
        let a = line.bytes().filter(|b| *b == b'A').count();
        let b = line.bytes().filter(|b| *b == b'B').count();
        // A writer's line is one repeated byte and nothing else, so a line that
        // is mostly one tag is one of its lines however it ended up.
        let (tag, other) = if a >= b { (a, b) } else { (b, a) };
        if tag * 2 < declared.width - 1 {
            continue; // not a writer's line at all
        }
        if other > 0 {
            mixed.push(line);
            continue;
        }
        if tag != declared.width - 1 {
            short += 1;
            continue;
        }
        pure[usize::from(b > a)] += 1;
    }

    if !mixed.is_empty() {
        let sample: Vec<String> = mixed
            .iter()
            .take(3)
            .map(|l| l.chars().take(80).collect::<String>())
            .collect();
        return Err(format!(
            "{} of {} console lines carry both writers' bytes — a `write` syscall is still the \
             unit of interleaving, so half a line reaches the backend and another process's \
             half follows it. First three, truncated to 80 columns:\n{}",
            mixed.len(),
            declared.writers * declared.lines,
            sample.join("\n")
        ));
    }
    if short != 0 {
        return Err(format!(
            "{short} console lines are a writer's bytes at the wrong width; a whole line is one \
             unit and these were cut"
        ));
    }
    // Non-vacuity: a capture that lost the writers' output entirely would count
    // zero mixed lines and prove nothing.
    for (i, tag) in ["A", "B"].iter().enumerate() {
        if pure[i] != declared.lines {
            return Err(format!(
                "writer {tag} declared {} whole lines and the capture carries {}",
                declared.lines, pure[i]
            ));
        }
    }
    // **The buffer's other half: a process that exits mid-line.** The third
    // writer says `midline` bytes in two `write`s, ends them with nothing and
    // exits; the only thing that can put them on the wire is
    // `ConsoleObject::drop` flushing what the last handle left behind (§4.4).
    // A tree without that flush loses them silently, which is a buffer that
    // drops a dying process's last words — so the assertion is the run's
    // *length*, and it is exact on both sides: shorter means bytes were lost,
    // longer means something else was acquired inside them.
    let longest = result
        .stdout
        .split(|c| c != 'C')
        .map(str::len)
        .max()
        .unwrap_or(0);
    if longest != declared.midline {
        return Err(format!(
            "a process exited having written {} unterminated bytes and the longest run of them on \
             the console is {longest} — the last handle to a console going away is what turns a \
             partial line into all there will ever be, and this capture says it went nowhere",
            declared.midline
        ));
    }

    // The kernel-into-userland half, on the same capture. A kernel record can
    // only land inside a userland line if the line reached the backend in
    // pieces, so this reds on exactly the coupling the count above reds on and
    // observes it from the other side.
    let console = Serial::named("console", &result.stdout);
    if let Some(spliced) = console.interleaved() {
        return Err(format!(
            "a kernel record landed inside a userland line: {:?}",
            spliced.chars().take(160).collect::<String>()
        ));
    }
    eprintln!(
        "  [console] {} writers x {} lines of {} bytes, 0 mixed; {} unterminated bytes flushed by \
         an exit",
        declared.writers, declared.lines, declared.width, declared.midline
    );
    Ok(())
}

fn declared(stdout: &str) -> Result<Declared, String> {
    let line = stdout
        .lines()
        .find(|l| l.contains("console-atomicity: writers="))
        .ok_or_else(|| format!("the guest never declared its run\n{}", tail(stdout)))?;
    let field = |key: &str| -> Result<usize, String> {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .and_then(|v| v.parse::<usize>().ok())
            .ok_or_else(|| format!("the guest's declaration has no `{key}`: {line:?}"))
    };
    Ok(Declared {
        writers: field("writers=")?,
        lines: field("lines=")?,
        width: field("width=")?,
        midline: field("midline=")?,
    })
}

/// The last of a capture, for a failure message. Two thousand 200-byte lines is
/// not something to put in an assertion message whole.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..]
        .iter()
        .map(|l| l.chars().take(100).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
