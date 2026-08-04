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

/// The guard page under every per-CPU idle stack.
///
/// That stack is 16 KiB of ordinary heap, so an overflow off its bottom did
/// not fault — it rewrote whatever the allocator had put underneath, and the
/// damage surfaced somewhere else entirely (a `BTreeMap` node with an
/// out-of-range index, a write to `0x4`). The idle loop runs `log_file::poll`,
/// a filesystem write reaching a block device, whose measured high water was
/// 11,505 bytes of the 16,384 with the USB command path still below the probe.
///
/// Absence is invisible to every log line and every screendump, so the only
/// way to ask whether the page is really gone is to touch it — which nothing
/// in the kernel does, that being the point of a guard page. `test-idle-guard`
/// supplies the one read.
pub fn idle_stack_guard(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            kernel_features: &["test-idle-guard"],
            ..Default::default()
        },
    );

    writeln!(qemu.stdin_mut(), "run test_rs_test_panic_child 9").expect("write to QEMU stdin");
    qemu.flush_stdin();
    let log = qemu.drain_serial(Duration::from_secs(20));

    // The premise: which address the kernel went for. Without it every
    // assertion below could be satisfied by a fault somewhere else.
    let addr = log
        .lines()
        .find_map(|l| l.split("reading the idle stack guard at ").nth(1))
        .map(|rest| rest.split_whitespace().next().unwrap_or("").to_string())
        .ok_or_else(|| {
            format!("the kernel never reached the guard read — is `test-idle-guard` on?\n{log}")
        })?;

    // The tell of a guard that is not there: `SYS_DEBUG` returned, so the read
    // landed on dlmalloc's bookkeeping for the chunk the idle stack lives in
    // and the child walked away.
    if log.contains("debug syscall returned") {
        return Err(format!(
            "the read at {addr} succeeded — the page below the idle stack is still mapped, \
             so an overflow writes into the heap instead of faulting"
        ));
    }
    for want in [
        format!("#PF UNHANDLED: cr2={addr}"),
        format!("KERNEL PANIC: read unmapped address at {addr}"),
    ] {
        if !log.contains(&want) {
            return Err(format!("no {want:?}; the kernel said:\n{log}"));
        }
    }
    // The page walk is the ground truth, and it is in the report: a PDE that
    // is a page table rather than a 2 MiB leaf, and a PTE of zero under it.
    // Without the split the direct map would still show `PS=1` here.
    if !log.contains("PS=0") || !log.contains("PTE:   0x0000000000000000") {
        return Err(format!(
            "the crash report's page walk does not show a split leaf with an empty entry:\n{log}"
        ));
    }
    eprintln!("  [guard] a read at {addr} faulted, one page below the idle stack");

    // And the machine halts, which is the intended end. An overflow off the
    // bottom of the idle stack is a kernel bug, not untrusted input, and
    // `fatal_exception` treats a fault on a *kernel* address as fatal by
    // policy. The whole change is that it is now reported at all: without the
    // guard the same overflow writes into the heap and the machine carries on
    // with a `BTreeMap` node the allocator no longer agrees about.
    Ok(())
}

/// A machine with no NVMe controller must boot.
///
/// `.expect("NVMe: no controller found")` killed it at 0.08 s — before
/// storage, before a console on the target laptop, and with the screen still
/// showing whatever the last checkpoint painted. It is the same class M1
/// closed for xHCI, on a different controller, and the same class the
/// designation stamp closed one layer up: absence of storage is a
/// configuration, not a failure.
pub fn diskless_boot(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let options = BootOptions {
        profile: qemu::Profile::Diskless,
        ..Default::default()
    };
    // The teeth, and the only ones: absence is invisible to every console line
    // and every screendump, so the argv is where it has to be checked.
    let argv = qemu::profile_argv(&options);
    if argv.iter().any(|a| a.contains("nvme")) {
        return Err(format!("the diskless profile still has an NVMe device: {argv:?}"));
    }

    let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
    let log = crate::common::serial::Serial::boot(&qemu);

    // The two absence claims are only claims if the console carried anything,
    // and `must_not_say` is what establishes that. The positives below made
    // this safe by luck rather than by design -- reorder them and the panic
    // scan is a claim about nothing again.
    log.must_be_clean()?;
    log.must_not_say("no controller found")?;
    log.must_say("NVMe: no controller on this machine")?;
    log.must_say("Boot: complete")?;
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
