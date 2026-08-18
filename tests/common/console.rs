//! The console's line atomicity, and the one thing the harness may conclude
//! from it.
//!
//! Two subjects, in this order because the second rests on the first:
//!
//! 1. [`console_line_atomicity`] — every line on the console is one writer's,
//!    whole. `specs/log-architecture-spec.md` §4.4 and §9.5.
//! 2. [`verdict`] — therefore a line's *first bytes are its writer's own*, so
//!    the C family can tell a daemon's line from the program under test's by
//!    reading it, and stop failing on output that is not its own.
//!
//! The order is the argument, and it is the whole of what closed task #84.
//! Before L5 a daemon's `println!` and a test's could reach the backend in
//! pieces and arrive spliced into one line, and no rule over lines could
//! separate them — so the standing write-up said there was no cheap honest fix
//! and left a choice between giving each child a capture channel and tagging
//! every console write with its writer. L5 built neither and made a third
//! answer sound: a console *is* a per-holder line buffer now, so a line begins
//! with its writer's first bytes or `console_line_atomicity` is red.
//!
//! **L5's guarantee is about flushes, not about newlines, and that is the
//! second half.** A program that writes without a trailing newline has its
//! bytes joined to the next writer's line by the *host's* splitter — see
//! [`speaker_at`], which is where that is written down. Both halves are
//! [`c_capture_ignores_daemon_lines`]'s, and every one of its verdicts carries
//! the control that says it has teeth.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
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

// --- What the C family may conclude from a shared console ---

/// The `system.toml` the C family boots, every time.
///
/// [`verdict`] is called from a comparison that has a capture and no guest, so
/// the config is named here rather than passed: the whole registry boots
/// `tests/testcases` and [`c_capture_ignores_daemon_lines`] asserts that the
/// machine it staged the gate on is this one, so the day a second config runs C
/// tests the gate says so instead of the filter quietly deriving its names from
/// the wrong image.
fn config() -> PathBuf {
    super::compile::repo_root().join("tests/testcases/system.toml")
}

/// Every name a process on that boot speaks its console lines in.
///
/// Derived from the config by `toyos_build::build::console_speakers` and cached
/// once — a list written here would be a list that goes stale the next time a
/// daemon joins `[boot] start`, which is precisely how this defect would come
/// back.
fn speakers() -> &'static BTreeSet<String> {
    static SPEAKERS: OnceLock<BTreeSet<String>> = OnceLock::new();
    SPEAKERS.get_or_init(|| toyos_build::build::console_speakers(&config()))
}

/// Whose line this is, when it is not the program under test's.
///
/// A prefix and nothing cleverer, because after L5 that is exactly what the
/// wire carries: `ConsoleObject` is one line buffer per holder, so the bytes at
/// the front of a console line were written by the process the line belongs to.
/// The shape is `<name>: ` — what every daemon in this tree prints, and what
/// `/bin/init` prints when it speaks in one of their names before it has
/// started them.
fn speaker_of<'a>(line: &str, speakers: &'a BTreeSet<String>) -> Option<&'a str> {
    let (head, rest) = line.split_once(':')?;
    // `soundd: ready` and a bare `soundd:`, and nothing else — `main: x` from a
    // C case is a colon in the middle of a word, and `a:b` is not a speaker's
    // line whatever `a` is.
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    speakers.get(head).map(String::as_str)
}

/// Where a daemon's whole line begins inside a captured line — which is not
/// always at its front.
///
/// **The half of this defect that no prefix rule reaches, and it is not a
/// splice.** L5's guarantee is about what the kernel emits: every *flush* is
/// one holder's bytes. It says nothing about where a **newline** is, and the
/// host's line splitter is `BufReader::lines()`. A program that writes without
/// a trailing newline — `71_macro_empty_arg` is `printf("%d", …)` and nothing
/// else — leaves `17` on the wire unterminated, and the next writer's whole
/// line is appended to it by the splitter, not by the kernel. Measured on this
/// tree, 2026-08-15: `17init: started test-runner` in one captured line, with
/// `17` expected. The same shape joins the two halves of a line longer than
/// `MAX_CONSOLE_LINE`, which the kernel does emit in pieces:
/// `90_stdio_buffering` prints a 10,000-byte line against that 1024-byte bound
/// and is the only case in the corpus that does.
///
/// So a daemon's unit is `<speaker>: …` up to the newline that ended it, and it
/// can start anywhere in a captured line. Found by walking the colons rather
/// than every offset, because `90_stdio_buffering`'s ten thousand `x`s hold
/// none and a per-offset search would read them ten thousand times.
fn speaker_at(line: &str, speakers: &BTreeSet<String>) -> Option<usize> {
    for (colon, _) in line.match_indices(':') {
        for name in speakers {
            let Some(start) = colon.checked_sub(name.len()) else { continue };
            if line.is_char_boundary(start)
                && &line[start..colon] == name.as_str()
                && speaker_of(&line[start..], speakers).is_some()
            {
                return Some(start);
            }
        }
    }
    None
}

/// What the C family concluded from one capture, and what it took out first.
pub struct Verdict<'a> {
    /// Each whole line another process wrote, removed before the comparison —
    /// as it stood on the wire, which is from where it started to the newline
    /// that ended it, and not necessarily a whole captured line.
    ///
    /// **Kept and printed either way, never dropped.** The removal is a claim
    /// about who wrote a line, and a claim that nobody can see is a capture
    /// quietly getting shorter; on a red these are usually the whole
    /// explanation.
    pub filtered: Vec<&'a str>,
    /// `None` is a match.
    pub mismatch: Option<String>,
}

/// Compare a C test's capture against its `.expect`, ignoring lines that are
/// some other process's.
///
/// **The scope boundary, and it is the whole safety argument.** This is the C
/// family's stdout comparison and nothing else. Every other reader of a
/// daemon's line — the audio gates counting soundd's stats, `netd_*` waiting on
/// `netd: ready`, the sshd tests reading its host identity, the log gates —
/// reads `TestResult::serial` or a boot log, which this never touches. Those
/// tests *assert on* a daemon's line; this family is the one for which a
/// daemon's line is by construction not the subject, because the subject is a
/// C program's own stdout against a file recorded from it.
///
/// Which is also why the filter cannot make a broken case pass: a tinycc case's
/// output is decided by its source, so the only way `soundd: …` appears in one
/// is that the source prints it — and then the `.expect` declares it, and the
/// refusal below fires by name rather than the line being silently eaten. 0 of
/// the 153 expectations contain such a substring anywhere, measured, and
/// [`c_capture_ignores_daemon_lines`] re-measures it every run.
pub fn verdict<'a>(
    stdout: &'a str,
    expected: &str,
    speakers: &BTreeSet<String>,
) -> Verdict<'a> {
    let mut mine = String::new();
    let mut filtered = Vec::new();
    for line in stdout.lines() {
        match speaker_at(line, speakers) {
            // **The newline this captured line ended with was the daemon's, so
            // it is removed with the rest of that unit and no line break takes
            // its place.** That is what puts a program's unterminated `17` back
            // beside its own next bytes instead of leaving `17init: started
            // test-runner`, and what rejoins the two halves of a line the
            // kernel emitted in `MAX_CONSOLE_LINE` pieces. `Some(0)` — a
            // daemon's line arriving on its own, the ordinary case — falls out
            // of the same arm with an empty head.
            Some(at) => {
                mine.push_str(&line[..at]);
                filtered.push(&line[at..]);
            }
            None => {
                mine.push_str(line);
                mine.push('\n');
            }
        }
    }

    // The one thing this may never do: remove a line the case exists to print.
    // Refused by name — a filter that made an exception for such a case would
    // be a filter nobody could reason about afterwards.
    if let Some(declared) = expected.lines().find(|l| speaker_at(l, speakers).is_some()) {
        return Verdict {
            filtered,
            mismatch: Some(format!(
                "the expectation declares {declared:?}, and this comparison attributes that \
                 line to another process and removes it from the capture — so the case's own \
                 output would be filtered away. Change what the case prints, or take the name \
                 out of the boot config. The speakers this boot declares are {:?}",
                speakers.iter().collect::<Vec<_>>(),
            )),
        };
    }

    let mismatch = (mine.trim_end() != expected.trim_end()).then(|| {
        format!(
            "output mismatch\n--- expected ---\n{}\n--- what this program wrote ---\n{}",
            expected.trim_end(),
            mine.trim_end(),
        )
    });
    Verdict { filtered, mismatch }
}

/// [`verdict`] against the boot config the C family runs on.
pub fn c_verdict<'a>(stdout: &'a str, expected: &str) -> Verdict<'a> {
    verdict(stdout, expected, speakers())
}

/// The line the gate has a guest write inside a capture window on purpose.
///
/// `soundd` because that is the daemon the write-up caught doing this, and the
/// text is a sentence no `.expect` in the corpus contains.
const IMPOSTOR: &str = "soundd: capture window gate line";

/// The same line with the speaker taken off the front, which is what a C
/// program's own output looks like. The pair is the whole gate: one has to go
/// and the other has to stay, and a filter that got either wrong would pass
/// only one of them.
const MINE: &str = "capture window gate line";

/// A daemon's line inside a C test's window no longer decides that test.
///
/// Deterministic, because nothing here waits for the race: a guest process
/// writes [`IMPOSTOR`] *into* a real capture window on purpose, and the real
/// comparison is then run over the real capture. Four verdicts, and the last
/// two are the controls that stop this from being a gate that would pass on a
/// filter which removed everything or nothing:
///
/// 1. the impostor lands in the window whole, which is L5's guarantee and this
///    fix's premise — a spliced line would fail the equality, not a `contains`;
/// 2. the comparison ignores it, and names it as ignored;
/// 3. **filter off** (an empty speaker set) and the same capture reds — so the
///    filter is what makes 2 pass and not something else;
/// 4. a line no speaker owns survives, and an expectation that omits it still
///    reds — so the filter removes daemons' lines and not the program's.
pub fn c_capture_ignores_daemon_lines(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    // The filter derives its names from one config; this gate stages its
    // evidence on a guest. They have to be the same machine or the evidence is
    // about a different image than the one the C family runs on.
    if test_config.join("system.toml") != config() {
        return Err(format!(
            "this gate boots {} and `verdict` derives its speakers from {} — the evidence \
             would be about a different image than the one the C family runs on",
            test_config.join("system.toml").display(),
            config().display(),
        ));
    }
    let speakers = speakers();
    // Non-vacuity: an empty or truncated set filters nothing and every
    // assertion below would still be satisfiable by a capture with no daemon
    // line in it.
    for want in ["init", "logd", "soundd"] {
        if !speakers.contains(want) {
            return Err(format!(
                "the speakers derived from {} are {:?} and do not include `{want}` — either \
                 the config stopped starting it or the derivation is reading the wrong file",
                config().display(),
                speakers.iter().collect::<Vec<_>>(),
            ));
        }
    }

    // The scope boundary, asserted against the corpus rather than described.
    // Every `.expect` the C family compares against is checked here, so a case
    // whose own output would be filtered is caught by this gate rather than by
    // whichever suite happened to run it.
    let dir = super::compile::testcases_dir();
    let mut declaring: Vec<String> = Vec::new();
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    let mut checked = 0usize;
    for entry in entries {
        let path = entry.map_err(|e| format!("walk {}: {e}", dir.display()))?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("expect") {
            continue;
        }
        checked += 1;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        for line in text.lines() {
            // `speaker_at` and not `speaker_of`: the removal reaches a daemon
            // unit anywhere in a captured line, so the corpus has to be clear of
            // one anywhere and not only at the front.
            if let Some(at) = speaker_at(line, speakers) {
                declaring.push(format!(
                    "{}: {:?} reads as another process's",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    &line[at..],
                ));
            }
        }
    }
    if !declaring.is_empty() {
        return Err(format!(
            "{} expectation(s) contain text this comparison would remove from the capture, so \
             the case could never match its own output:\n{}",
            declaring.len(),
            declaring.join("\n"),
        ));
    }
    if checked == 0 {
        return Err(format!("{} holds no `.expect` file at all", dir.display()));
    }

    let mut qemu = QemuInstance::boot(test_config, c_bins, rust_bins);

    // Zero, and the assertion that keeps the derivation honest as the tree
    // grows: **every line this boot's userland wrote is one this set can
    // account for.** A daemon added tomorrow that speaks in a name the config
    // does not declare would otherwise rejoin the defect silently — its line
    // would reach a C test's window and decide that test's verdict, at the
    // family's own rate, from a name nobody knew to look for. Here it is a red
    // on this gate, with the line quoted.
    //
    // It is also what found `virtio-sound:` — soundd's driver layer speaks in
    // the *device's* name, so `[programs]` keys alone were never the set.
    let unattributed: Vec<&str> = qemu
        .boot_log()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !super::qemu::is_kernel_line(l))
        // The runner's own protocol, which is not a console writer's sentence.
        .filter(|l| !l.contains(super::qemu::DEFAULT_READY))
        .filter(|l| speaker_at(l, speakers).is_none())
        .collect();
    if !unattributed.is_empty() {
        return Err(format!(
            "this boot's userland wrote {} line(s) that no name in {:?} accounts for, so a C \
             test whose window one of them lands in would fail on it:\n{}\n\
             Add whatever declares them to the boot config — the set is derived from \
             `[programs]`, their `devices` and `[boot] start`, and never listed in the harness.",
            unattributed.len(),
            speakers.iter().collect::<Vec<_>>(),
            unattributed.join("\n"),
        ));
    }

    // One. A real process writes a daemon-shaped line inside a real window.
    let staged = qemu.run_test(&format!("echo {IMPOSTOR}"), CEILING);
    if let Some(err) = &staged.error {
        return Err(format!("staging the impostor line: {err}\n{}", tail(&staged.stdout)));
    }
    if !staged.stdout.lines().any(|l| l == IMPOSTOR) {
        return Err(format!(
            "the guest wrote {IMPOSTOR:?} and the capture has no such line — equality and not \
             `contains`, because a line arriving spliced with another writer's is what makes \
             attribution by prefix unsound in the first place. The capture was:\n{}",
            tail(&staged.stdout),
        ));
    }

    // Two. The comparison the C family makes ignores it, and says it did.
    let ignored = c_verdict(&staged.stdout, "");
    if let Some(mismatch) = &ignored.mismatch {
        return Err(format!(
            "a daemon-shaped line inside the window still decides a C test's verdict, which \
             is the defect this gate exists for:\n{mismatch}"
        ));
    }
    if !ignored.filtered.iter().any(|l| *l == IMPOSTOR) {
        return Err(format!(
            "the comparison passed without naming {IMPOSTOR:?} among the lines it removed — a \
             capture that silently got shorter is not evidence. It named {:?}",
            ignored.filtered,
        ));
    }

    // Three, the negative control: with nothing declared as a speaker, the same
    // capture reds. If it did not, step two would prove nothing about the
    // filter.
    let unfiltered = verdict(&staged.stdout, "", &BTreeSet::new());
    if unfiltered.mismatch.is_none() {
        return Err(format!(
            "with an empty speaker set the same capture still compares equal to an empty \
             expectation — so the filter is not what made this pass and this gate has no \
             teeth. The capture was:\n{}",
            tail(&staged.stdout),
        ));
    }

    // Four. The other direction: a line no speaker owns is the program's, and
    // it both survives the filter and still reds an expectation that omits it.
    let ordinary = qemu.run_test(&format!("echo {MINE}"), CEILING);
    if let Some(err) = &ordinary.error {
        return Err(format!("staging the ordinary line: {err}\n{}", tail(&ordinary.stdout)));
    }
    let kept = c_verdict(&ordinary.stdout, MINE);
    if let Some(mismatch) = &kept.mismatch {
        return Err(format!(
            "a line no speaker owns did not survive the filter, so this removes the program's \
             own output:\n{mismatch}"
        ));
    }
    let blanket = c_verdict(&ordinary.stdout, "");
    if blanket.mismatch.is_none() {
        return Err(format!(
            "a capture carrying {MINE:?} compares equal to an *empty* expectation — the filter \
             is removing everything rather than one process's lines"
        ));
    }

    // Five. The half a whole-line rule cannot reach, staged as the captures the
    // wire actually produced rather than as a guess about them. Both of these
    // are transcribed from a run of this suite on 2026-08-15, and both are
    // *one* captured line: the host splits on newlines, and the newline the
    // program never wrote is the reason its bytes and somebody else's share a
    // line at all.
    let joined: &[(&str, &str, &str)] = &[
        // `71_macro_empty_arg` is `printf("%d", …)` and nothing after it, so
        // its `17` reaches the wire with no terminator and init's next whole
        // line is appended to it by the splitter.
        ("17init: started test-runner\n", "17", "the program's unterminated tail"),
        // The other joiner, and the only case in the corpus that reaches it:
        // `90_stdio_buffering` prints a line ten times `MAX_CONSOLE_LINE`, so
        // the kernel does emit it in pieces and a daemon's line lands between
        // two of them. The two halves have to come back as one line.
        ("aaasoundd: suspended\nbbb\n", "aaabbb", "a line the kernel emitted in pieces"),
        // And the ordinary case still has to work the ordinary way.
        ("one\nsoundd: suspended\ntwo\n", "one\ntwo", "a daemon's line between two of the program's"),
    ];
    for (capture, want, what) in joined {
        let got = verdict(capture, want, speakers);
        if let Some(mismatch) = &got.mismatch {
            return Err(format!(
                "{what}: the capture {capture:?} does not read back as {want:?}\n{mismatch}"
            ));
        }
        if got.filtered.is_empty() {
            return Err(format!(
                "{what}: {capture:?} matched {want:?} while removing nothing, so the two were \
                 equal already and this row proves nothing"
            ));
        }
        // The control, per row: without the speakers there is nothing to
        // remove and each of these must red.
        if verdict(capture, want, &BTreeSet::new()).mismatch.is_none() {
            return Err(format!(
                "{what}: {capture:?} reads back as {want:?} with an empty speaker set too, so \
                 this row is not testing the removal"
            ));
        }
    }

    // Six, end to end: the corpus case whose output has no trailing newline, on
    // a real guest. It is `c_bins`' own binary and this boot carries it.
    let unterminated = qemu.run_test("test_c_71_macro_empty_arg", CEILING);
    if let Some(err) = &unterminated.error {
        return Err(format!("running the unterminated case: {err}"));
    }
    let read_back = c_verdict(&unterminated.stdout, "17");
    if let Some(mismatch) = &read_back.mismatch {
        return Err(format!(
            "a C program that wrote `17` and no newline did not read back as its own output — \
             this is the capture losing its tail, whichever writer followed it:\n{mismatch}"
        ));
    }

    eprintln!(
        "  [console] {} speakers declared by tests/testcases/system.toml, {checked} \
         expectations clear of them, every userland line of the boot attributed; {IMPOSTOR:?} \
         written inside a capture window and ignored, {MINE:?} kept, {} joined captures read \
         back whole; every control red",
        speakers.len(),
        joined.len(),
    );
    Ok(())
}

/// A pending poll on stdin is not something the keyboard *claim* closing can
/// cancel.
///
/// **`Source::Keyboard` is named by two kinds of object and only one of them
/// can end it.** `io_uring::remove_fd` cancels by source across every ring in
/// the machine, and `object::ops::close` used to decide whether to call it by
/// asking the object: `Device(_)` answered yes, on the argument that a claim
/// admits exactly one handle so every ring watching it is the one holder's. The
/// condition it needed was about the *source* — that no other **kind** of object
/// names it — and every `Console` names `Source::Keyboard` too. So the one
/// process holding the keyboard claim closing its handle posted `-NotFound` into
/// every pending `POLL_ADD` on stdin in the machine, which is what libc's
/// terminal read arms.
///
/// The guest half is `userland/test-runner/src/kbd_close.rs` and it carries all
/// three verdicts; the host owes it one keystroke, which is the only thing that
/// can complete a poll on `Source::Keyboard` and therefore the only way to show
/// that what survived the close was a live registration.
///
/// **`Profile::Metal` because the keystroke has to arrive.** Its i8042 is the
/// only keyboard on the machine — no USB HID, no virtio — which is the shape
/// `i8042_keyboard` and `swiss_german_layout` already inject through, and the
/// mouse the middle arm claims is the PS/2 one beside it.
pub fn keyboard_claim_close_spares_stdin(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    /// What the guest prints once both claim arms have run.
    const READY: &str = "===KBD_CLOSE_READY===";

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: super::qemu::Profile::Metal,
            qmp: true,
            ..Default::default()
        },
    );
    // One tap, injected only after the guest says it is armed. The hook runs
    // inside the console read loop, which is the one place "the poll is
    // registered" and "the host has not injected yet" are both true.
    let result = qemu.run_test_hooked("kbd-close", CEILING, READY, |socket| {
        super::qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
    });
    if let Some(err) = &result.error {
        return Err(format!("{err}\nstdout:\n{}", result.stdout));
    }
    if result.exit_code != Some(0) || !result.stdout.contains("kbd-close: OK") {
        return Err(format!(
            "the keyboard-close probe exited {:?}\n{}",
            result.exit_code, result.stdout
        ));
    }
    let survived = result
        .stdout
        .lines()
        .find(|l| l.contains("kbd-close: survived="))
        .ok_or_else(|| format!("the guest never said what it saw\n{}", result.stdout))?;
    eprintln!("  [console] {}", survived.trim());
    Ok(())
}
