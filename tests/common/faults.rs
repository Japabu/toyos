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
            kernel_features: toyos_build::build::TEST_KERNEL,
            ..Default::default()
        },
    );

    writeln!(qemu.stdin_mut(), "run test_rs_test_panic_child 4").expect("write to QEMU stdin");
    qemu.flush_stdin();
    // Until the report, not for twenty seconds: the fatal path halts every CPU
    // without exiting QEMU, so a plain drain has nothing left to disconnect it
    // and waits out the whole ceiling. The marker is the line every assertion
    // below reads, and `ist1_report` writes it last.
    let log = qemu.drain_until(Duration::from_secs(20), |line| line.contains(MARKER));

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
/// out-of-range index, a write to `0x4`). The idle loop ran `log_file::poll`
/// when that was measured — a filesystem write reaching a block device, whose
/// high water was 11,505 bytes of the 16,384 with the USB command path still
/// below the probe. That caller is gone at log architecture L6 and `drain_irqs`
/// still reaches a device from the same stack.
///
/// Absence is invisible to every log line and every screendump, so the only
/// way to ask whether the page is really gone is to touch it — which nothing
/// in the kernel does, that being the point of a guard page. `SYS_DEBUG` action
/// 9 supplies the one read.
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
            kernel_features: toyos_build::build::TEST_KERNEL,
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
            format!("the kernel never reached the guard read — is `test-actuators` on?\n{log}")
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

/// A NIC that cannot raise an interrupt must cost the machine networking and
/// nothing else.
///
/// The MSI-X setup was written out three times and the copies answered this
/// question three different ways: the xHCI driver fell back to MSI, and both
/// virtio drivers called `panic!`. So the one device on the bus with no way to
/// deliver a packet took down a kernel whose disk, console, audio and USB were
/// all working — class M1 again, on the mechanism M1's own fix went through.
///
/// The other two virtio functions keep their vectors, which is what makes the
/// verdict mean anything: the console that carries the refusal and the audio
/// device beside it are on the same bus, driven by the same code, and neither
/// notices.
pub fn virtio_net_no_msix() -> Result<(), String> {
    let options = BootOptions {
        profile: qemu::Profile::VirtioNetNoMsix,
        ..Default::default()
    };
    // The actuator is a device property and argv is the only place one is
    // visible: a NIC that quietly kept its MSI-X table would make every line
    // below a re-run of the happy path under a different name.
    let argv = qemu::profile_argv(&options);
    let devices = |kind: &str| -> Vec<&str> {
        argv.windows(2)
            .filter(|w| w[0] == "-device" && w[1].starts_with(kind))
            .map(|w| w[1].as_str())
            .collect()
    };
    let nics = devices("virtio-net");
    let [nic] = nics[..] else {
        return Err(format!("this profile is one NIC; argv has {nics:?}"));
    };
    if !nic.contains("vectors=0") {
        return Err(format!("{nic} still has its MSI-X table"));
    }
    for kind in ["virtio-sound", "virtio-serial"] {
        let others = devices(kind);
        let [other] = others[..] else {
            return Err(format!("this profile is one {kind}; argv has {others:?}"));
        };
        if other.contains("vectors=") {
            return Err(format!(
                "{other} is crippled too, so a refusal could not be shown to be per device \
                 — and with no console there would be nothing to read it on"
            ));
        }
    }

    // `tests/netcase` rather than the ordinary config, because it is the one
    // that runs netd — and netd's own answer is the assertion below that the
    // refusal reached userland rather than stopping at a log line.
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/netcase");
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    // netd is spawned before the ready marker and speaks after it, so its line
    // is drained for rather than read out of the boot capture. **What is waited
    // for is the whole line and not a prefix naming the program**: init reports
    // the claim it could not make as `init: netd: no nic on this machine
    // (NotFound)`, and that is already in the boot capture before netd has run
    // at all, so a `"netd: "` predicate is satisfied by the wrong speaker.
    const NETD_EXITS: &str = "netd: no NIC on this machine, exiting";
    let mut text = qemu.boot_log().to_string();
    let stalled =
        qemu::await_guest(&mut qemu, &mut text, "netd's own answer", |c| c.contains(NETD_EXITS))
            .err();
    let log = crate::common::serial::Serial::named("boot console", text);

    // Refused by name, at a named function, and not by claiming a mode it does
    // not have: the xHCI driver's `polled mode` line is the defect this whole
    // family exists to keep out of the tree.
    log.must_say("VirtIO net: NOT INITIALISED at PCI")?;
    log.must_not_say("VirtIO net: MSI-X vector")?;
    // All the way out to userland, rather than a kernel that logged a refusal
    // and handed netd a NIC anyway.
    if !log.text().contains(NETD_EXITS) {
        return Err(format!(
            "{}{NETD_EXITS:?} never reached the boot console:\n{}",
            stalled.map(|why| format!("{why}\n")).unwrap_or_default(),
            log.text()
        ));
    }
    // And the machine is otherwise whole. `must_be_clean` is what makes the
    // change from `panic!` an assertion rather than a hope.
    log.must_say("virtio-sound: MSI-X vector")?;
    log.must_say("Boot: complete")?;
    log.must_be_clean()?;
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

/// The blocked-task dump's NMI probe: a CPU that ignores a kick is named, and
/// then asked where it is with the one interrupt it cannot mask.
///
/// The verdict `no answer: it did not reach a scheduler pass` has three causes
/// — spinning with `IF` clear, halted with a lost kick, wedged below the
/// interrupt layer — and on the owner's T14 it named three CPUs without saying
/// which. The NMI separates them, so what this asserts is the separation: the
/// kick goes unanswered, the NMI is answered, and the `rip` it brings back
/// lands in the spin the actuator is executing.
///
/// The last assertion is the one that keeps the instrument honest. A probe that
/// reported *some* address would satisfy every other line here; only resolving
/// it against the kernel's own symbols says the report points at where the CPU
/// actually was.
pub fn dump_nmi_probe(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            kernel_params: &["dump-deaf-cpu"],
            ..Default::default()
        },
    );
    // 3 s is the actuator's earliest arming, not its schedule: cpu0 only looks
    // once per idle-loop iteration, and on a settled guest the next thing that
    // wakes it is the 10 s health tick. Add 400 ms of deafness and the dump's
    // 250 ms kick budget, and 20 s is the first round number that clears it.
    let log = qemu.drain_serial(Duration::from_secs(20));

    if !log.contains("=== blocked-task dump:") {
        return Err(format!("the dump never ran — is `dump-deaf-cpu` on?\n{log}"));
    }
    let silent: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("no answer: it did not reach a scheduler pass"))
        .collect();
    if silent.len() != 1 {
        return Err(format!(
            "expected exactly the deafened CPU to miss its kick, got {}:\n{}\n{log}",
            silent.len(),
            silent.join("\n"),
        ));
    }
    if log.contains("no NMI answer either") {
        return Err(format!(
            "the NMI went unanswered too. The victim spins with IF clear and an NMI is not \
             maskable by IF, so this says the NMI never reached it at all — vector 2, the ICR \
             delivery mode, or the handler.\n{log}"
        ));
    }
    let Some(rest) = log.split("NMI answered, it is here:\n").nth(1) else {
        return Err(format!("the probe reported no rip for the silent CPU\n{log}"));
    };
    let rip_line = rest.lines().next().unwrap_or("");
    if !rip_line.contains("deaf_window") {
        return Err(format!(
            "the rip resolved to `{}`, not to the spin the CPU was executing — a probe that \
             names the wrong instruction is worse than one that names none\n{log}",
            rip_line.trim(),
        ));
    }
    // And it comes back: an NMI interrupts, it does not kill. The witness has
    // to be the victim's own line, printed after it re-enables interrupts.
    // `Boot: complete` was the first attempt and is no witness at all — it is
    // printed at 225 ms, ten seconds before this window opens, and by cpu0 into
    // the boot log this drain does not even contain.
    if !log.contains("rejoined after") {
        return Err(format!(
            "the deafened CPU never said it was back — an NMI must interrupt a CPU, not kill \
             it\n{log}"
        ));
    }
    Ok(())
}
