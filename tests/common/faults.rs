//! The double fault path, which is the one that has to survive being the
//! thing that reports on itself.
//!
//! #DF is the only vector with an IST, so it is the only stack in the kernel
//! whose overflow is invisible: it is heap memory, it is written while the
//! crash report is being produced, and the corruption lands under whatever
//! the allocator handed out next. A test that only asserted "the report
//! appeared" would have passed throughout -- the report *did* appear, and it
//! scribbled on the heap on its way out.
//!
//! So the assertion is the kernel's own high-water measurement, taken after
//! `panic_flush` (the deepest point) and written straight to the UART rather
//! than through the log ring, which is one of the things an overflow may have
//! corrupted.
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::qemu::{self, BootOptions, QemuInstance};

/// The line `ist1_report` writes to the UART.
const MARKER: &str = "[ist1] used ";

pub fn double_fault_stack(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // Profile::Metal, because there the 16550 *is* the console, so the raw
    // write and the ordinary serial stream arrive on the same channel and one
    // reader sees both. It is also the T14's shape, which is the machine this
    // bug would have poisoned every double-fault investigation on.
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            kernel_features: &["test-double-fault"],
            ..Default::default()
        },
    );

    writeln!(qemu.stdin_mut(), "run test_rs_test_panic_child 4").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let log = qemu.drain_serial(Duration::from_secs(20));

    // The premise. If the CPU never took a #DF then nothing ran on IST1 and
    // every assertion below would be measuring the wrong stack.
    if !log.contains("DOUBLE FAULT") {
        return Err(format!("no double fault was taken — the trigger did not work\n{log}"));
    }
    let Some(line) = log.lines().find(|l| l.contains(MARKER)) else {
        return Err(format!(
            "the kernel never reported its IST1 usage; the report cannot have run to the \
             end on IST1\n{log}"
        ));
    };

    let (used, capacity) = parse(line)
        .ok_or_else(|| format!("could not read a usage out of {line:?}"))?;
    eprintln!("  [ist1] double fault report used {used} of {capacity} bytes");

    if line.contains("GUARD CORRUPTED") {
        return Err(format!(
            "the double fault report overflowed IST1 and wrote into the heap below it: \
             {used} bytes used of {capacity}"
        ));
    }
    if !line.contains("guard intact") {
        return Err(format!("unrecognised verdict in {line:?}"));
    }
    // Not just "it fit": it has to fit with room, or the next line added to
    // the crash report silently reintroduces the bug. Half the stack is the
    // margin, and it is stated here so that a change which eats it fails
    // here rather than on somebody's laptop.
    if used * 2 > capacity {
        return Err(format!(
            "the double fault report used {used} of {capacity} bytes — over half the stack, \
             so the margin for one more report line is gone"
        ));
    }
    Ok(())
}

/// `[ist1] used N of M bytes, ...`
fn parse(line: &str) -> Option<(usize, usize)> {
    let rest = line.split(MARKER).nth(1)?;
    let mut words = rest.split_whitespace();
    let used = words.next()?.parse().ok()?;
    if words.next()? != "of" {
        return None;
    }
    let capacity = words.next()?.parse().ok()?;
    Some((used, capacity))
}
