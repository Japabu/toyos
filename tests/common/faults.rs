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

    // **And the harness's own claim about a capture like this one, asked of the
    // only real one the suite produces.** `serial::death_report` is what a
    // failure verdict now carries, and it is staged against transcribed lines
    // everywhere else; a #DF is on the wire here already, so checking it costs
    // nothing and is the difference between a recovery gated on a guess about
    // the kernel's output and one gated on the output. It is a claim about the
    // report and not about IST1, which is why it sits above every assertion
    // that is.
    let report = super::serial::death_report(&log).ok_or_else(|| {
        format!("a capture carrying a real #DF yields no death report at all\n{log}")
    })?;
    let head = report.lines().next().unwrap_or_default();
    if !head.contains("DOUBLE FAULT") {
        return Err(format!("the report starts at {head:?} and not at the death\n{log}"));
    }
    // The body. The header alone is what the arm that lost this report already
    // printed, so the assertion is on the lines under it: the address that
    // started the chain, and the backtrace `double_fault_handler` writes after
    // the page walk.
    for want in ["cr2=", "Kernel backtrace:", MARKER] {
        if !report.contains(want) {
            return Err(format!("the report drops {want:?}:\n{report}"));
        }
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
    // Until the page walk, not for twenty seconds — `double_fault_stack`'s
    // shape and for its reason: this fault is fatal, so `halt_all_cpus` stops
    // every CPU without QEMU exiting and a plain drain has nothing left to
    // disconnect it. `debug_page_walk` is the last thing any assertion below
    // reads (PDPTE, then the PDE carrying `PS=`, then this line), and it runs
    // early in the crash report, so what follows on the wire — registers,
    // backtrace, stack — is diagnostic that nothing here asks for. A boot where
    // the guard is *not* there prints no page walk at all, which is the
    // `debug syscall returned` arm below: it pays the whole ceiling and then
    // reds, which is the right way round.
    //
    // **The three spaces are load-bearing.** `PDPTE:` one level up contains
    // `PTE:` as a substring, so the obvious predicate ends the drain two lines
    // early and reds a green machine with `the crash report's page walk does
    // not show a split leaf` — measured, on this change's first run.
    // `mm::paging::debug_page_walk` writes `PTE:   {:#018x}`, which is the
    // spelling the assertion below reads too.
    let log = qemu.drain_until(Duration::from_secs(20), |line| line.contains("PTE:   0x"));

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

/// How long the guest spins. The storm arms about 190 ms after the spinner
/// starts — a million syscalls at its measured rate — and this is what covers a
/// slow arming plus the storm itself on a shard with company.
const SPIN_SECS: u32 = 10;

/// The kernel's own summary line, printed last so a drain that ends on it has
/// every per-CPU line and the symbolized `rip` under it already.
const NMI_REPORT: &str = "syscall-window-nmi: sent=";

/// An NMI delivered where CPL is 0 and `rsp` is still the user's, and a machine
/// that carries on.
///
/// **One boot, and the two negative controls are `syscall_window_nmi_controls`'s
/// two.** All three used to be one name and it priced at 19,740 ms on the hosted
/// lane against a 10,000 ms ceiling — three Metal boots of 3,000 NMIs each. What
/// belongs per pull request is the property: the window is reachable, arrivals
/// land in it, and the machine survives them. What the controls establish is that
/// the property is not vacuous, which is a claim about the *instrument* and moves
/// to the nightly tier.
///
/// **The window.** `SYSCALL` switches no stack, so `arch::syscall`'s entry runs
/// three instructions at CPL 0 with the user's `rsp` and its exit one more
/// between `pop rsp` and `sysretq`. A frame the CPU builds there is a supervisor
/// write to a user page: SMAP refuses it, the `#PF` lands on the same stack, and
/// the machine takes a `#DF`. `arch::idt`'s IST2 row is the fix and this is what
/// says the row is load-bearing.
///
/// **Only one accelerator can put an interrupt in that window, and the verdict
/// says which one it ran on.** Under TCG, QEMU checks for a pending interrupt
/// between translation blocks and `syscall` ends one, so a pending NMI is
/// delivered at `syscall_entry+0`: the dev host reads 36 to 47 arrivals per
/// 3,000. Under KVM an NMI to a running vCPU is a host kick, a VM exit and an
/// injection at the next VM entry, and that entry is never one of these three
/// instructions — **0 of 6,000 on the hosted lane** (run 32584121311, two boots,
/// with 2,451 and 438 of the same NMIs arriving in Ring 3, so the aim was right
/// and the delivery point is elsewhere). That is a fact about the instrument,
/// and CI's guest lane is KVM only (`tests/CLAUDE.md`), so a derived in-window
/// count asserted there can never be green.
///
/// So the accelerator is read off the argv this boot was built from — the same
/// `-accel kvm` decision `qemu_command` made, not a re-derivation of it — and:
///
/// - **under TCG** the derived count is asserted as [`SAME_ORDER`] below;
/// - **under KVM** the numbers are printed as the instrument's verdict and what
///   is asserted is what KVM can witness: 3,000 aimed NMIs delivered to a CPU in
///   Ring 3, no `#DF`, and the victim still making syscalls when the last one
///   landed.
///
/// The window itself is gated on both by `syscall_window_nmi_controls`, whose
/// `nmi-without-ist` arm double faults at `syscall_entry` with `cr2 = rsp - 8`
/// wherever it runs. `wake_storm_cost` is the shape this follows: whether an
/// instrument can read the thing is the instrument's verdict, printed, and the
/// derived assertion is made only on a run that can read it.
///
/// **The derivation, where it applies.** Every iteration of the spinner's loop
/// passes through the window exactly once and through Ring 3 exactly once, so
/// the two counts differ only by how many points an NMI can be delivered at
/// inside each. The spinner's user loop is four instructions — `mov`, `syscall`,
/// `dec`, `jnz` — and the window is four more: `cld`, the `rsp` save, the
/// switch, and the exit's gap between `pop rsp` and `sysretq`. Four against four
/// under a delivery model uniform over instructions; under TCG the window
/// contributes one block boundary and the user side two or three. Both readings
/// say one traversal each within a small factor, and [`SAME_ORDER`] is the
/// bound: an order of magnitude, which no reading of the delivery model reaches
/// and a classification that has stopped tracking the loop fails at once.
///
/// Measured, dev host, TCG, `-smp 4`, 3,000 NMIs sent: **47 window arrivals
/// against 136 in Ring 3** aimed, **36 against 122** while the storm still
/// sprayed every sibling.
///
/// The bound is not the teeth on its own. What says the count means the window
/// is the first arrival's own `rip`, symbolized by the kernel and asserted
/// against `syscall_entry` — `dump_nmi_probe`'s rule, that a probe naming the
/// wrong instruction is worse than one naming none.
pub fn syscall_window_nmi(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // One traversal each per iteration, so the two counts are of one order.
    const SAME_ORDER: u64 = 10;

    let survived = storm(test_config, c_bins, rust_bins, &["syscall-window-nmi"], SPIN_SECS, |l| {
        l.contains(NMI_REPORT)
    })?;
    if survived.contains("DOUBLE FAULT") {
        return Err(format!(
            "an NMI in the syscall window still took the machine down — vector 2's IST index \
             is not doing what the table says\n{survived}"
        ));
    }
    let report = survived
        .lines()
        .find(|l| l.contains(NMI_REPORT))
        .ok_or_else(|| format!("the storm never reported — is `syscall-window-nmi` on?\n{survived}"))?;
    let field = |name: &str| -> Result<u64, String> {
        report
            .split_whitespace()
            .find_map(|w| w.strip_prefix(name)?.parse::<u64>().ok())
            .ok_or_else(|| format!("no {name}N field in {report:?}"))
    };
    let (sent, seen) = (field("sent=")?, field("seen=")?);
    let (window, ring3) = (field("window=")?, field("ring3=")?);
    let spun = field("spun=")?;
    eprintln!(
        "  [nmi-window] {sent} sent, {seen} taken, {window} in the window, {ring3} in Ring 3, \
         {spun} syscalls made under the storm"
    );

    // **What every accelerator witnesses.** The storm reached a CPU that was in
    // Ring 3 and kept it working: a victim that died at the first NMI would
    // stall `seen` at one, and one that stopped running Ring 3 code would leave
    // `spun` at zero however many NMIs were delivered.
    if seen * 10 < sent * 9 {
        return Err(format!(
            "{sent} NMIs were sent and only {seen} taken — the victim stopped taking them, \
             which is what an NMI that ends a CPU looks like from here\n{survived}"
        ));
    }
    if ring3 == 0 {
        return Err(format!(
            "not one of {seen} NMIs arrived with a Ring 3 frame — the storm was aimed at a CPU \
             that was not running the spinner, so this run measured an idle loop\n{survived}"
        ));
    }
    if spun == 0 {
        return Err(format!(
            "the victim made no syscall at all while {seen} NMIs were delivered to it — it \
             stopped running Ring 3 code under the storm\n{survived}"
        ));
    }

    if kvm_accelerated() {
        // The instrument's verdict, not the kernel's. See this function's
        // header: KVM injects at a VM entry that is never one of the three
        // instructions, measured 0 of 6,000, so an in-window count asserted here
        // would be asserting against the hypervisor's scheduling.
        eprintln!(
            "  [nmi-window] KVM delivered {window} of {seen} into the window: this accelerator \
             injects at a VM entry and cannot reach it, so what this run gates is that the \
             machine took {sent} aimed NMIs with IST2 in place and went on working"
        );
        return Ok(());
    }

    if window == 0 {
        return Err(format!(
            "{sent} NMIs were sent and {seen} taken under TCG, and not one landed in the \
             syscall window — this accelerator delivers at translation-block boundaries and \
             `syscall` ends one, so the instrument proved nothing about the stack the CPU \
             pushes on\n{survived}"
        ));
    }
    if window * SAME_ORDER < ring3 {
        return Err(format!(
            "{window} window arrivals against {ring3} in Ring 3. Every iteration passes through \
             both exactly once, so they are of one order; a {SAME_ORDER}x shortfall says the \
             arrivals are not being classified where they land\n{survived}"
        ));
    }
    // What makes the count a claim about the window rather than about some
    // other Ring 0 frame with a low `rsp`: the kernel symbolizes the first one
    // it saw, and it has to be the entry.
    let Some(rest) = survived.split("the first window arrival was here:\n").nth(1) else {
        return Err(format!("the report named no rip for the first window arrival\n{survived}"));
    };
    let named = rest.lines().next().unwrap_or("");
    if !named.contains("syscall_entry") {
        return Err(format!(
            "the first window arrival resolved to `{}`, not to the syscall entry — a Ring 0 \
             frame with a user `rsp` somewhere else is a different finding, and this test is \
             not measuring it\n{survived}",
            named.trim(),
        ));
    }
    Ok(())
}

/// Whether this host's guests run under KVM, read off the argv a boot is built
/// from.
///
/// **The decision itself rather than a second reading of it**: `qemu_command`
/// puts `-accel kvm` there when `toyos_build::kvm_usable()` says so, and
/// `profile_argv` is that same builder. A CPUID probe in the guest would be a
/// second place that can be told the wrong answer, and `virtio_net_no_msix` and
/// `diskless_boot` already assert about a boot by reading its argv.
fn kvm_accelerated() -> bool {
    qemu::profile_argv(&storm_options(&[]))
        .windows(2)
        .any(|w| w[0] == "-accel" && w[1] == "kvm")
}

/// The two negative controls on [`syscall_window_nmi`], which is where the
/// property is asserted and this is where it is shown not to be vacuous.
///
/// **Nightly, and `src/tiers.rs` carries the row.** Two Metal boots of 3,000
/// NMIs, and both end in a halted machine that has to be drained past its own
/// report — which is what the price is. A control is a claim about the
/// instrument rather than about the kernel under review: it says the same test,
/// run against a kernel with the defect, reds. That does not change per pull
/// request, and the fixed arm reds per pull request if the kernel does.
///
/// `#MC` has no control here and cannot have one: CR4.MCE is set and nothing in
/// QEMU raises a machine check. Its IST index rides the same table column NMI's
/// does, plus `arch::idt`'s compile-time assertion over that table.
pub fn syscall_window_nmi_controls(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // The first control: the same boot with vector 2's IST index taken off.
    // Everything else — the handler, the gate, the storm, the spinner — is the
    // same, so what the `#DF` below measures is the one byte.
    // Drained past the header, not to it: `double_fault_handler` prints the
    // address that started the chain, then the registers, then the backtrace
    // that carries the symbol every assertion below reads, and only then this.
    let unfixed = storm(
        test_config,
        c_bins,
        rust_bins,
        &["syscall-window-nmi", "nmi-without-ist"],
        SPIN_SECS,
        |l| l.contains("Scanning kernel stack"),
    )?;
    let Some(df) = unfixed.lines().find(|l| l.contains("DOUBLE FAULT")) else {
        return Err(format!(
            "with no IST on vector 2 the machine survived the whole storm — the control stages \
             nothing, so `syscall_window_nmi` proves nothing either\n{unfixed}"
        ));
    };
    eprintln!("  [nmi-window] without IST2: {}", df.trim());
    if !unfixed.contains("syscall_entry") {
        return Err(format!(
            "the control double faulted somewhere other than the syscall entry\n{unfixed}"
        ));
    }
    // **The exact signature, and the reason this is a control and not a
    // coincidence**: the address the CPU faulted on is the first qword of the
    // frame it was trying to push, one below the `rsp` it was pushing at. A #DF
    // for any other reason does not put `cr2` there.
    let hex = |line: &str, field: &str| -> Option<u64> {
        let rest = line.split(field).nth(1)?;
        let digits: String = rest.trim_start_matches("0x").chars().take(16).collect();
        u64::from_str_radix(&digits, 16).ok()
    };
    let cr2 = unfixed.lines().find_map(|l| hex(l, "cr2="));
    let rsp = unfixed.lines().find_map(|l| hex(l, "rsp="));
    match (cr2, rsp) {
        (Some(cr2), Some(rsp)) if cr2 == rsp.wrapping_sub(8) => {
            eprintln!("  [nmi-window] without IST2: cr2={cr2:#x} is rsp-8, the frame's first qword");
        }
        (cr2, rsp) => {
            return Err(format!(
                "the control's #DF reports cr2={cr2:#x?} against rsp={rsp:#x?}; the fault this \
                 stages is the frame's own first qword at rsp-8, so this is a different \
                 death\n{unfixed}"
            ));
        }
    }

    // The second control: an NMI handler that returns early through `iretq`
    // un-masks NMIs while still standing on IST2, which is the one way a second
    // NMI can enter on that stack. The check has to fire and say so.
    let nested = storm(
        test_config,
        c_bins,
        rust_bins,
        &["syscall-window-nmi", "nmi-nested"],
        SPIN_SECS,
        |l| l.contains("NESTED NMI"),
    )?;
    let Some(loud) = nested.lines().find(|l| l.contains("NESTED NMI")) else {
        return Err(format!(
            "a second NMI entered on IST2 and the machine said nothing: the outer handler's \
             frame was overwritten silently, which is the failure this check exists for\n{nested}"
        ));
    };
    eprintln!("  [nmi-window] nested: {}", loud.trim());
    Ok(())
}

/// One storm boot: the spinner in Ring 3, the kernel's NMIs at it, drained until
/// `done` or the ceiling.
///
/// The ceiling is a ceiling and not the run — every arm here ends either with
/// the kernel's report or with a halted machine, and a halted machine neither
/// exits QEMU nor disconnects the drain.
/// One declaration of the machine every arm boots, so that the argv
/// [`kvm_accelerated`] reads is the argv the boot is built from.
fn storm_options(params: &'static [&'static str]) -> BootOptions {
    BootOptions {
        kernel_params: params,
        // `double_fault_stack`'s profile and for its reason: on Metal the 16550
        // *is* the console, so `serial::panic_raw`'s bytes and the ordinary log
        // stream arrive on one channel and one reader sees both. The nested-NMI
        // report is a raw write — that handler may not reach the log ring at all
        // (`arch::idt::nmi`) — so on any other profile it lands on a UART
        // nothing here is reading.
        profile: qemu::Profile::Metal,
        // Four, so that the scheduler has somewhere to put the spinner that is
        // not the CPU whose idle loop does the storming.
        smp: 4,
        ..Default::default()
    }
}

fn storm(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
    params: &'static [&'static str],
    secs: u32,
    done: impl Fn(&str) -> bool,
) -> Result<String, String> {
    let mut qemu =
        QemuInstance::boot_with_options(test_config, c_bins, rust_bins, storm_options(params));
    writeln!(qemu.stdin_mut(), "run test_rs_nmi_window_spin {secs}").expect("write to QEMU stdin");
    qemu.flush_stdin();
    Ok(qemu.drain_until(Duration::from_secs(u64::from(secs) + 20), |line| done(line)))
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
    //
    // **A ceiling now rather than the run.** The guest neither exits nor halts
    // here — an NMI interrupts a CPU, it does not kill it — so a plain drain
    // paid the whole twenty seconds on every green run, against a guest that
    // was done in about a third of it. Both markers, and neither implies the
    // other's order: the dump is requested while the victim is deaf and the
    // victim announces its own return when the 400 ms window closes, so which
    // of the two lands last is a fact about how long the report takes rather
    // than about the machine.
    let dumped = std::cell::Cell::new(false);
    let rejoined = std::cell::Cell::new(false);
    let log = qemu.drain_until(Duration::from_secs(20), |line| {
        dumped.set(dumped.get() || line.contains("=== end of dump ==="));
        rejoined.set(rejoined.get() || line.contains("rejoined after"));
        dumped.get() && rejoined.get()
    });

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
