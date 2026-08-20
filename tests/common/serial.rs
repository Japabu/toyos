//! Assertions over what the guest said, that cannot pass on a dead channel.
//!
//! The capture itself is not new: [`QemuInstance::boot_log`] has always held
//! every console line up to the ready marker, and nineteen call sites read it.
//! What was missing is a vocabulary — every one of those sites hand-rolls
//! `contains` against a `String`, and seven of them do it in the shape
//!
//! ```ignore
//! for bad in ["PANIC:", "panicked at"] {
//!     if log.contains(bad) { return Err(..) }
//! }
//! ```
//!
//! which is a claim about nothing if `log` is empty. A capture that silently
//! comes back empty turns every such scan green. That is the failure this type
//! exists to make impossible: a negative assertion first has to prove the
//! channel carried anything at all.
//!
//! Liveness is "the kernel wrote at least one line". Every configuration that
//! has a text channel logs before anything a test asserts on can happen —
//! including a guest that dies at 0.068 s, which is the earliest failure in
//! the suite — so zero kernel lines means the channel broke, never that the
//! boot was clean.
//!
//! This is the text channel. The framebuffer is `screen.rs`, deliberately the
//! only thing in the suite that reads pixels.

use super::qemu::{is_kernel_line, QemuInstance};

/// Whose death a console line reports.
///
/// The distinction is not decoration — it is the whole of what
/// [`QemuInstance::run_test_paced`] was missing. A machine that has halted
/// answers nothing else the run asks; a process that died is what half this
/// suite is *for*.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Died {
    /// The kernel itself. Every path that writes one of these words ends at
    /// `apic::halt_all_cpus` — **unless** the panic handler finds the panic
    /// recoverable, which it does for a `panic!` taken in syscall context
    /// (`kernel/src/main.rs`: the caller is killed and the machine carries on,
    /// which is what `panic_recovery`, `heap_ceiling` and
    /// `screen_recoverable_untouched` are about). No line says which of the two
    /// happened. So what a caller learns here is "the kernel said it was
    /// dying", and the guest going quiet afterwards is what says it meant it.
    Kernel,
    /// A process the kernel killed: a Ring 3 fault, reported by name in
    /// `kernel/src/arch/idt/exceptions.rs`. The machine is fine — a test whose
    /// whole subject is a process dying (`handle_kill_policy` and every
    /// `faults.rs` probe) produces these deliberately. Before a boot's ready
    /// marker it still ends the boot: whatever died was `init` or one of its
    /// children, and nothing left is going to reach the marker.
    Faulted,
    /// A process that ended itself — its own panic handler wrote the line
    /// (`userland/libc/src/lib.rs`, or the std fork's). Never the machine's
    /// business, and not even always the boot's: `sshd` loses a race with
    /// `netd`'s teardown on a NIC-less machine and panics across boots that
    /// then come up perfectly
    /// (`issues/build/sshd-panics-when-netd-exits-before-it-binds.md`),
    /// which is why a boot wait must not end on one.
    Panicked,
}

/// Every spelling of a death this tree produces, and what it means from each of
/// the two speakers a console carries.
///
/// **One table, two columns, because the spelling is only half the answer.**
/// `PANIC:` is the header `crash_report_panic` writes and it is also whatever a
/// program chooses to print; `SEGFAULT` is written by the kernel *about
/// somebody else*. So who is speaking picks the column — [`is_kernel_line`],
/// the harness's one definition of that — and a spelling read out of the wrong
/// column is exactly the bug this table exists to make unwriteable. Nothing
/// else in `tests/common/qemu.rs` knows these words; `one_vocabulary` in
/// `tests/toyos.rs` is what keeps it that way.
///
/// The two columns are equal for every spelling no program in this tree writes,
/// and that is deliberate rather than lazy: the console is not line-atomic, so
/// a program's unterminated write can be spliced ahead of a kernel record and
/// take the `[kernel …]` prefix off the front of the assembled line. A word
/// only the kernel says is still the kernel's however the line was built.
///
/// Order matters where one spelling contains another: `PANIC:` is looked for
/// before `panicked at`, so the header of a kernel crash report is read as the
/// header and not as its own second line — and `EARLY PANIC:` needs no row,
/// because it carries `PANIC:` inside it.
///
/// What this table does **not** claim, because the console cannot say it:
/// `fatal_exception`'s recursive arm writes `FAULT rip=… RECURSIVE` and then
/// halts if the fault was the kernel's and kills the process if it was a
/// program's — and it is the only line either case produces, because that arm
/// skips `crash_report`. The non-recursive arm prints the same `FAULT rip=…`
/// before every ordinary Ring 3 segfault, of which this suite stages many
/// deliberately. So the spelling is ambiguous both ways and is left out; a
/// recursive kernel fault is still found by the guard, one silent ceiling later.
const DEATHS: &[(&str, Died, Died)] = &[
    // spelling             the kernel wrote it   anybody else wrote it
    // kernel/src/arch/idt/exceptions.rs — a Ring 0 exception. Always fatal.
    ("KERNEL PANIC", Died::Kernel, Died::Kernel),
    // `double_fault_handler`, which is `-> !` and ends at `halt_all_cpus`. It
    // writes none of the words above it, which is how a staged `#DF` inside a
    // `run_test` was still reported as a stall after the rest of this table
    // existed — the measurement that put this row here.
    ("DOUBLE FAULT", Died::Kernel, Died::Kernel),
    // `machine_check_handler`, the one exception a Ring 3 frame does not make
    // the process's fault. Also `-> !`.
    ("MACHINE CHECK", Died::Kernel, Died::Kernel),
    // kernel/src/iommu/vtd/fault.rs — every stream on this machine is
    // kernel-owned, so a DMA fault is a kernel bug and the handler halts.
    ("iommu: DMA FAULT", Died::Kernel, Died::Kernel),
    // kernel/src/main.rs — a panic that landed on a CPU already inside a fault
    // or a report. The rest of the line is `panic::last_words`: which of the
    // four states it found, what that first crash was, and where the second
    // one is. It goes out the UART port first and then as a record, so a
    // capture can carry it twice.
    ("DOUBLE PANIC", Died::Kernel, Died::Kernel),
    // kernel/src/main.rs — the reentry guard, written straight out the UART
    // port with no lock and therefore with no prefix. It reaches the 16550 log
    // rather than the console, and is here so that a capture carrying it is
    // never read as anything else.
    ("PANIC REENTRY", Died::Kernel, Died::Kernel),
    // kernel/src/arch/idt/exceptions.rs `crash_report_panic` — a Rust `panic!`.
    ("PANIC:", Died::Kernel, Died::Panicked),
    // `PanicInfo`'s `Display` newlines this out of the record above, so the
    // kernel writes it too — and so does every program's panic handler.
    ("panicked at", Died::Kernel, Died::Panicked),
    ("libc panic:", Died::Panicked, Died::Panicked),
    // kernel/src/arch/idt/exceptions.rs — a Ring 3 fault, by name.
    ("SEGFAULT", Died::Faulted, Died::Faulted),
    ("SIGILL tid=", Died::Faulted, Died::Faulted),
    ("SIGFPE tid=", Died::Faulted, Died::Faulted),
    ("SIGBUS tid=", Died::Faulted, Died::Faulted),
    ("FATAL tid=", Died::Faulted, Died::Faulted),
];

/// What this console line says died, if anything.
///
/// The one answer. `wait_for_ready` ends a boot on a death of any kind the
/// machine cannot come back from; `run_test_paced` and `await_guest` end a run
/// on [`Died::Kernel`] alone, because a program is allowed to die without
/// taking the machine with it; and [`Serial::must_be_clean`] refuses a capture
/// carrying one. Four questions, one vocabulary, and no way for them to
/// disagree about a spelling.
pub fn died(line: &str) -> Option<Died> {
    let (_, by_kernel, by_anyone) = DEATHS.iter().find(|(word, _, _)| line.contains(word))?;
    Some(if is_kernel_line(line) { *by_kernel } else { *by_anyone })
}

/// The first line of a capture on which the kernel said it was dying.
///
/// For a wait that holds the whole capture rather than reading a line at a time
/// — [`super::qemu::await_guest`] is the one — and for the same reason: a guest
/// that has halted every CPU has stopped for a reason that is written down, and
/// a verdict of "it went quiet" throws that reason away.
pub fn kernel_death(capture: &str) -> Option<&str> {
    capture.lines().find(|l| died(l) == Some(Died::Kernel))
}

/// How much of a dying kernel's own account a verdict carries.
///
/// A fatal report is a header, a register dump, a page walk and a bounded
/// backtrace, and every CPU is being halted around it, so very little else
/// reaches the console after it. Eighty lines holds one whole report with room
/// to spare. The *first* eighty, where `kernel_account` in `tests/toyos.rs`
/// keeps the *last* sixty of what a killed process left behind: that one reads
/// a machine that is still running and its tail says how the process ended,
/// this one starts at the end and the head is the whole of what says why.
const REPORT_LINES: usize = 80;

/// Everything the guest said from the line the kernel announced its own death.
///
/// **The artefact, and a verdict that named the death used to throw it away.**
/// On 2026-08-18 a `DOUBLE FAULT on CPU 1` took a twelve-wide suite's guest
/// down; `double_fault_handler` writes its whole report on IST1 — the header,
/// `cr2`, the `#DF` frame, a page walk, a kernel backtrace and a scan of the
/// original stack for the frame that started the chain — and the failing test's
/// arm printed `result.stdout`, which is the *userland* half of the capture and
/// carried two daemon lines. The report was in `result.serial` and nothing read
/// it (`issues/kernel/a-double-fault-on-cpu-1-under-a-wide-suite.md`).
///
/// The death line is where it starts, because everything before it is the run
/// going normally and the point of a bound is that the report survives it.
/// Truncation says how much it dropped rather than dropping it in silence.
///
/// One capture, taken in the order the guest wrote it: [`super::qemu::WaitVerdict`]
/// hands the halves of a test's window over in that order, so the first kernel
/// death in the window is the one reported on.
pub fn death_report(capture: &str) -> Option<String> {
    // `split_inclusive` rather than `lines`, because the offset of the line is
    // what the report starts at and `lines` throws it away.
    let mut at = 0;
    for line in capture.split_inclusive('\n') {
        if died(line) == Some(Died::Kernel) {
            let all: Vec<&str> = capture[at..].lines().collect();
            let kept = all.len().min(REPORT_LINES);
            let head = if all.len() > kept {
                format!("(the first {kept} of the {} lines that followed)\n", all.len())
            } else {
                String::new()
            };
            return Some(format!("{head}{}", all[..kept].join("\n")));
        }
        at += line.len();
    }
    None
}

/// What the one answer is made of, for the gate that keeps it the only one.
///
/// `one_vocabulary` in `tests/toyos.rs` refuses a wait that hands any of these
/// straight to a scan of its own; it reads them from here so that it cannot
/// become a second, staler copy of the list it is protecting.
pub fn spellings() -> impl Iterator<Item = &'static str> {
    DEATHS.iter().map(|(word, _, _)| *word)
}

pub struct Serial {
    text: String,
    /// What produced it, for error messages that name the channel.
    source: String,
}

impl Serial {
    /// Everything the guest said on the way to its ready marker.
    pub fn boot(qemu: &QemuInstance) -> Self {
        Self { text: qemu.boot_log().to_string(), source: String::from("boot console") }
    }

    /// For text a test collected itself — a `drain_serial` window, a
    /// `TestResult::serial`, the 16550 file of a guest that died early.
    pub fn named(source: &str, text: impl Into<String>) -> Self {
        Self { text: text.into(), source: source.to_string() }
    }

    /// Append a later window — `drain_serial`, a test's own serial. Keeps one
    /// object to assert against instead of a `format!` of two.
    pub fn push(&mut self, more: &str) {
        self.text.push_str(more);
        if !more.ends_with('\n') {
            self.text.push('\n');
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn kernel_lines(&self) -> usize {
        self.text.lines().filter(|l| is_kernel_line(l)).count()
    }

    /// A line carrying a kernel prefix somewhere other than its start, which
    /// is the virtio-console's missing line atomicity showing up in the
    /// capture: `log!` and a userspace `println!` interleave mid-word (see
    /// `issues/`). Reported rather than repaired — a needle that went
    /// missing because it was split in half should say so instead of looking
    /// like the guest never said it.
    pub fn interleaved(&self) -> Option<&str> {
        self.text
            .lines()
            .find(|l| !is_kernel_line(l) && l.contains("[kernel "))
    }

    /// The channel carried something the kernel wrote.
    pub fn alive(&self) -> Result<(), String> {
        if self.kernel_lines() == 0 {
            return Err(format!(
                "the {} carried no kernel output at all ({} bytes): every assertion \
                 below it would be a claim about nothing",
                self.source,
                self.text.len()
            ));
        }
        Ok(())
    }

    /// The guest said this. Returns the whole line, so a caller that needs a
    /// field out of it parses from the line rather than re-scanning the blob.
    pub fn must_say(&self, needle: &str) -> Result<&str, String> {
        if let Some(line) = self.text.lines().find(|l| l.contains(needle)) {
            return Ok(line);
        }
        let note = match self.interleaved() {
            Some(l) => format!(
                "\nnote: the {} has interleaved lines, so this needle may have been \
                 split across one — first: {l:?}",
                self.source
            ),
            None => String::new(),
        };
        Err(format!("{needle:?} never reached the {}:{note}\n{}", self.source, self.text))
    }

    /// The guest said this **after** it said `marker`.
    ///
    /// A whole-capture scan answers with the earliest line of that shape,
    /// whoever wrote it and whenever — and for a test that *stages* the event it
    /// is looking for, the earliest line is the wrong one whenever anything else
    /// on the machine can produce the same shape. `i8042_undecoded_bytes`
    /// injects an undecodable key once the guest prints `===I8042_READY===` and
    /// then read the first `nothing decoded` line in its capture as the answer;
    /// the driver's own bring-up produces one before that marker, and on a
    /// laptop a real spurious interrupt can too
    /// (`issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md`).
    ///
    /// The marker is what the injection was timed off, so it is the boundary the
    /// test actually knows — no host clock is involved, and a stranger line
    /// before it can no longer be read as the test's own. A missing marker is a
    /// failure rather than a fallback to the whole capture: the anchor going
    /// missing is exactly when the loose scan would look like it worked.
    pub fn must_say_after(&self, marker: &str, needle: &str) -> Result<&str, String> {
        let Some(at) = self.text.find(marker) else {
            return Err(format!(
                "{marker:?} — the line {needle:?} would have to follow — never reached the {}:\n{}",
                self.source, self.text
            ));
        };
        // From the end of the marker's own line, so a marker and a needle that
        // share one line (the console splices them when a writer left no
        // newline) is not read as the needle arriving first.
        let after = self.text[at..].find('\n').map_or(self.text.len(), |n| at + n + 1);
        if let Some(line) = self.text[after..].lines().find(|l| l.contains(needle)) {
            return Ok(line);
        }
        let earlier = match self.text[..after].lines().find(|l| l.contains(needle)) {
            Some(l) => format!(
                "\nnote: one arrived *before* {marker:?} and is not this test's — first: {l:?}"
            ),
            None => String::new(),
        };
        Err(format!(
            "{needle:?} never reached the {} after {marker:?}:{earlier}\n{}",
            self.source, self.text
        ))
    }

    /// The guest did not say this — and the channel was working, so the
    /// absence means something.
    pub fn must_not_say(&self, needle: &str) -> Result<(), String> {
        self.alive()?;
        match self.text.lines().find(|l| l.contains(needle)) {
            Some(line) => Err(format!(
                "{needle:?} on a {} that should not have it: {line:?}\n{}",
                self.source, self.text
            )),
            None => Ok(()),
        }
    }

    /// Nothing panicked.
    ///
    /// Read straight off [`DEATHS`] rather than out of a second list beside it:
    /// what this refuses is every spelling the *kernel* uses about itself,
    /// wherever on the line it appears. A process dying is not this assertion's
    /// business — `must_not_say("SEGFAULT")` is a thing a caller says when it
    /// means it.
    pub fn must_be_clean(&self) -> Result<(), String> {
        for (bad, by_kernel, _) in DEATHS {
            if *by_kernel != Died::Kernel {
                continue;
            }
            self.must_not_say(bad)?;
        }
        Ok(())
    }
}

/// Prove the vocabulary in both directions, with no guest.
///
/// `screen_decoder` does this for the framebuffer decoder — an instrument
/// nothing else checks is an instrument nobody knows is broken. Every case
/// here is one this type must *fail*, because the failures are the point: a
/// `must_not_say` that returns `Ok` on an empty capture is the whole hazard.
pub fn self_check() -> Result<(), String> {
    let live = Serial::named("test capture", "[kernel 0.001 cpu0] NVMe: found\nhello from userland\n");
    let dead = Serial::named("test capture", "");
    // Userland said things; the kernel said nothing. This is what a broken
    // capture looks like when it is not simply empty, and the case a
    // `text.is_empty()` guard would wave through.
    let mute = Serial::named("test capture", "hello from userland\n");
    let panicking = Serial::named(
        "test capture",
        "[kernel 0.001 cpu0] NVMe: found\n[kernel 0.002 cpu0] PANIC: nope\n",
    );

    /// One row: what it is called, whether it must pass, and the call itself.
    type Case<'a> = (&'a str, bool, &'a dyn Fn() -> Result<(), String>);

    let cases: &[Case] = &[
        // must_say
        ("must_say finds a line", true, &|| live.must_say("NVMe: found").map(|_| ())),
        ("must_say on an absent line", false, &|| live.must_say("no such line").map(|_| ())),
        ("must_say on a dead channel", false, &|| dead.must_say("anything").map(|_| ())),
        // must_not_say: the absent case passes only because the channel is alive
        ("must_not_say on an absent line", true, &|| live.must_not_say("no such line")),
        ("must_not_say on a present line", false, &|| live.must_not_say("NVMe: found")),
        // The dead gate itself, from both directions.
        ("must_not_say on an empty capture", false, &|| dead.must_not_say("anything")),
        ("must_not_say with no kernel output", false, &|| mute.must_not_say("anything")),
        // must_be_clean
        ("must_be_clean on a clean boot", true, &|| live.must_be_clean()),
        ("must_be_clean on a panic", false, &|| panicking.must_be_clean()),
        ("must_be_clean on an empty capture", false, &|| dead.must_be_clean()),
    ];

    for (what, want_ok, run) in cases {
        let got = run();
        if got.is_ok() != *want_ok {
            return Err(format!(
                "{what}: wanted {}, got {got:?}",
                if *want_ok { "Ok" } else { "Err" }
            ));
        }
    }

    // must_say hands back the line, not just a yes.
    let line = live.must_say("NVMe")?;
    if !line.contains("cpu0") {
        return Err(format!("must_say returned {line:?}, not the whole line"));
    }

    // `must_say_after`, against the capture that made it exist: a stranger line
    // of the right shape before the marker, and the test's own after it. The
    // first case is the defect and is asserted in both directions — the plain
    // scan reads the stranger, which is what `i8042_undecoded_bytes` did.
    const READY: &str = "===I8042_READY===";
    let stranger = "[kernel 0.418 cpu1] i8042: 1 interrupts and 0 bytes, nothing decoded — first \
                    seen at 418ms";
    let mine = "[kernel 2.816 cpu0] i8042: 2 interrupts and 6 bytes, nothing decoded — no event \
                from [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5], first seen at 2816ms";
    let staged = Serial::named("test capture", format!("{stranger}\n{READY}\n{mine}\n"));
    if staged.must_say("nothing decoded")? != stranger {
        return Err(String::from("the whole-capture scan stopped reading the earliest line"));
    }
    if staged.must_say_after(READY, "nothing decoded")? != mine {
        return Err(format!("must_say_after read a line from before {READY:?}"));
    }
    // And with only the stranger in the capture there is no answer at all,
    // rather than the stranger: a test that staged nothing must not pass on
    // somebody else's line.
    let only_stranger = Serial::named("test capture", format!("{stranger}\n{READY}\n"));
    let err = only_stranger.must_say_after(READY, "nothing decoded").unwrap_err();
    if !err.contains("before") {
        return Err(format!("a stranger-only capture failed without naming why: {err}"));
    }
    // A missing anchor is a failure, not a fallback to the whole capture.
    let no_marker = Serial::named("test capture", format!("{stranger}\n"));
    if no_marker.must_say_after(READY, "nothing decoded").is_ok() {
        return Err(String::from("must_say_after answered from a capture with no marker in it"));
    }

    // Interleaving is detected and named, and a clean capture reports none.
    let split = Serial::named("test capture", "[kernel 0.001 cpu0] a\nBoot: comp[kernel 0.002 cpu0] lete\n");
    if split.interleaved().is_none() {
        return Err(String::from("a kernel prefix spliced mid-line was not detected"));
    }
    if live.interleaved().is_some() {
        return Err(String::from("a clean capture was reported as interleaved"));
    }
    // And a needle the interleaving split says so rather than "never said it".
    let err = split.must_say("Boot: complete").unwrap_err();
    if !err.contains("interleaved") {
        return Err(format!("a split needle failed without naming the cause: {err}"));
    }

    // **Who died, and the case the naive fix gets wrong.** A wait that ends a
    // run on a bare panic spelling ends it on a *program's* panic too, and a
    // program is expected to be able to die without killing the machine. The
    // prefix is the whole discriminator, so it is asserted from both sides:
    // the same words, once from the kernel and once from somebody else.
    const KERNEL_PANIC_LINE: &str =
        "[kernel 1.450 cpu3] PANIC: panicked at kernel/src/sched/reserve.rs:812:9:";
    const USER_PANIC_LINE: &str = "thread 'main' (1) panicked at sshd/src/main.rs:359:23:";
    let whose: &[(&str, Option<Died>)] = &[
        // The kernel, about itself.
        (KERNEL_PANIC_LINE, Some(Died::Kernel)),
        ("[kernel 0.068 cpu0] KERNEL PANIC: read unmapped address at 0x0", Some(Died::Kernel)),
        // The three that write none of the words beside them. The first is
        // verbatim what a staged `#DF` put on the console.
        (
            "[kernel 0.443 cpu0] DOUBLE FAULT on CPU 0 (pid=Some(Pid(5)) tid=Some(Tid(0)))",
            Some(Died::Kernel),
        ),
        ("[kernel 0.443 cpu0] MACHINE CHECK on CPU 3", Some(Died::Kernel)),
        (
            "[kernel 4.100 cpu0] iommu: DMA FAULT unit0 stream=00:1f.2 addr=0x1000 access=read \
             reason=0x06 unknown",
            Some(Died::Kernel),
        ),
        ("[kernel 0.001 cpu0] EARLY PANIC: nothing is up yet", Some(Died::Kernel)),
        (
            "[kernel 2.000 cpu1] DOUBLE PANIC: the cpu was already in Fatal; first: invalid \
             opcode rip=0x0000000000401234 cr2=0x0000000000000000 err=0x0000000000000000; \
             second: panic at kernel/src/mm/paging.rs:41:5: the page is not there",
            Some(Died::Kernel),
        ),
        // No prefix, and still the kernel's: the reentry line goes out the UART
        // port directly, and no program in this tree says these words.
        ("\n!!! PANIC REENTRY: CPU halted !!! (apic 3)", Some(Died::Kernel)),
        ("KERNEL PANIC: spliced onto somebody's unterminated write", Some(Died::Kernel)),
        // The kernel, about a process. Its line, somebody else's death.
        ("[kernel 0.412 cpu0] SEGFAULT tid=7: read unmapped address at 0x0", Some(Died::Faulted)),
        ("[kernel 0.412 cpu0] SIGILL tid=7: illegal instruction", Some(Died::Faulted)),
        ("[kernel 0.412 cpu0] FATAL tid=7: machine check", Some(Died::Faulted)),
        // A process, about itself. Neither of these ends anybody's run.
        (USER_PANIC_LINE, Some(Died::Panicked)),
        ("libc panic: panicked at src/main.rs:9:1:", Some(Died::Panicked)),
        // The one the naive fix cannot tell from the kernel's, and must.
        ("PANIC: printed by a program that felt like printing it", Some(Died::Panicked)),
        // Nothing died.
        ("[kernel 0.377 cpu0] NVMe: found", None),
        ("hello from userland", None),
        ("", None),
    ];
    for (line, want) in whose {
        let got = died(line);
        if got != *want {
            return Err(format!("{line:?} reads as {got:?}, and it is {want:?}"));
        }
    }
    // The two spellings that decide it, side by side, from both speakers. Stated
    // as its own case because the table above would still pass if `died`
    // ignored the prefix and every kernel-written line simply came first.
    for word in ["PANIC:", "panicked at"] {
        let kernel = format!("[kernel 1.450 cpu3] {word} whatever follows");
        let program = format!("some program says {word} whatever follows");
        if died(&kernel) != Some(Died::Kernel) || died(&program) != Some(Died::Panicked) {
            return Err(format!(
                "{word:?} does not depend on who said it: kernel {:?}, program {:?}",
                died(&kernel),
                died(&program)
            ));
        }
    }
    // And `must_be_clean` still refuses both of them, because a boot that
    // carries either is not a clean boot whoever wrote it.
    for line in [KERNEL_PANIC_LINE, USER_PANIC_LINE] {
        let capture = Serial::named("test capture", format!("[kernel 0.001 cpu0] up\n{line}\n"));
        if capture.must_be_clean().is_ok() {
            return Err(format!("must_be_clean passed a capture carrying {line:?}"));
        }
    }

    // **The report, which is the artefact a verdict used to drop.** Staged as
    // the lines `double_fault_handler` really writes
    // (`kernel/src/arch/idt/exceptions.rs`), with the ordinary run in front of
    // it and a daemon still talking after the header — a capture that begins at
    // the death would be a capture nobody has.
    const DF_HEADER: &str =
        "[kernel 6.204 cpu1] DOUBLE FAULT on CPU 1 (pid=Some(Pid(2)) tid=Some(Tid(0)))";
    let staged_df = format!(
        "[kernel 6.201 cpu0] spawn: /bin/test_rs_console_line_atomicity pid=41\n\
         AAAAAAAA\n\
         {DF_HEADER}\n\
         [kernel 6.204 cpu1]   cr2=0xffff800002672ff8 (address that caused the fault chain)\n\
         [kernel 6.204 cpu1]   rip=0xffffffff80121a40  rsp=0xffff800002673000  rbp=0x0\n\
         [kernel 6.204 cpu1]   Kernel backtrace:\n\
         soundd: suspended\n\
         [kernel 6.205 cpu1]   Found interrupt frame at stack offset +0x18:\n"
    );
    let Some(report) = death_report(&staged_df) else {
        return Err(String::from(
            "a capture carrying a whole double-fault report has no report in it, which is the \
             verdict that threw one away",
        ));
    };
    if !report.starts_with(DF_HEADER) {
        return Err(format!("the report does not start at the death line:\n{report}"));
    }
    // The body, and not merely the header: quoting the sentence again is what
    // the old arms already did.
    for want in ["cr2=0xffff800002672ff8", "rip=0xffffffff80121a40", "Found interrupt frame"] {
        if !report.contains(want) {
            return Err(format!("the report drops {want:?}:\n{report}"));
        }
    }
    // Nothing from before the death, because that is the run going normally and
    // a bound spent on it is a bound not spent on the report.
    if report.contains("spawn: /bin/test_rs_console_line_atomicity") {
        return Err(format!("the report starts before the death:\n{report}"));
    }
    // A line another process wrote *after* the header stays: the console is not
    // line-atomic and a report with holes cut in it is worse than one with a
    // daemon's line in the middle.
    if !report.contains("soundd: suspended") {
        return Err(format!("the report drops the lines it did not recognise:\n{report}"));
    }
    // The other direction, and the one that keeps this out of everybody's
    // terminal: a capture nothing died in has no report at all.
    let healthy = "[kernel 0.377 cpu0] NVMe: found\nBoot: complete\n";
    if death_report(healthy).is_some() {
        return Err(String::from("a clean capture produced a death report"));
    }
    // A *program* dying is not the machine's account either — the same
    // discrimination `died` makes, asked of the thing that quotes a capture.
    let program = format!("[kernel 0.001 cpu0] up\n{USER_PANIC_LINE}\nmore output\n");
    if death_report(&program).is_some() {
        return Err(format!("a program's own panic reads as the kernel's death:\n{program}"));
    }
    // Bounded, and it says by how much rather than trailing off. 400 lines of
    // report is four times what the deepest one in this tree writes.
    let flood: String = std::iter::once(DF_HEADER.to_string())
        .chain((0..400).map(|i| format!("[kernel 6.204 cpu1]   line {i}")))
        .collect::<Vec<_>>()
        .join("\n");
    let bounded = death_report(&flood).ok_or("a 401-line report vanished")?;
    if bounded.lines().count() > REPORT_LINES + 1 {
        return Err(format!(
            "the report is unbounded: {} lines of a {}-line capture",
            bounded.lines().count(),
            flood.lines().count()
        ));
    }
    if !bounded.contains("of the 401 lines that followed") {
        return Err(format!(
            "a truncated report does not say how much it dropped:\n{}",
            bounded.lines().next().unwrap_or_default()
        ));
    }

    eprintln!(
        "  [serial] {} vocabulary cases, both directions, plus the anchored scan against a \
         stranger line, {} lines classified by who said them, and a {}-line double-fault report \
         recovered from a capture that also carried a daemon and a program's panic",
        cases.len(),
        whose.len(),
        report.lines().count(),
    );
    Ok(())
}
