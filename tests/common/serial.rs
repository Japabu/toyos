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

/// Panic markers. One list, so a test cannot scan for two of the three.
const FATAL: &[&str] = &["PANIC:", "KERNEL PANIC", "panicked at"];

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
    /// `specs/issues/`). Reported rather than repaired — a needle that went
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
    /// (`specs/issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md`).
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

    /// Nothing panicked. The one place the marker list lives.
    pub fn must_be_clean(&self) -> Result<(), String> {
        for bad in FATAL {
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

    let cases: &[(&str, bool, &dyn Fn() -> Result<(), String>)] = &[
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

    eprintln!(
        "  [serial] {} vocabulary cases, both directions, plus the anchored scan against a \
         stranger line",
        cases.len()
    );
    Ok(())
}
