use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{fs, thread};

use super::compile;

/// When true, serial output is printed to stderr as it arrives.
pub static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Distinguishes every file one QEMU boot owns from every other boot's within
/// one test process — the wav capture, the UART log, the QMP socket, the
/// screendump, and the bootable image itself.
static BOOT_SEQ: AtomicU32 = AtomicU32::new(0);

/// Guests that have been booted and not yet dropped.
///
/// Gate A's numbers were recorded with one QEMU on the host and nothing else
/// (`tests/audio-baseline.toml`), so "the parallel phase has drained" is a
/// precondition of the audio block rather than a property of where it sits in
/// `main`. This is what lets it be asserted instead of arranged — see
/// [`live_instances`].
static LIVE: AtomicU32 = AtomicU32::new(0);

/// How many guests are up right now, across every thread.
pub fn live_instances() -> u32 {
    LIVE.load(Ordering::SeqCst)
}

/// The NVMe backing files live guests are holding open.
///
/// A lane reuses one image across its boots on purpose ([`super::lane`]), so
/// "one image, one guest" is an invariant this harness already believed and
/// nothing checked. QEMU checks it — it takes an exclusive `write` lock and the
/// second process exits 1 — but it checks it *after* the first one is unusable,
/// on stderr, in a sentence about locks that says nothing about which two boots
/// overlapped. This is the same claim, made before anything spawns and in the
/// harness's own words.
static NVME_HELD: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// One live guest's hold on the NVMe image it was given.
///
/// Taken before the QEMU process is spawned and released when the
/// [`QemuInstance`] is dropped — including when `wait_for_ready` panics on its
/// way out, which builds no instance to drop and so must not leave a hold
/// behind either.
pub struct NvmeClaim {
    path: PathBuf,
    /// A profile declaring no NVMe controller is handed no image: the path is
    /// `no-nvme`, it never reaches QEMU's argv, and every lane's is the same
    /// name. There is nothing to hold and nothing to conflict with.
    held: bool,
}

impl NvmeClaim {
    /// Hold `path` for a guest that is about to be launched with it.
    ///
    /// The refusal is returned rather than raised because it is what
    /// `nvme_image_is_held_by_one_guest` stages: [`QemuInstance::boot_with_options`]
    /// panics on it, since a lane whose image is already open cannot boot and
    /// there is nothing else to do about that.
    pub fn take(path: &Path) -> Result<Self, String> {
        // Decided under the lock and raised after it: a panic with the guard
        // held poisons the mutex, and one refusal would then become a refusal
        // on every later boot in the process — the shape this whole entry is
        // about.
        let refusal = {
            let mut held = NVME_HELD.lock().unwrap_or_else(|e| e.into_inner());
            match nvme_conflict(&held, path) {
                Some(why) => Some(why),
                None => {
                    held.insert(path.to_path_buf());
                    None
                }
            }
        };
        match refusal {
            Some(why) => Err(why),
            None => Ok(Self { path: path.to_path_buf(), held: true }),
        }
    }

    /// The image a profile with no controller names and never uses.
    pub fn unattached(path: &Path) -> Self {
        Self { path: path.to_path_buf(), held: false }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for NvmeClaim {
    fn drop(&mut self) {
        if self.held {
            NVME_HELD.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.path);
        }
    }
}

/// Why a boot may not open `want`, given what live guests are already holding.
///
/// Pure, and every input a parameter, so both directions can be staged without
/// a guest.
pub fn nvme_conflict(held: &std::collections::BTreeSet<PathBuf>, want: &Path) -> Option<String> {
    held.contains(want).then(|| {
        format!(
            "a live guest is still holding {}. QEMU takes an exclusive write lock on the image \
             it is given, so the second process exits 1 before it says anything and the boot \
             that waited on it panics — which is how one lost guest reported 129 tests red on \
             2026-08-17. A guest that replaces another must be built from that one's \
             `QemuInstance::shutdown`, which takes it by value; `qemu = boot()` evaluates its \
             right-hand side first and launches the replacement while the old guest is up.",
            want.display()
        )
    })
}

/// Proof that no guest is holding a lane's images.
///
/// There are two ways to have one and there is no third: a lane that has not
/// booted anything yet ([`LaneFree::no_guest_yet`]), and a guest that has been
/// ended ([`QemuInstance::shutdown`], which takes `self`). A boot that takes
/// this by value therefore *cannot be written* before the guest it replaces is
/// gone — which is the mistake `qemu = boot()` makes, because Rust evaluates
/// the right-hand side first.
#[must_use]
pub struct LaneFree(());

impl LaneFree {
    /// Before a lane's first boot, where there is no guest to end.
    pub fn no_guest_yet() -> Self {
        Self(())
    }
}

/// Guests this run has started, how many of them were not the shipping kernel,
/// and every distinct kernel build it asked cargo for.
///
/// A registration is not a boot — several tests boot two machines and one boots
/// four — so the count that decides whether a scheduling or build change worked
/// cannot be read off the test lists. It was static analysis until now, which
/// only ever gave a lower bound.
///
/// **The third is the one this run is judged on.** A kernel build is ~6.9 s of
/// wall clock and ~29.6 s of CPU after any edit to `kernel/`, and
/// until 2026-08-10 a full run made 45 of them. The set is what a run reports
/// and what [`declared_kernel_builds`] refuses an addition to.
///
/// **A boot that stages its own image builds nothing and counts nothing here.**
/// It used to build the image it then threw away — so the run reported a kernel
/// build no guest booted and counted the boot as one that was not the shipping
/// kernel, on a guest that was. What a boot with a staged image contributes is
/// what that image was built with, and the build that made it counted itself
/// (`qemu::build_boot_image`) at the point cargo was actually asked.
static BOOTS: AtomicU32 = AtomicU32::new(0);
static FEATURE_BOOTS: AtomicU32 = AtomicU32::new(0);
static KERNELS: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// `(boots, boots that were not the shipping kernel, the kernels built)`.
pub fn boot_census() -> (u32, u32, Vec<String>) {
    (
        BOOTS.load(Ordering::Relaxed),
        FEATURE_BOOTS.load(Ordering::Relaxed),
        KERNELS.lock().expect("the kernel census").iter().cloned().collect(),
    )
}

/// The kernel builds an ordinary suite run is allowed to make, and the whole
/// list.
///
/// `""` is what an image ships. [`toyos_build::build::TEST_KERNEL`] is every
/// actuator compiled in, armed by boot parameter. `fpu-save-nothing` is the one
/// actuator that could not become a parameter — it takes the `fxsave64` out of
/// `arch::entry`'s `naked_asm!` bracket, which is the path its own gate is
/// about.
///
/// [`toyos_build::build::SCHED_CHECK_KERNEL`] is the fourth, and it is the one
/// this list's own warning was written about: an entry here is a decision to pay
/// a kernel build per suite run forever, made in the shared declaration rather
/// than by adding a `kernel_features` to a `BootOptions`. It was made because
/// the alternative had already been paid for and delivered nothing —
/// `kernel/Cargo.toml` has forwarded `sched-check = ["toyos-sched/check"]` since
/// the check build was written, and nothing in `src/` or `tests/` ever asked for
/// it, so `cpu::MAX_PASS_NS`, the pass-cost recorder and `invariants::check_cpu`
/// were compiled by no CI run at all. `sched_check_build` is the test that asks,
/// and `common::passcost` is what judges the half of it that is a measurement.
///
/// A fifth entry is that decision again, and it gets this paragraph's argument
/// made afresh. Interactive debug mode is separate: it builds
/// [`toyos_build::build::DEBUG_KERNEL_BUILD`] and returns before the suite.
pub const DECLARED_KERNEL_BUILDS: [&str; 4] =
    toyos_build::build::TEST_SUITE_KERNEL_BUILDS;

/// How many guests the phase now running may have up at once.
///
/// The harness's own wall-clock margins are margins on the *host*, and they were
/// all derived when one guest had it to itself. Four guests is a different
/// machine, so such a margin has to be stated against the regime it runs in
/// rather than widened outright — which is what this multiplies. A serial phase
/// sets it back to 1 and gets the number it always had.
static WIDTH: AtomicU32 = AtomicU32::new(1);

pub fn set_width(width: u32) {
    assert!(width >= 1, "a phase runs at least one guest");
    WIDTH.store(width, Ordering::SeqCst);
}

/// A liveness ceiling, stated for one guest and paid out for the phase's.
///
/// Every timeout a test hands [`QemuInstance::run_test`] and its relatives is a
/// guard against a wedge, never a verdict: the assertion is what the guest
/// *said*, and a test whose pass depended on a deadline expiring would be
/// asserting on the host's clock. So the number in the source stays the number
/// its author reasoned about — one guest, this host — and the phase multiplies
/// it, exactly as `wait_for_ready` has multiplied the boot timeout since the
/// parallel phase landed.
///
/// The cost of getting this wrong in the generous direction is that a wedge
/// takes longer to report. The cost in the other direction is a red run that
/// says a guest hung when it was only sharing a machine, which is the failure
/// mode that put the whole shared block in the serial tail.
pub fn budget(one_guest: Duration) -> Duration {
    let (num, den) = host_scale();
    one_guest * WIDTH.load(Ordering::SeqCst) * num / den
}

/// The fastest boot-to-ready this run has seen, in milliseconds.
///
/// A boot is the one piece of guest work every test does and no test asserts on
/// — `wait_for_ready`'s own comment names the two exceptions, and both read the
/// guest's stamps rather than this clock — so it is a measurement of the host
/// that costs nothing to take. The *fastest* rather than the mean because a boot
/// taken with three other guests up measures the phase; the minimum over a run
/// is the closest this can get to the machine with nothing else on it.
static FASTEST_BOOT_MS: AtomicU32 = AtomicU32::new(u32::MAX);

/// The same measurement on the host every ceiling in this tree was written for.
///
/// Dev host, M4 Pro, cross-arch TCG, measured 2026-08-08: the fastest of ten
/// boots at `--jobs 1` was 1433 ms against a 1433–2063 ms spread, and the
/// fastest of a whole 291-test suite at width 12 was this. The smaller of the
/// two is the one to hold, because the factor only widens and a reference set
/// too high is a correction that does not happen.
const REFERENCE_BOOT_MS: u32 = 1320;

fn record_boot(took: Duration) {
    let ms = took.as_millis().min(u32::MAX as u128) as u32;
    FASTEST_BOOT_MS.fetch_min(ms, Ordering::SeqCst);
}

/// How much slower than the host these ceilings were written on this one is, as
/// a fraction so that a 1.4× host is not rounded to 1.
///
/// [`budget`] corrects a ceiling for how many guests share the machine. It never
/// corrected for how fast the machine *is*, and that is the other half of the
/// same mistake: a number reasoned about on an M4 Pro is not a liveness ceiling
/// on a four-core Azure vCPU, it is a verdict about which of the two is running
/// the test. 307 bare timeouts were counted in one CI run and every one of
/// them was that.
///
/// **Only ever upward.** On a faster host the number in the source stands,
/// because it is the number its author reasoned about, and a ceiling that shrank
/// would start reporting wedges that are not there. The ceiling of 8 is because
/// one anomalous boot must not be able to disable every liveness guard in the
/// suite at once.
fn host_scale() -> (u32, u32) {
    let fastest = FASTEST_BOOT_MS.load(Ordering::SeqCst);
    // Before the first boot there is no measurement, and the sentinel must not
    // read as the slowest host imaginable.
    if fastest == u32::MAX || fastest <= REFERENCE_BOOT_MS {
        return (1, 1);
    }
    (fastest.min(REFERENCE_BOOT_MS * 8), REFERENCE_BOOT_MS)
}

/// The fastest boot seen, the reference, and the scale it produced — for a run's
/// own report, because the number in the source is no longer the number that was
/// enforced.
pub fn host_speed() -> (Option<u32>, u32, u32, u32) {
    let (num, den) = host_scale();
    let fastest = FASTEST_BOOT_MS.load(Ordering::SeqCst);
    ((fastest != u32::MAX).then_some(fastest), REFERENCE_BOOT_MS, num, den)
}

/// A liveness guard that watches the guest instead of the host's clock.
///
/// [`budget`] corrects a ceiling for how many guests share the machine, which
/// is the part of "how fast is the host today" the harness knows. It does not
/// know the rest, and a retry loop bounded by elapsed time has that ceiling for
/// a *verdict* the moment the rest moves: a guest that is merely late reports
/// exactly what a wedged one reports. `issues/design-debt/` is the bill —
/// `desktop_audio_client` 385 s wide against 13 s alone, a landing gate that is
/// a coin toss, and six reds in four suites every one of which was
/// `ALONE: GREEN`.
///
/// The two are distinguishable and the console is what distinguishes them: a
/// guest still printing is a guest still working. So the ceiling here is time in
/// which **nothing arrived**, and a guest that keeps talking is given as long as
/// it needs. That is the whole idea — no number in this type is a statement
/// about the host.
///
/// `total` is the second half, and it is a wedge guard rather than a verdict
/// too. A guest can be stuck and chatty: the compositor prints an interval line
/// every two seconds whatever else has stopped, so silence alone cannot end a
/// desktop loop and a suite that never ends is worse than one that reds.
///
/// The caller owns the capture, so progress is "did it grow" and costs nothing.
pub struct Liveness {
    quiet_for: Duration,
    last_growth: Instant,
    seen: usize,
    give_up: Instant,
}

impl Liveness {
    /// `quiet_for` of silence ends the wait, and so does `total` however loud
    /// the guest is.
    pub fn new(quiet_for: Duration, total: Duration) -> Self {
        let now = Instant::now();
        Self { quiet_for, last_growth: now, seen: 0, give_up: now + total }
    }

    /// Whether the guest may still be working, given everything it has said.
    pub fn working(&mut self, capture: &str) -> bool {
        if capture.len() != self.seen {
            self.seen = capture.len();
            self.last_growth = Instant::now();
        }
        let now = Instant::now();
        now < self.give_up && now.duration_since(self.last_growth) < self.quiet_for
    }

    /// What ended the wait, for a caller putting it in a failure message.
    pub fn why(&self) -> &'static str {
        if Instant::now() >= self.give_up {
            "it never stopped talking and never got there"
        } else {
            "it went quiet"
        }
    }
}

/// How the reason line begins when what expired was a **guard** and not an
/// assertion.
///
/// A test that ran out of time has not found the guest doing the wrong thing;
/// it has found nothing at all, and the two readings send an agent to opposite
/// places. `screen_pager_keys` reporting `0 page moves over 30 keystrokes`
/// after 0.3 s was bisected as a kernel regression twice in one day by two
/// agents, and the fact it was hiding is that the whole run had collapsed
/// before the guest could answer once.
///
/// Still red. A guest that stopped answering may have stopped for a reason this
/// tree owns, and a status that is not a failure is a status nobody reads. What
/// this buys is that the summary says which of the two kinds of red it is.
///
/// It lives here rather than beside the classifier because [`Liveness`] is what
/// produces the evidence for it, and [`QemuInstance::run_test_paced`] is a
/// second producer: a test's own ceiling is a guard of exactly this kind.
pub const STALLED: &str = "STALLED:";

/// How long a guest may say nothing before a wait on it is a stall.
///
/// Every config these waits run on has something on a periodic interval — the
/// compositor's frame batch and soundd's stats window are both about 2 s — so
/// silence here is a machine that has stopped rather than a machine that is
/// thinking. It is not a verdict about any of them: no assertion in this suite
/// is satisfied by the guest merely talking.
///
/// The other side of that coin: on a shared boot the kernel itself is one of
/// the periodic speakers, on a 10 s cadence, so a wait whose predicate never
/// comes true is never ended by this bound — the guest keeps talking, and the
/// wait runs the whole of [`GUEST_WEDGED`].
pub const GUEST_QUIET: Duration = Duration::from_secs(15);

/// The other end of the same guard: a guest can be stuck and chatty.
///
/// The compositor prints its interval line whatever else has stopped, so
/// silence alone cannot end a desktop wait, and a suite that never ends is
/// worse than one that reds.
///
/// **Not [`budget`]-scaled, and that is the point of the pair.** Width is what a
/// ceiling on a *slow* guest has to be corrected for, and the silence bound
/// above is what a slow guest is now judged by — it keeps talking, so it is
/// never judged by this at all. What is left for this number to catch is a guest
/// that is stuck *and* chatty, which is a state the width does not produce;
/// scaling it would only make that state cost an hour at width 12. The longest
/// guest action any caller waits on is eight seconds of audio.
///
/// It is also the real ceiling of any failing wait on a shared boot, because
/// the kernel's own 10 s cadence keeps the quiet clock above reset: a settle
/// predicate that could never come true was measured ending here at 302 s,
/// not at 15 (PR #96's verification). Price a new waiting check against this
/// number, not the one above.
pub const GUEST_WEDGED: Duration = Duration::from_secs(300);

pub fn guest_liveness() -> Liveness {
    Liveness::new(GUEST_QUIET, GUEST_WEDGED)
}

/// A kernel line without its `[kernel <t> cpu<N>] ` stamp.
///
/// The stamp is the instance and the rest is the finding, and which of the two
/// a verdict quotes decides an adjudication. `alone_line` compares the wide
/// run's sentence against the lone re-run's, and two runs of one deterministic
/// panic differ in the stamp alone — quoted whole, a staged double fault read
/// `red again on a DIFFERENT failure`, which is the harness reporting two
/// defects where there is one. The stamp is still in the capture underneath.
fn without_stamp(line: &str) -> &str {
    if !is_kernel_line(line) {
        return line;
    }
    line.split_once("] ").map_or(line, |(_, rest)| rest)
}

/// The sentence a wait gives when what stopped the guest is on the console.
///
/// **One wording for all three waits**, so a summary line, a redlist row and an
/// issue file quote the same words wherever the wait was — and so that nothing
/// in it is a measurement of the host. The silence that proved the panic was
/// fatal is deliberately not in the sentence: it differs by a poll interval
/// between two runs of one panic, and `alone_line` compares those two sentences
/// to decide whether a re-run reproduced the defect or found a second one.
fn kernel_died_here(line: &str) -> String {
    format!(
        "kernel panic: {} — the guest went quiet because every CPU is halted, not because it \
         was still working. The panic is the finding and the guard never got to be one.",
        without_stamp(line.trim())
    )
}

/// The heading a verdict puts the guest's own account under.
///
/// One spelling, so an issue file, a redlist row and a CI log all quote the
/// same words when they quote a report.
pub const DIED_SAYING: &str = "--- what the kernel said as it died ---";

/// Why a wait ended badly, carrying the guest's own account of it.
///
/// **A newtype, because what this closes is an omission and an omission cannot
/// be gated by review.** Fifty-two sites in this suite format
/// [`TestResult::error`] and thirty-six of them printed no capture beside it
/// (counted on the tree, 2026-08-18), and on
/// 2026-08-18 that is what a `DOUBLE FAULT on CPU 1` cost — the wait named the
/// death in one sentence, the kernel's report sat in `TestResult::serial`, and
/// the arm printed `stdout`
/// (`issues/kernel/a-double-fault-on-cpu-1-under-a-wide-suite.md`). Fixing
/// the arms would have fixed the arms. What is fixed here is that the sentence
/// cannot be built without the capture: [`Self::new`] is the only constructor
/// there is and the capture is one of its two arguments, so a wait that reports
/// a kernel death and no report is not expressible.
///
/// It carries nothing when the capture carries no kernel death, and that half
/// matters as much: an ordinary ceiling on a live guest, or a guest binary
/// reporting its own error, must not start pasting a boot's serial log into
/// somebody's terminal. [`ceiling_self_check`] asserts both directions.
#[derive(Clone, Debug)]
pub struct WaitVerdict(String);

impl WaitVerdict {
    /// The sentence a wait reached, and the capture it reached it on.
    ///
    /// `capture` is the window in the order the guest wrote it — for a test,
    /// [`TestResult::before`] and then [`TestResult::serial`], which is where
    /// the two halves of one window live — because the first kernel death in it
    /// is the one this verdict is about. An empty slice is a claim that there
    /// was no capture at all, and it is a visible one rather than an omission.
    pub fn new(sentence: String, capture: &[&str]) -> Self {
        let Some(report) = capture.iter().find_map(|c| super::serial::death_report(c)) else {
            return Self(sentence);
        };
        Self(format!("{sentence}\n{DIED_SAYING}\n{report}"))
    }

    /// The sentence, without the account under it.
    ///
    /// What `headline` in `tests/toyos.rs` reads off a red's reason and what
    /// `alone_line` compares two runs of one defect on — so the first line is
    /// still one line, and the report below it can differ between two boots of
    /// the same panic without reading as a second defect.
    pub fn sentence(&self) -> &str {
        self.0.lines().next().unwrap_or_default()
    }
}

impl std::fmt::Display for WaitVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a test's ceiling caught — the panic, the stall, or the slow test.
///
/// Pure, and every input a parameter, so [`ceiling_self_check`] can stage all
/// three rather than wait for a guest to produce one.
///
/// `dying` is the line on which the kernel said it was dying, if it ever did,
/// and `quiet` is how long the guest has said nothing. **The first arm is the
/// whole point.** A Rust `panic!` in the kernel prints `PANIC:` and then
/// `halt_all_cpus` stops every CPU, so the guest goes silent and the ceiling —
/// which is a liveness guard and never a verdict — expired on a machine that
/// had been dead since the first second. `sched_check_build` in run
/// `31946183485` was reported `STALLED: 382s of guard expired` with the panic
/// and its full backtrace four lines above that sentence, on a guest that died
/// at 1.450 s of its own uptime.
///
/// A kernel panic does not end the wait *by itself*, and that is deliberate:
/// the same handler recovers a panic taken in syscall context, killing the
/// caller and leaving the machine running, which is exactly what
/// `panic_recovery`, `heap_ceiling` and `screen_recoverable_untouched` assert.
/// Silence is what separates the two, and it is the separation the harness
/// already trusts everywhere else ([`GUEST_QUIET`]): a recovering guest keeps
/// talking — it has a periodic speaker on every config and the test's own
/// `===TEST_END` arrives in milliseconds — and a halted one cannot.
pub fn ceiling_verdict(
    dying: Option<&str>,
    elapsed: Duration,
    ceiling: Duration,
    quiet: Duration,
    lines: usize,
) -> Option<String> {
    if let Some(line) = dying {
        if quiet >= GUEST_QUIET {
            return Some(kernel_died_here(line));
        }
    }
    if elapsed <= ceiling {
        return None;
    }
    let secs = ceiling.as_secs();
    Some(if quiet >= GUEST_QUIET {
        format!(
            "{STALLED} {secs}s of guard expired, and the guest had said nothing for the last \
             {quiet:.0?} of it — the ceiling caught a machine that had stopped, which is not an \
             answer to what this test asked"
        )
    } else {
        format!(
            "timed out after {secs}s, with the guest still talking {quiet:.0?} ago ({lines} \
             console line(s) while it ran) — it was working and did not finish"
        )
    })
}

/// The three verdicts a ceiling reaches and what each carries, staged with no
/// guest at all.
///
/// The gate for [`ceiling_verdict`], and it runs in both directions on each:
/// the panic must be named *and* not read as a stall, the stall must still read
/// as one, and a program's own panic must not end anybody's run. That last one
/// is the case the obvious patch breaks — a bare panic spelling in the read
/// loop matches a guest binary's panic, and a guest binary is allowed to die.
///
/// The fourth section is [`WaitVerdict`]: naming a death is not the same as
/// keeping the report, and a suite that had the first without the second lost a
/// double fault's whole account on 2026-08-18.
pub fn ceiling_self_check() -> Result<(), String> {
    const CEILING: Duration = Duration::from_secs(380);
    const KERNEL: &str =
        "[kernel 1.450 cpu3] PANIC: panicked at kernel/src/sched/reserve.rs:812:9:";
    let quiet = GUEST_QUIET + Duration::from_secs(1);
    let talking = Duration::from_millis(200);

    // 1. The kernel panicked and the machine went quiet. Named, and named
    //    *before* the ceiling: the guest died at 1.45 s and the guard is 380 s.
    let early = Duration::from_secs(17);
    let Some(panic) = ceiling_verdict(Some(KERNEL), early, CEILING, quiet, 40) else {
        return Err(String::from(
            "a kernel panic followed by silence did not end the wait, so it costs the whole guard",
        ));
    };
    if !panic.contains("kernel panic") || !panic.contains("reserve.rs:812:9") {
        return Err(format!("the verdict does not name the panic: {panic}"));
    }
    if panic.contains(STALLED) {
        return Err(format!("a kernel panic is still reported as a stall: {panic}"));
    }
    if early >= CEILING {
        return Err(String::from("staged the panic after the ceiling, so it proves nothing"));
    }
    // The same panic on a later boot, differing only in its stamp, is the same
    // sentence — or the lone re-run of a reproducible panic reads as a second,
    // different defect. `alone_line` is what compares the two.
    let again = "[kernel 1.503 cpu7] PANIC: panicked at kernel/src/sched/reserve.rs:812:9:";
    if ceiling_verdict(Some(again), early, CEILING, quiet, 40).as_deref() != Some(panic.as_str()) {
        return Err(format!(
            "one panic on two boots gives two sentences, so a re-run reads as a second defect:\n\
             {panic}\n{:?}",
            ceiling_verdict(Some(again), early, CEILING, quiet, 40)
        ));
    }

    // 2. A **userland** panic is not the machine's death. `died` is what the
    //    read loop asks, so the case is staged where the loop reads it: the
    //    same words from a program classify as nobody's business, and a wait
    //    with no kernel death in it runs on.
    const USER: &str = "thread 'main' (1) panicked at sshd/src/main.rs:359:23:";
    if super::serial::died(USER) == Some(super::serial::Died::Kernel) {
        return Err(format!("a program's own panic reads as the kernel's: {USER:?}"));
    }
    if ceiling_verdict(None, early, CEILING, quiet, 40).is_some() {
        return Err(String::from(
            "a run with no kernel death in it ended before its ceiling — a program that panicked \
             would take the whole test down with it",
        ));
    }

    // 2b. The same two cases for the wait that holds a whole capture rather
    //     than a line at a time — `await_guest`, whose `it went quiet` is the
    //     wording #156's signature is stated in.
    let halted = format!("[kernel 0.400 cpu0] compositor: frames=120\n{KERNEL}\n");
    let Some(found) = super::serial::kernel_death(&halted) else {
        return Err(String::from("a capture ending in a kernel panic reads as a guest that merely \
                                 stopped, which is the verdict that threw the cause away"));
    };
    if kernel_died_here(found) != panic {
        return Err(String::from("the two waits word one panic differently"));
    }
    let program_died = format!("[kernel 0.400 cpu0] compositor: frames=120\n{USER}\n");
    if super::serial::kernel_death(&program_died).is_some() {
        return Err(format!(
            "a capture whose only panic is a program's reads as a halted machine:\n{program_died}"
        ));
    }

    // 3. A guest that merely stopped, with no panic of either kind, still
    //    reports as a stall — the classification the whole redlist is written
    //    against.
    let Some(stall) = ceiling_verdict(None, CEILING + Duration::from_secs(1), CEILING, quiet, 40)
    else {
        return Err(String::from("an expired guard on a silent guest returned no verdict at all"));
    };
    if !stall.starts_with(STALLED) {
        return Err(format!("a genuine stall stopped reporting as one: {stall}"));
    }
    // And the other end of the same guard: a guest still talking at the ceiling
    // was working, and that is a different red.
    let Some(slow) = ceiling_verdict(None, CEILING + Duration::from_secs(1), CEILING, talking, 900)
    else {
        return Err(String::from("an expired guard on a talking guest returned no verdict"));
    };
    if slow.contains(STALLED) || !slow.contains("did not finish") {
        return Err(format!("a slow test reads as a stall: {slow}"));
    }
    // Nothing has expired and nothing died: no verdict.
    if ceiling_verdict(None, early, CEILING, talking, 40).is_some() {
        return Err(String::from("a healthy run was given a verdict"));
    }

    // 4. **What the verdict carries, which is the half that was missing.** Every
    //    arm above names a death in one sentence; until 2026-08-18 that sentence
    //    was the whole of what a failure arm had, and a `DOUBLE FAULT on CPU 1`
    //    went into the record with its report — written, on IST1, 6688 bytes of
    //    it — never printed. Both directions, because the second is what keeps a
    //    stall or a slow test from pasting a boot's console at somebody.
    const DF: &str = "[kernel 6.204 cpu1] DOUBLE FAULT on CPU 1 (pid=Some(Pid(2)) tid=Some(Tid(0)))";
    let window_before = "[kernel 6.201 cpu0] spawn: /bin/test_rs_console_line_atomicity pid=41\n";
    let window_serial = format!(
        "AAAAAAAA\n{DF}\n\
         [kernel 6.204 cpu1]   cr2=0xffff800002672ff8 (address that caused the fault chain)\n\
         [kernel 6.204 cpu1]   rip=0xffffffff80121a40  rsp=0xffff800002673000  rbp=0x0\n"
    );
    let died_verdict = ceiling_verdict(Some(DF), early, CEILING, quiet, 40)
        .ok_or("a staged double fault reached no verdict at all")?;
    let carried = WaitVerdict::new(died_verdict.clone(), &[window_before, &window_serial]);
    for want in [DIED_SAYING, "cr2=0xffff800002672ff8", "rip=0xffffffff80121a40"] {
        if !carried.to_string().contains(want) {
            return Err(format!(
                "the verdict names the death and drops {want:?}, which is the defect \
                 `issues/kernel/a-double-fault-on-cpu-1-under-a-wide-suite.md` is \
                 about:\n{carried}"
            ));
        }
    }
    // The sentence is still one line and still the sentence — `headline` in
    // `tests/toyos.rs` reads it off a red's reason and `alone_line` compares two
    // runs of one defect on it, so a report under it must not become part of it.
    if carried.sentence() != died_verdict {
        return Err(format!(
            "the report changed the sentence a summary quotes:\n{}\n{died_verdict}",
            carried.sentence()
        ));
    }
    // The other direction. A guest still talking at its ceiling has nothing to
    // account for, and a verdict that grew a serial log would be a second defect
    // dressed as a fix.
    let quiet_capture = WaitVerdict::new(slow.clone(), &["[kernel 0.377 cpu0] NVMe: found\n"]);
    if quiet_capture.to_string() != slow {
        return Err(format!(
            "a verdict on a capture nothing died in grew a report:\n{quiet_capture}"
        ));
    }
    // And the capture being handed over at all is the argument, not a habit: an
    // empty slice is what a wait with nothing to show says, and it says it.
    if WaitVerdict::new(died_verdict.clone(), &[]).to_string() != died_verdict {
        return Err(String::from("a verdict built on no capture invented a report"));
    }

    eprintln!(
        "  [ceiling] the panic, the stall, the slow test and the healthy run, each named apart \
         from the other three; the panic's verdict carries the kernel's own {} lines and the \
         other three carry nothing",
        carried.to_string().lines().count() - 1,
    );
    Ok(())
}

/// Collect console output until `done` reads true of the whole capture, or the
/// guest stops making progress.
///
/// The shape [`QemuInstance::drain_serial`] cannot have: its caller passes a
/// number of seconds, and a number of seconds is a claim about the host. Here
/// the wait ends when the guest goes quiet or wedges, so a guest with a twelfth
/// of the machine costs the run wall clock and never a verdict — and when it
/// does end early the message says so in the words the classifier reads
/// ([`STALLED`]).
///
/// `doing` is what the guest was asked to do, in the caller's own words. The
/// caller keeps its assertion; what this owns is the difference between "it did
/// the wrong thing" and "it never got there".
///
/// It lives beside [`Liveness`] rather than in the test list because a test in
/// `tests/common/` could not reach it there, and the two that could not —
/// `metal_sim_null_audio` and `hda_two_live_refused` — each reached for a span
/// of host wall clock instead and lost the race on a runner.
pub fn await_guest(
    qemu: &mut QemuInstance,
    log: &mut String,
    doing: &str,
    done: impl Fn(&str) -> bool,
) -> Result<(), String> {
    // Where this wait's own evidence starts. The capture is the caller's and
    // outlives every wait on it, so a panic the machine recovered from ten
    // probes ago must not be handed to this one as its cause.
    let from = log.len();
    let mut live = guest_liveness();
    while !done(log) && live.working(log) {
        let more = qemu.drain_serial(Duration::from_millis(200));
        log.push_str(&more);
    }
    if done(log) {
        return Ok(());
    }
    // **The third wait, asking the one question the other two ask.** A guest
    // that halted every CPU went quiet for a reason it wrote down first, and
    // `it went quiet` is that reason thrown away — which is the shape #156's
    // whole signature is stated in (`a total freeze of the guest`, judged by a
    // periodic line that stopped arriving), so what this says decides how the
    // next occurrence is read.
    let since = &log[from..];
    if let Some(line) = super::serial::kernel_death(since) {
        // Through [`WaitVerdict`] for the reason that type exists: this caller
        // owns the capture and usually prints it, and `usually` is what the
        // arms that do not have in common with the one that lost a double
        // fault's report.
        return Err(WaitVerdict::new(
            format!("{} It was waiting for {doing}", kernel_died_here(line)),
            &[since],
        )
        .to_string());
    }
    Err(format!("{STALLED} waiting for {doing} — {}", live.why()))
}

/// [`await_guest`] for the common case: one marker anywhere in the capture.
pub fn await_marker(
    qemu: &mut QemuInstance,
    log: &mut String,
    marker: &str,
    doing: &str,
) -> Result<(), String> {
    await_marker_new(qemu, log, marker, 0, doing)
}

/// [`await_marker`] over what arrives after `from`.
///
/// For a marker a test asks for more than once: a whole-capture scan answers
/// the second ask with the first ask's line and carries on against a guest that
/// has not done the thing yet.
pub fn await_marker_new(
    qemu: &mut QemuInstance,
    log: &mut String,
    marker: &str,
    from: usize,
    doing: &str,
) -> Result<(), String> {
    await_guest(qemu, log, doing, |log| log[from.min(log.len())..].contains(marker))
}

/// The hardware shape QEMU presents to the guest.
///
/// Not a display setting: each variant is a whole machine. `Headless` is the
/// historical test config -- no VGA and no GPU device at all, so firmware
/// publishes no GOP and `kernel_args.gop_framebuffer` is zero. `Gop` swaps in
/// `-vga std` so firmware publishes a linear framebuffer, which is the path a
/// laptop takes and the only one in which the on-screen panic console renders
/// anything. `Metal` goes the whole way to the target laptop's shape.
#[derive(Clone, Copy, PartialEq)]
pub enum Profile {
    Headless,
    /// [`Profile::Headless`] with the NIC's MSI-X capability taken away.
    ///
    /// The one configuration in this suite where a device the kernel has
    /// already reset and negotiated features with turns out to have no way of
    /// raising an interrupt. Every virtio function QEMU builds and every one
    /// that ships has the capability, so nothing else could ask what the
    /// driver does without it — and what it used to do was panic the kernel,
    /// on a machine whose other devices were all fine.
    VirtioNetNoMsix,
    Gop,
    /// M1 metal-sim: GOP, NVMe, xHCI with the boot stick on it, i8042 from
    /// q35, and nothing else -- no virtio device and no USB HID. This is the
    /// machine shape that gets flashed, so it is the one the input tests run
    /// on. The 16550 stays: every defect metal-sim has found came from the
    /// device shape, and with a console the guest can be driven over the
    /// ===TEST_START=== protocol like any other. [`BootOptions::mute`] takes
    /// it away for the one test that certifies the T14's literal shape.
    Metal,
    /// metal-sim with the T14's internal xHCI actually populated: the boot
    /// stick plus five more devices, two of them keyboards. The laptop's
    /// controller carries a camera, Bluetooth and a fingerprint reader
    /// alongside whatever is plugged in, and a profile with one USB device
    /// cannot see any defect that needs a fourth.
    MetalUsb,
    /// metal-sim with the T14's actual NVMe capacity instead of a token
    /// image. Device *size* is a shape dimension and it was the one nobody
    /// had varied: every test disk was small enough that a per-device-block
    /// index fit under the object allocator's 2 MiB ceiling, so the first
    /// boot on the laptop was the first time anything asked for a
    /// device-sized allocation.
    MetalDisk,
    /// metal-sim with no NVMe controller at all.
    ///
    /// Device *presence* is the shape dimension underneath size and sector
    /// size, and it was the one nobody had varied for storage: every profile
    /// gave the guest a disk, so nothing asked what the kernel does without
    /// one. The answer was `.expect("NVMe: no controller found")` at 0.08 s.
    /// The bootloader reads the initrd through UEFI before ExitBootServices,
    /// so a machine really can boot ToyOS with no NVMe -- and a controller
    /// hidden behind a firmware setting looks exactly the same.
    Diskless,
    /// metal-sim with a namespace formatted in 8 KiB logical blocks.
    ///
    /// Sector size is a shape dimension, and it was one the harness could
    /// not express: every profile got QEMU's implicit 512-byte namespace, so
    /// nothing asked the driver what it does with a device it cannot address.
    /// The answer was `4096 / sector_size == 0` and then a divide by zero, at
    /// 0.068 s, before storage is up and before there is a console to report
    /// it on.
    ///
    /// 8192 rather than something absurd because it is real: 8 KiB-format
    /// namespaces ship, and this driver's whole stack above the sector layer
    /// is written in 4096-byte blocks. The guest is expected to refuse the
    /// device by name, so this profile boots no userland at all.
    NvmeWideSector,
    /// metal-sim with a second USB stick beside the boot stick.
    ///
    /// The boot stick is on the bus in every profile and is the one device the
    /// guest must never write to, so a storage test needs a *second* disk —
    /// one the harness stages on the host, stamps as writable, and reads back
    /// afterwards. Presence of that disk is the shape dimension; every other
    /// profile is its absence.
    UsbDisk,
    /// [`Profile::UsbDisk`] with the second stick formatted in 4 KiB logical
    /// blocks. Sector size is a shape dimension for USB exactly as it is for
    /// NVMe, and it is the one that produced a divide-by-zero there.
    UsbDisk4k,
    /// [`Profile::UsbDisk`] with a 3 TB external disk instead of a stick.
    ///
    /// Past 2 TiB a 512-byte-sector device has more sectors than a READ(10)
    /// command can address, and READ CAPACITY(10) stops being able to report
    /// the size at all — so this is the profile where the 16-byte form runs
    /// and where the driver has to refuse a device rather than serve the first
    /// 2 TiB of it. Sparse, so the host pays for the blocks the guest touches.
    UsbDiskHuge,
    /// [`Profile::UsbDisk`] with the second stick's backing opened read-only.
    ///
    /// The only configuration in this suite where a *device* refuses an I/O
    /// the driver was right to issue: QEMU answers WRITE(10) on a write-
    /// protected LUN with a CHECK CONDITION, which is a CSW status of 1 and
    /// the REQUEST SENSE path behind it. Reads on the same disk still work, so
    /// one boot shows the error channel carrying a failure and not carrying a
    /// success.
    UsbDiskReadOnly,
    /// [`Profile::UsbDiskHuge`] with the 3 TB disk attached *ahead* of the boot
    /// stick, so the controller enumerates the disk the driver refuses first.
    ///
    /// Order is the whole shape. `bind` configures a device's two bulk
    /// endpoints into a pool block and only then asks the disk how big it is,
    /// so a disk refused for its size has already pointed the controller's
    /// endpoint contexts at that block. Every other USB profile puts the boot
    /// stick on port 1, where it binds successfully and the question never
    /// arises; here the refusal comes first, and what the *next* disk is given
    /// is the assertion. QEMU assigns ports in device-creation order, measured
    /// against the kernel's own `port N connected` lines.
    UsbDiskRefusedFirst,
    /// More USB disks on one controller than its DMA pool has blocks for.
    ///
    /// `MSC_BLOCKS` is 2 and the boot stick takes one of them, so the second
    /// data disk here is the first one past the ceiling. Every other profile
    /// declares one disk, which is why nothing could ask what a caller sees when
    /// the bound is hit — and the bound is policy, so that answer is the whole
    /// question. Both disks are stamped: the one that binds is written, and the
    /// one the pool had no room for has to come back byte-identical, which is
    /// the claim a log line cannot make.
    ///
    /// Two and not three, though the pool would refuse either way.
    /// `nec-usb-xhci` offers four SuperSpeed ports and QEMU puts the fifth
    /// device behind an auto-created hub, which this driver walks past — so a
    /// third data disk is not one the guest refuses, it is one the guest never
    /// sees, and a count that included it would be measuring QEMU's port
    /// allocation. Measured: `class=0x9 vendor=0409 product=55aa` on port 8 at
    /// full speed, with `no HID boot interface found, skipping`.
    UsbDiskCrowd,
    /// metal-sim with a device that attaches at **full speed**.
    ///
    /// Speed is a shape dimension and it was one no profile varied: every USB
    /// device in this suite is high or SuperSpeed, and those two are the speeds
    /// whose EP0 max packet size is fixed by the specification. Full speed is
    /// the one where it is not — 8, 16, 32 or 64, and unknown until the first
    /// eight bytes of the device descriptor have been read over the very
    /// endpoint being sized. A T14 port answered a USB Transaction Error to a
    /// driver that assumed 64 and read 18 bytes in one go, and no test here
    /// could have seen it.
    ///
    /// Two of them, because `bMaxPacketSize0` is the dimension under test and a
    /// profile with one value of it cannot tell "the driver read the device's
    /// answer" from "the driver's guess happened to match": the tablet answers
    /// **8** and the smartcard reader answers **64**, so one boot carries both
    /// the correction and its absence. Both are full-speed only — QEMU gives
    /// each a `.full` descriptor set and no `.high` one, so `usb_desc_attach`
    /// has no faster speed to pick — and neither needs a chardev, drive or
    /// audiodev to enumerate. Measured with `info usb` on QEMU 11.0.2: both
    /// report 12 Mb/s, and `usb-kbd`, which every other profile uses, reports
    /// 480.
    MetalFullSpeed,
    /// Two xHCI controllers, with every device on the *second* one.
    ///
    /// The T14 Gen 2's literal shape, and the one that had never been staged:
    /// Tiger Lake puts a USB4 xHCI in the Thunderbolt block at 00:0d.0 and the
    /// PCH's at 00:14.0 — same class, same subclass, same prog_if — and the
    /// laptop's own ports hang off the second. Nothing is attached to the
    /// first here, exactly as nothing is plugged into the laptop's Thunderbolt
    /// ports, so a kernel that stops at the first PCI match sees a machine
    /// with no USB at all. The i8042 is off, which is what stops a PS/2
    /// keyboard delivering the keystroke this profile means to route over USB.
    MetalXhciSecond,
    /// Two xHCI controllers with HID devices on both.
    ///
    /// One held-set and one button merge for the whole machine is a claim
    /// about devices on *different controllers* as much as about two on one
    /// bus, and it is a claim nothing could test: with one controller, an
    /// xHCI slot id was a machine-wide name for a device. It is not — the
    /// device lists here are shaped so both pointers land on the same slot id
    /// of their own controller.
    MetalXhciBoth,
    /// The HID controller has no MSI-X, and nothing else can drain its ring.
    ///
    /// The T14's Thunderbolt xHCI has no MSI-X capability — the laptop's own
    /// boot log says so — and every controller in this suite had one, so the
    /// branch that handles its absence had never executed. It logged "using
    /// polled mode" and returned, and there is no polled mode: the driver
    /// reads an event ring only when vector 0x21 has fired. This profile is
    /// the machine where the driver has to fall through to MSI and where an
    /// injected keystroke is the only thing that can prove it did — which
    /// takes a machine with no USB storage on it at all, for the reason the
    /// shape below states.
    MetalXhciMsi,
    /// Two controllers, the second with neither MSI-X nor MSI.
    ///
    /// A function offering neither is not a machine that ships — QEMU is the
    /// only place it can be built — but "this driver cannot drive this
    /// controller" is a state the code has to be able to reach and say, and
    /// nothing else can stage it. The first controller is ordinary and carries
    /// the boot stick, so the refusal is visibly *per controller*: the machine
    /// boots, and the HID on the crippled one is refused by name rather than
    /// enumerated and left mute.
    MetalXhciNoIrq,
    /// Two controllers, and every input device arrives *after* the boot.
    ///
    /// The T14's shape for the one thing no profile stages: its Thunderbolt
    /// xHCI at 00:0d.0 has five ports and has never had a device on them, so
    /// the controller a user plugs
    /// into is the one that enumerated nothing at boot. Here the second
    /// controller is that one and the boot stick is on the first.
    ///
    /// The boot-time device list is one `usb-tablet`, and every part of that is
    /// load-bearing. It is a pointer, so a late-bound one has to compose with a
    /// source that already exists rather than being the first. It is
    /// *absolute*, so QEMU has no relative handler until a `usb-mouse` is
    /// plugged in — which makes an injected `rel` event ground truth that the
    /// late device is the one delivering, not the boot-time one. And it is not
    /// a keyboard: with `i8042=off` this machine has no keyboard at all until
    /// one is hot-plugged, so a keystroke that arrives can only have come
    /// through the device that was added after the boot.
    MetalHotplug,
    /// metal-sim with no IOMMU at all, so firmware publishes no `DMAR`.
    ///
    /// Presence of the unit is the shape dimension, and it is the one QEMU
    /// gives for free that no real machine gives at all: on hardware, "no
    /// DMAR" and "VT-d disabled in firmware setup" are the same observation.
    /// This is the machine where the kernel has
    /// to say which of the two it cannot tell apart.
    NoIommu,
    /// metal-sim whose unit advertises a 39-bit address width instead of 48.
    ///
    /// `CAP.SAGAW` is a register the guest decodes into a page-table depth,
    /// and a suite with one value of it cannot tell a decode from a constant.
    /// Both widths are real: 39-bit units ship, and the IOVA base every domain
    /// gets is derived from this number.
    IommuNarrow,
    /// metal-sim whose unit cannot remap interrupts.
    ///
    /// Two registers move together — the DMAR's own `INTR_REMAP` flag and the
    /// unit's `ECAP.IR` — and the kernel gives them separate
    /// refusals, because a platform that declares it cannot remap and a unit
    /// that cannot are different facts a user can act on differently.
    IommuNoIntremap,
    /// [`Profile::Headless`] with its virtio sound card replaced by an Intel
    /// HDA controller and one codec — the machine soundd drives itself.
    ///
    /// Everything else is held still on purpose. The console is still
    /// virtio-serial, the NIC is still there, the disks are the same: what
    /// differs from the machine gate A's four recorded configs run on is the
    /// sound card, so a difference in the capture is a difference in the audio
    /// path. It is not the T14's literal shape and does not try to be — this is
    /// the audio arm, not a PCI-topology one. H0's diagnostic staged that
    /// comparison and is deleted now that the
    /// driver above answers every question it was asked for.
    Hda,
    /// [`Profile::Hda`] with a second controller that also has a codec.
    ///
    /// Two live links, which the kernel refuses by name rather than binding
    /// the first: choosing between them means walking their codec graphs, and
    /// that is the driver's work. The negative control on the whole bind path
    /// — a first-match kernel would go green on every other HDA test.
    HdaTwoLive,
}

/// The vIOMMU a profile puts on the machine.
///
/// A whole machine dimension rather than a flag: the unit is what decodes
/// every DMA and every interrupt message on the bus. Two fields, because two
/// are what a guest can tell apart — `aw_bits` moves `CAP.SAGAW`, `intremap`
/// moves `ECAP.IR` and the DMAR's `INTR_REMAP` flag — and a harness that
/// stages one value of each cannot distinguish a kernel that reads those
/// registers from one that prints what it expected to find.
///
/// `caching-mode` is deliberately not a field. It is on everywhere: it is the
/// stricter configuration, it is the only one QEMU can stage, and the kernel
/// refuses to branch on it — so a profile that
/// turned it off would be staging a machine no code here distinguishes.
#[derive(Clone, Copy, PartialEq)]
pub struct Iommu {
    /// `aw-bits`. QEMU 11.0.2 takes 39 or 48 and nothing else.
    pub aw_bits: u8,
    /// Interrupt remapping. Off is a platform declaring it cannot remap.
    pub intremap: bool,
}

/// What every profile but the three that vary it declares: the widest address
/// width QEMU offers and interrupt remapping on.
pub const IOMMU_DEFAULT: Iommu = Iommu { aw_bits: 48, intremap: true };

/// The controller every profile but [`Profile::MetalUsb`] gets. `nec-usb-xhci`
/// registers `MAX(p2, p3)` attachable USB ports over `p2 + p3` port registers —
/// the two ranges are two speed-specific views of the same ports, not two sets
/// of them — so the default `p2=4,p3=4` takes **four** devices, two short of the
/// crowded set rather than one.
const XHCI_DEFAULT: &str = "nec-usb-xhci,id=xhci";
/// Eight attachable ports, which is `MAX(p2=8, p3=4)`, over twelve port
/// registers: 1-4 the SuperSpeed view, 5-12 the USB2 view. Measured on QEMU
/// 11.0.2 against the kernel's own lines — `max_ports=12`, and the six devices
/// landing on registers 1 and 6-10. The boot stick is a `usb-storage` with a
/// SuperSpeed descriptor, so it takes the SuperSpeed view of the first port and
/// is enumerated *before* every HID; the five devices below are full or high
/// speed and take the USB2 view of ports 2-6. Six of eight used, two spare.
///
/// `slots=` would have been the natural way to stage slot exhaustion, and it
/// is not: on QEMU 11.0.2 `nec-usb-xhci,slots=N` reads back as N through
/// `qom-get` and HCSPARAMS1 still reports 64, `qemu-xhci` has no such property
/// at all, and Enable Slot ignores the MaxSlotsEn the driver writes to CONFIG.
/// The kernel's own `xhci-one-slot` feature is what drives that path.
const XHCI_WIDE: &str = "nec-usb-xhci,id=xhci,p2=8";
/// A second controller, for the profiles that stage a machine with two. Only
/// the id differs — the point is precisely that the two are indistinguishable
/// by class, subclass and prog_if, which is why taking the first PCI match
/// looked right for as long as it did.
const XHCI_SECOND: &str = "nec-usb-xhci,id=xhci1";
/// A controller with no MSI-X table, which leaves `msi=auto` to give it MSI —
/// the shape of the T14's Thunderbolt xHCI and of Intel PCH parts generally.
const XHCI_MSI_ONLY: &str = "nec-usb-xhci,id=xhci1,msix=off";
/// A controller with no message-signalled interrupts at all, in each of the
/// two bus positions a profile puts one in. Nothing on a PCIe bus is really
/// built this way; it is how the harness reaches the branch where the driver
/// has to refuse a controller instead of driving it blind — and, in the first
/// position, how it takes USB storage off a machine entirely.
const XHCI_NO_IRQ_FIRST: &str = "nec-usb-xhci,id=xhci,msix=off,msi=off";
const XHCI_NO_IRQ_SECOND: &str = "nec-usb-xhci,id=xhci1,msix=off,msi=off";

/// One controller with one codec: the ordinary machine, and the one an audio
/// arm needs. `hda-output` because it is a playback-only codec — the driver
/// configures no input path and a duplex codec would only add widgets nothing
/// walks.
const HDA_ONE: &[&str] = &["intel-hda,id=hda0", "hda-output,bus=hda0.0,cad=0,audiodev=hdaaud"];

/// Two controllers, each with a codec that answers.
///
/// The state the kernel refuses: it can tell which links are alive and cannot
/// tell which one a human is wired to, so binding either would be a guess.
const HDA_TWO_LIVE: &[&str] = &[
    "intel-hda,id=hda0",
    "hda-output,bus=hda0.0,cad=0,audiodev=hdaaud",
    "intel-hda,id=hda1",
    "hda-output,bus=hda1.0,cad=0,audiodev=hdaaud",
];

/// Whether a machine has the virtio block, and whether its NIC can raise an
/// interrupt.
///
/// Two shape dimensions and not one, because a device that publishes no MSI-X
/// capability is a device, not an absence: the driver reaches it, resets it,
/// negotiates features with it and only then finds it has no way to be told a
/// packet arrived. `vectors=0` is the actuator — QEMU builds a virtio-pci
/// function's MSI-X table only for a non-zero vector count — and it is the
/// only one, since every emulated and every real virtio function has the
/// capability.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Virtio {
    Absent,
    Present,
    /// The whole block, with the NIC's MSI-X capability removed and
    /// virtio-sound's and virtio-serial's left alone — so the console still
    /// carries the refusal and audio still works while networking does not.
    NicWithoutMsix,
    /// The whole block **without virtio-sound**, so the machine's only audio
    /// device is the one in `hda`.
    ///
    /// Not a lesser [`Virtio::Present`]: soundd claims a kernel-driven card
    /// before it looks for a controller to drive itself, so a machine carrying
    /// both would exercise the virtio path and nothing else. This is what makes
    /// an HDA arm of gate A a *different machine* rather than a different flag,
    /// and it keeps the console, the NIC and the timing of the recorded audio
    /// configs so the two arms differ in the sound card and not in the machine.
    WithoutSound,
}

impl Virtio {
    fn present(self) -> bool {
        self != Self::Absent
    }

    fn sound(self) -> bool {
        matches!(self, Self::Present | Self::NicWithoutMsix)
    }
}

/// Everything a profile decides about the machine, in one table. A new
/// variant answers every question here or does not compile — which `self !=
/// Profile::Metal` did the opposite of: it handed anything that was not
/// literally Metal the whole virtio block, a USB keyboard and a console.
struct Shape {
    /// `-vga` mode. "none" leaves firmware with no GOP to publish.
    vga: &'static str,
    /// Video memory, which is what decides the panel: OVMF offers every mode
    /// that fits in it and the bootloader takes the one with the most pixels.
    /// `None` is QEMU's default 16 MiB, whose largest mode is 2048x2048 --- a
    /// panel that is a whole number of glyph rows tall, which no real one has
    /// to be. Declared rather than defaulted because the panel's *size* is a
    /// shape dimension exactly as a disk's is, and the tests that read pixels
    /// were all blind to the remainder until one profile had one.
    vgamem_mb: Option<u32>,
    /// virtio-net, virtio-sound, and the console on virtio-serial.
    virtio: Virtio,
    /// The `-device` argument for each xHCI controller, port and slot counts
    /// included. A list because a machine can have more than one and the T14
    /// does — its keyboard is on the second.
    xhci: &'static [&'static str],
    /// The bus the boot stick and the second USB disk attach to. Named rather
    /// than assumed, because which controller carries the storage is a shape
    /// dimension once there is more than one: the index the block layer holds
    /// has to name the same disk either way.
    storage_bus: &'static str,
    /// Every USB device besides the boot stick, each naming its own bus.
    /// Absence is what makes an i8042 test measure anything: QEMU activates
    /// one input handler per device class, so with a usb-kbd present every
    /// injected keystroke goes to it.
    usb: &'static [&'static str],
    /// The NVMe namespace's size. The backing file is sparse, so this is free
    /// to state honestly — and it has to be stated, because a kernel
    /// structure sized per device block is bounded by this number and by
    /// nothing else.
    nvme_bytes: u64,
    /// The namespace's logical block size. Stated per profile for the same
    /// reason `nvme_bytes` is: it is a dimension of the device, the driver
    /// turns it into a shift and a divisor, and QEMU's implicit namespace only
    /// ever produced one value of it.
    nvme_lba_bytes: u32,
    /// Every `usb-storage` device besides the boot stick, in the order QEMU
    /// creates them.
    ///
    /// A list and not one device's dimensions. **How many disks are on the bus
    /// is a shape dimension in its own right**: the driver's DMA pool holds
    /// `MSC_BLOCKS` of them and refuses the rest by name, and every profile
    /// that could have asked what happens at that ceiling declared exactly one.
    /// The order is the second half of the same field — QEMU hands out
    /// root-hub ports in device-creation order, so where the boot stick falls
    /// in this list is what decides which disk the controller enumerates first.
    usb_disks: &'static [UsbDisk],
    /// Every Intel HDA controller on the machine and the codecs behind each,
    /// as `-device` arguments in the order QEMU is to create them. Empty is
    /// what every profile but [`Profile::Hda`] and [`Profile::HdaTwoLive`]
    /// declares, and it is the machine this kernel has always booted: audio
    /// through virtio-sound or through nothing at all.
    ///
    /// Presence of a class-0403 *function* is the shape dimension, and it is
    /// separate from whether anything answers on the link behind it — which is
    /// H0's question (b), and what the codec
    /// arguments in this list decide per controller.
    hda: &'static [&'static str],
    /// The unit that decodes this machine's DMA, or its absence. Stated per
    /// profile because absence is a shape and because the unit's own
    /// capabilities are what the kernel reads at boot.
    iommu: Option<Iommu>,
}

/// One `usb-storage` device beside the boot stick.
#[derive(Clone, Copy)]
pub struct UsbDisk {
    /// Its size. Stated for the same reason the namespace's is — the driver
    /// turns it into an LBA, and whether that LBA fits the command it is sent
    /// in is a property of this number. The backing is sparse, so a realistic
    /// one is nearly free.
    pub bytes: u64,
    /// Its logical block size. `usb-storage` takes any power of two from 512 B
    /// up, so unlike the boot stick this is something a profile can choose.
    pub lba_bytes: u32,
    /// Open its backing read-only, so the guest's writes are refused by the
    /// device rather than by the driver. Nothing else in this suite can make a
    /// real device say no to an I/O the driver was right to issue.
    readonly: bool,
    /// Attach it *ahead* of the boot stick. Which disk comes first is a shape
    /// dimension the moment one of them can be refused: a driver that hands the
    /// pool block of a failed bind to the next disk is only observable when the
    /// failure is first.
    before_boot_stick: bool,
}

impl UsbDisk {
    /// The nominal 32 GiB stick this suite's storage tests are staged on, and
    /// what a profile carries when it just needs a disk it may write to.
    const DATA: Self = Self {
        bytes: USB_STICK_BYTES,
        lba_bytes: 512,
        readonly: false,
        before_boot_stick: false,
    };
    /// A 3 TB external disk, which this driver has to refuse by name rather
    /// than serve the first 2 TiB of.
    const HUGE: Self = Self { bytes: USB_HUGE_BYTES, ..Self::DATA };
}

/// QEMU's name for the `i`-th data disk's backing, and for the device in front
/// of it. Derived from the position rather than declared, so a profile cannot
/// give two disks one name.
fn usb_drive_id(i: usize) -> String {
    format!("usbdisk{i}")
}

/// The device id, which is what `device_del` names.
pub fn usb_device_id(i: usize) -> String {
    format!("usbdev{i}")
}

/// The boot stick's device id.
///
/// The data disks have carried one since a test first had to unplug one; the
/// stick the machine booted from had none, so the one device whose removal
/// takes `/boot` and `/log` with it was the one the host could not name — which
/// is the removal the owner's machine dies on.
pub const BOOT_STICK_ID: &str = "bootstick";

/// What every profile but [`Profile::MetalDisk`] gives the guest. Large
/// enough for a filesystem, small enough that a boot formats it quickly.
const NVME_SMALL: u64 = 128 * 1024 * 1024;

/// What every namespace but [`Profile::NvmeWideSector`]'s reports — QEMU's
/// implicit default, and the T14's.
const NVME_LBA_DEFAULT: u32 = 512;

/// The data stick every USB storage profile but [`Profile::UsbDiskHuge`]
/// carries: a nominal 32 GiB stick, the size of the class of device this
/// project boots from. Chosen rather than measured off one part — but not a
/// token number either, because the last 4 KiB block on it sits at sector
/// 67,108,856, which needs 27 bits of LBA. A 128 MiB scratch image needs 18
/// and could not tell a truncated LBA field from a correct one.
pub const USB_STICK_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// A 3 TB external USB disk: a device that exists, and one this driver cannot
/// address. At 512-byte sectors it has 6,442,450,944 of them, so READ(10)'s
/// 32-bit LBA is a bit short and READ CAPACITY(10) cannot report the size —
/// which is the only configuration in which the 16-byte form runs.
pub const USB_HUGE_BYTES: u64 = 3 * 1024 * 1024 * 1024 * 1024;

/// The T14 Gen 2's namespace, to the byte: 500,118,192 sectors of 512 B.
/// Taken from the laptop's own boot line rather than rounded from "244 GB",
/// so a test that asserts on the block count is asserting against the machine
/// that gets flashed.
pub const NVME_T14_BYTES: u64 = 500_118_192 * 512;
/// The same device as the kernel counts it: 62,514,774 blocks of 4 KiB.
pub const NVME_T14_BLOCKS: u64 = NVME_T14_BYTES / 4096;

impl Profile {
    fn shape(self) -> Shape {
        match self {
            Self::Headless => Shape {
                vga: "none",
                vgamem_mb: None,
                virtio: Virtio::Present,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::VirtioNetNoMsix => Shape {
                vga: "none",
                vgamem_mb: None,
                virtio: Virtio::NicWithoutMsix,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::Gop => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Present,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::Diskless => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                // Zero is the absence, not a zero-length disk: `nvme_args`
                // emits no controller, no namespace and no backing file.
                nvme_bytes: 0,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::Metal => Shape {
                vga: "std",
                // The T14's panel. 1920x1080x4 is 8,294,400 bytes, so 8 MiB
                // admits it and excludes every mode with more pixels ---
                // 1920x1200 and 2048x1536 both need more, and 1600x1200 has
                // fewer pixels, so this is the one the bootloader picks. It
                // gives 240x67 cells with 8 pixels left over at the bottom,
                // which is the geometry the machine actually has and the one
                // the 2048x2048 default could not express.
                vgamem_mb: Some(8),
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // Two keyboards and two pointers, because the collision this
            // stages is between devices of the same HID class; a hub for a
            // second non-HID device, since it needs no backing file and the
            // driver has to walk past it exactly as it walks past the stick.
            Self::MetalUsb => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_WIDE],
                storage_bus: "xhci.0",
                usb: &[
                    "usb-kbd,bus=xhci.0",
                    "usb-kbd,bus=xhci.0",
                    "usb-mouse,bus=xhci.0",
                    "usb-tablet,bus=xhci.0",
                    "usb-hub,bus=xhci.0",
                ],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::MetalDisk => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_T14_BYTES,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::NvmeWideSector => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: 8192,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDisk => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk::DATA],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDisk4k => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk { lba_bytes: 4096, ..UsbDisk::DATA }],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskHuge => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk::HUGE],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskRefusedFirst => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk { before_boot_stick: true, ..UsbDisk::HUGE }],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskReadOnly => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk { readonly: true, ..UsbDisk::DATA }],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskCrowd => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk::DATA, UsbDisk::DATA],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // The first controller carries nothing at all — not even the boot
            // stick, which is on the second with the HID. That is the laptop
            // exactly: a USB-A port is a PCH port, and the Thunderbolt block's
            // controller is empty until something is plugged into it. It also
            // means the disk index the block layer holds names a device on a
            // controller that is not the first, which nothing else stages.
            Self::MetalFullSpeed => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &["usb-wacom-tablet,bus=xhci.0", "usb-ccid,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::MetalXhciSecond => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT, XHCI_SECOND],
                storage_bus: "xhci1.0",
                usb: &["usb-kbd,bus=xhci1.0", "usb-mouse,bus=xhci1.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // A hub ahead of the second controller's HID, so that controller's
            // devices take the same slot ids as the first's: the boot stick is
            // SuperSpeed and enumerates ahead of every USB2 device, and the hub
            // stands in for it. Both mice therefore land on one slot id, which
            // is the collision a slot-derived pointer source turns into a
            // single button-merge entry.
            Self::MetalXhciBoth => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT, XHCI_SECOND],
                storage_bus: "xhci.0",
                usb: &[
                    "usb-kbd,bus=xhci.0",
                    "usb-mouse,bus=xhci.0",
                    "usb-hub,bus=xhci1.0",
                    "usb-kbd,bus=xhci1.0",
                    "usb-mouse,bus=xhci1.0",
                ],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // The boot stick's controller is the one with no interrupt
            // mechanism at all, so the driver refuses it and the machine does
            // no USB storage I/O whatsoever. That is the load-bearing part of
            // this shape, not decoration: `wait_transfer` drains the *whole*
            // event ring and dispatches every HID report in it, so a keyboard
            // sharing a controller with the boot stick delivers on the back of
            // the ESP log's idle-loop writes whether or not its interrupt
            // works. Measured — the first version of this profile put both on
            // one controller and passed with MSI deliberately left disabled.
            Self::MetalXhciMsi => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_NO_IRQ_FIRST, XHCI_MSI_ONLY],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci1.0", "usb-mouse,bus=xhci1.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // Boot stick on the good controller, HID on the crippled one. A
            // keyboard is what makes the absence assertion mean something:
            // the driver has a device it would otherwise bind and announce.
            Self::MetalXhciNoIrq => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT, XHCI_NO_IRQ_SECOND],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci1.0", "usb-mouse,bus=xhci1.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::MetalHotplug => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT, XHCI_SECOND],
                storage_bus: "xhci.0",
                usb: &["usb-tablet,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // The three below are metal-sim with one field of the unit moved,
            // so what differs between their boot logs and Metal's is the unit
            // and nothing else on the machine.
            Self::NoIommu => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: None,
            },
            Self::IommuNarrow => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(Iommu { aw_bits: 39, ..IOMMU_DEFAULT }),
            },
            Self::IommuNoIntremap => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: Virtio::Absent,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(Iommu { intremap: false, ..IOMMU_DEFAULT }),
            },
            Self::Hda => Shape {
                virtio: Virtio::WithoutSound,
                hda: HDA_ONE,
                ..Self::Headless.shape()
            },
            Self::HdaTwoLive => Shape {
                virtio: Virtio::WithoutSound,
                hda: HDA_TWO_LIVE,
                ..Self::Headless.shape()
            },
        }
    }

    /// The unit this profile puts on the machine, or `None`. A test asserting
    /// on what the guest decoded reads the expectation from here rather than
    /// restating it, exactly as [`Profile::usb_disk`] does for the data stick.
    pub fn iommu(self) -> Option<Iommu> {
        self.shape().iommu
    }

    /// Every `usb-storage` device this profile puts on the bus besides the
    /// boot stick, in creation order. A test asserting on a size or a sector
    /// size has to read it from here rather than restate it.
    pub fn usb_disks(self) -> &'static [UsbDisk] {
        self.shape().usb_disks
    }

    /// The first of them, for the tests that stage exactly one.
    pub fn usb_disk(self) -> Option<(u64, u32)> {
        self.usb_disks().first().map(|d| (d.bytes, d.lba_bytes))
    }
}

pub struct BootOptions {
    pub gdb_stub: bool,
    pub debug_wait: bool,
    pub smp: u32,
    pub profile: Profile,
    /// Open a per-instance QMP socket, which `screendump` needs. Per-instance
    /// because screen tests boot their own QEMU and several may exist at once.
    pub qmp: bool,
    /// Which of [`DECLARED_KERNEL_BUILDS`] this boot wants, and empty for the
    /// kernel an image ships. Only a test whose subject *is* a build sets it —
    /// `fpu-save-nothing`, and the `SYS_DEBUG` boot; everything else names an
    /// actuator in [`BootOptions::kernel_params`] instead.
    ///
    /// It decides what this call *builds*, so it may not be set beside a
    /// [`BootOptions::boot_image`], which is what the guest boots instead —
    /// see [`refuse_a_staged_image_this_boot_did_not_ask_for`].
    pub kernel_features: &'static [&'static str],
    /// The actuators this boot arms, by the names `kernel/src/actuator.rs`
    /// declares. Non-empty selects the test kernel, which carries all of them.
    ///
    /// **The arming is in the image, not in this field.** The names are written
    /// onto the ESP the build produces, so a boot that also supplies a
    /// [`BootOptions::boot_image`] arms whatever *that* image was built with:
    /// the two must agree and are refused when they do not.
    pub kernel_params: &'static [&'static str],
    /// Give the machine an i8042 at all. `-machine q35,i8042=off` is the one
    /// absence scenario QEMU can stage.
    pub i8042: bool,
    /// Take the 16550 away, leaving the framebuffer as the guest's only
    /// channel out. Only [`Profile::Metal`] may set it -- the others carry
    /// their console on it or on virtio-serial. A muted guest has no marker
    /// to wait for and no `run_test` to drive, so it is observed with
    /// [`QemuInstance::screendump_while`] and nothing else.
    pub mute: bool,
    /// The console line that means the boot reached the state under test.
    /// Anything other than [`DEFAULT_READY`] also declares that a panic is the
    /// expected outcome rather than a boot failure -- the early-panic screen
    /// test never reaches userland at all. Ignored when [`BootOptions::mute`]
    /// is set, which leaves no console for a marker to arrive on.
    pub ready_marker: &'static str,
    /// Boot against this disk image instead of the shared scratch one.
    ///
    /// The shared image is created by `create_sparse`, which designates it --
    /// so every ordinary test boots a disk the kernel is allowed to format,
    /// and none of them can observe what it does with one it is not. This is
    /// how a test hands the guest somebody else's disk.
    pub nvme_image: Option<PathBuf>,
    /// Boot this disk image instead of the one this call would build.
    ///
    /// The built image is written fresh every boot and its GPT gets a fresh
    /// random partition GUID with it, so a test that has to know what is on
    /// the boot disk *before* the machine starts cannot use it — and asserting
    /// on the partition table firmware read is exactly that. Such a test
    /// builds the image itself, reads it, and hands it over here.
    ///
    /// **It replaces the image, so it replaces everything in it**: this call
    /// builds nothing when one is set, and every field that would have decided
    /// what went into that image has to agree with what is already in this one
    /// — [`refuse_a_staged_image_this_boot_did_not_ask_for`].
    pub boot_image: Option<PathBuf>,
    /// Back the profile's data disks with these files instead of blank ones,
    /// in the order the profile declares them. The USB gate stages a file
    /// *before* the boot -- the bytes the guest is meant to find are written
    /// there -- and reads it afterwards, so it has to name the file rather
    /// than discover it. Short lists are allowed: the disks past the end get
    /// the blank image their size would have given them anyway.
    pub usb_images: Vec<PathBuf>,
    /// What the emulated RTC reads when the machine starts, as
    /// `YYYY-MM-DDTHH:MM:SS`.
    ///
    /// The wall clock is a device the host can set, which is what makes the
    /// kernel's reading of it checkable from outside the guest: with this
    /// given, the name and the timestamp of the file the guest writes are both
    /// predictable before the machine exists. `None` leaves QEMU's default,
    /// which is the host's own clock in UTC — and leaves the argument off the
    /// command line entirely, so every existing profile assertion sees the argv
    /// it always saw.
    pub rtc_base: Option<&'static str>,
}

/// The in-guest test runner's startup marker.
///
/// It is that runner's own first line and nothing else's. Init spawns its
/// programs without waiting, so this marker orders nothing about any other
/// program's startup — a test asking about a daemon's line waits on the guest
/// for that line ([`await_guest`]), never on a span of host wall clock after
/// this one.
pub const DEFAULT_READY: &str = "===READY===";

impl Default for BootOptions {
    fn default() -> Self {
        Self {
            gdb_stub: false,
            debug_wait: false,
            smp: 2,
            profile: Profile::Headless,
            qmp: false,
            kernel_features: &[],
            kernel_params: &[],
            i8042: true,
            mute: false,
            ready_marker: DEFAULT_READY,
            nvme_image: None,
            boot_image: None,
            usb_images: Vec::new(),
            rtc_base: None,
        }
    }
}

#[derive(Debug)]
pub struct TestResult {
    pub name: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub serial: String,
    /// Every console line that arrived **before** this test announced itself.
    ///
    /// **It used to be dropped on the floor, and that is a hole in the capture
    /// rather than a tidiness.** A boot's capture is `boot_log()` up to the
    /// ready marker and then this function's `stdout`/`serial` from
    /// `===TEST_START===` onwards; between those two points the reader thread
    /// goes on delivering lines and nothing kept them. The window is not
    /// hypothetical and it is not narrow — measured on `wall_clock_file`,
    /// 2026-08-15: one run in three carried five real lines in it, including
    /// `soundd: null sink idle` and the kernel's `spawn: /bin/test-runner`
    /// record, so the ready marker fires before the runner is even loaded and
    /// every daemon still finishing its startup writes into a hole.
    ///
    /// That is how a `logd:` line went missing from a `wall_clock_file` capture
    /// while the *next* line logd writes was present: the two are either side of
    /// a file creation on the log volume, which is milliseconds, and the window
    /// closed between them.
    ///
    /// A caller that reads a daemon's startup out of a boot appends this to its
    /// capture. It is separate from `serial` because `serial` means "while this
    /// test ran" and audio gates count lines in it.
    pub before: String,
    /// Why the run did not finish, when it did not.
    ///
    /// A [`WaitVerdict`] and not a `String`, so that the sentence and the
    /// kernel's own account of its death cannot come apart — see that type.
    /// Every arm that formats this gets the report for free, and there are
    /// fifty-two of them that were never going to be edited one at a time.
    pub error: Option<WaitVerdict>,
    /// Whether the guest ever announced *this* test.
    ///
    /// The in-guest runner reads one command, prints `===TEST_START <name>` and
    /// spawns; so a test that never started is a guest that never got as far as
    /// reading its command, which is a different thing from a test that ran and
    /// hung. On a shared boot the two want different answers — the first is
    /// about the boot, the second about the test.
    pub started: bool,
}

impl TestResult {
    /// The guest is not answering any more: this test's turn came, its whole
    /// ceiling passed, and it was never even announced.
    pub fn boot_stopped_answering(&self) -> bool {
        !self.started && self.error.is_some()
    }
}

pub struct QemuInstance {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    rx: Receiver<String>,
    _reader_thread: thread::JoinHandle<String>,
    audio_wav: PathBuf,
    uart_log: PathBuf,
    nvme: NvmeClaim,
    usb_images: Vec<PathBuf>,
    qmp_socket: Option<PathBuf>,
    screendump: PathBuf,
    /// The image this boot built for itself, which is the only one it may
    /// delete: a [`BootOptions::boot_image`] belongs to the test that staged it
    /// and is often read back after the guest is gone.
    own_boot_image: Option<PathBuf>,
    boot_log: String,
}

/// The bootable disk image a boot with these arguments would use.
///
/// Public because a test that has to know what is on the boot disk *before*
/// the machine starts — or has to put something there — cannot let
/// `boot_with_options` build it: the image is written fresh every boot and its
/// GPT gets a new random partition GUID with it. Such a test builds the image
/// here, works on it, and hands it back through [`BootOptions::boot_image`].
pub fn build_boot_image(
    test_crate: &Path,
    c_tests: &[(String, Vec<u8>)],
    rust_tests: &[(String, Vec<u8>)],
    kernel_params: &[&str],
) -> Vec<u8> {
    let kernel: &[&str] =
        if kernel_params.is_empty() { &[] } else { toyos_build::build::TEST_KERNEL };
    build_boot_image_with(test_crate, c_tests, rust_tests, kernel, kernel_params, false)
}

/// Refuse a staged [`BootOptions::boot_image`] that is not the image this
/// boot's other options describe.
///
/// **A staged image replaces the image this call would have built, so every
/// option that decides what goes *into* an image decides nothing here.** The
/// guest boots the kernel that image ships, armed with the actuators it was
/// built with, and until this refused, a test that set `kernel_params` beside a
/// `boot_image` built without them got an unarmed guest, a pass, and a summary
/// line counting the arm as taken. Measured 2026-08-22: `usb-flush-fails` armed
/// through `kernel_params` alone on `esp_filesystem` passed with no injected
/// sense anywhere in the log, while the same actuator baked into the image
/// failed the same assertion.
///
/// A green run with an inert arm is the worst kind of harness defect, because
/// every negative control staged through one proves nothing.
///
/// The image was built by this same process moments earlier and carries its own
/// list on its own ESP, so the question is asked of the image rather than of
/// whoever built it — a name is a name on this side of the wire too, and the
/// guest need not be started to know which kind it is.
fn refuse_a_staged_image_this_boot_did_not_ask_for(image: &Path, options: &BootOptions) {
    assert!(
        options.kernel_features.is_empty(),
        "[qemu] this boot asks for the kernel build {:?} and hands the guest {}; a staged image \
         ships the kernel it was built with and this call builds nothing, so the request would \
         be inert",
        options.kernel_features,
        image.display(),
    );
    assert!(
        !options.debug_wait,
        "[qemu] this boot asks for the {:?} build and hands the guest {}; a staged image ships \
         the kernel it was built with and this call builds nothing, so the request would be \
         inert",
        toyos_build::build::DEBUG_KERNEL_BUILD,
        image.display(),
    );
    if let Some(why) = toyos_build::image::param_conflict(image, options.kernel_params) {
        panic!(
            "[qemu] {why}. `BootOptions::boot_image` replaces the image this call would have \
             built, so `kernel_params` cannot arm a guest booting one: build the staged image \
             with the same list — `qemu::build_boot_image` takes it — or drop the field"
        );
    }
}

/// Which of [`DECLARED_KERNEL_BUILDS`] this boot wants.
///
/// **A parameter never decides a build.** Every actuator lives in the one test
/// kernel, so asking for one selects that kernel and nothing more; the third
/// build is asked for by name and by one test.
///
/// A boot handed a [`BootOptions::boot_image`] builds nothing at all, and this
/// then answers what that image already carries: the two agree or the boot was
/// refused before it got here.
fn kernel_of(options: &BootOptions) -> Vec<&'static str> {
    if options.kernel_params.is_empty() {
        return options.kernel_features.to_vec();
    }
    assert!(
        options.kernel_features.is_empty(),
        "a boot asking to arm {:?} also asks for the kernel build {:?}; an actuator is a \
         parameter and the test kernel carries all of them",
        options.kernel_params,
        options.kernel_features,
    );
    toyos_build::build::TEST_KERNEL.to_vec()
}

fn build_boot_image_with(
    test_crate: &Path,
    c_tests: &[(String, Vec<u8>)],
    rust_tests: &[(String, Vec<u8>)],
    kernel_features: &[&str],
    kernel_params: &[&str],
    debug_wait: bool,
) -> Vec<u8> {
    // **The two fields have the same type, so swapping them compiles.** It
    // happened once, in this file's own conversion: the shared boot handed
    // `["boot-actuators", "test-actuators"]` to `kernel_params` and every
    // `SYS_DEBUG` test died with the kernel refusing `boot-actuators` as a
    // parameter it does not declare. The kernel's refusal is what found it and
    // it is the right refusal, but a name is a name on this side of the wire
    // too, and the guest need not be started to know which kind it is.
    static ACTUATORS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let actuators =
        ACTUATORS.get_or_init(|| toyos_build::build::declared_actuators(&compile::repo_root()));
    for name in kernel_params {
        assert!(
            actuators.iter().any(|a| a == name),
            "{name:?} is a `kernel_params` and `kernel/src/actuator.rs` declares no such actuator"
        );
    }
    for name in kernel_features {
        assert!(
            !actuators.iter().any(|a| a == name),
            "{name:?} is an actuator and was passed as a `kernel_features`; it is a boot \
             parameter, so it belongs in `kernel_params`"
        );
    }

    let joined = kernel_features.join(",");
    assert!(
        toyos_build::build::harness_kernel_build_is_declared(&joined, debug_wait),
        "this boot asks for the kernel build {joined:?}, which is not one of the {} an ordinary \
         suite run may make: {DECLARED_KERNEL_BUILDS:?}; interactive debug mode may instead \
         make {:?}",
        DECLARED_KERNEL_BUILDS.len(),
        toyos_build::build::DEBUG_KERNEL_BUILD,
    );
    KERNELS.lock().expect("the kernel census").insert(joined);
    let mut extra_files: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, data) in c_tests {
        extra_files.push((format!("bin/test_c_{name}"), data.clone()));
    }
    for (name, data) in rust_tests {
        if name.ends_with(".so") {
            extra_files.push((format!("lib/{name}"), data.clone()));
        } else {
            extra_files.push((format!("bin/test_rs_{name}"), data.clone()));
        }
    }

    let config_path = test_crate.join("system.toml");
    assert!(
        config_path.exists(),
        "Test crate missing system.toml: {}",
        config_path.display()
    );

    let quiet = !VERBOSE.load(Ordering::Relaxed);
    toyos_build::build::build_test_image(
        &compile::repo_root(),
        &config_path,
        kernel_features,
        kernel_params,
        quiet,
        &extra_files,
    )
}

/// Build all binaries in a test crate.
pub fn build_toyos_bins(crate_path: &Path) -> Vec<(String, Vec<u8>)> {
    let repo = compile::repo_root();
    let quiet = !VERBOSE.load(Ordering::Relaxed);
    toyos_build::build::build_toyos_bins(&repo, crate_path, quiet)
}

/// All kernel serial output goes through log!() which prepends "[kernel ...]".
/// User program output goes through serial::write directly with no prefix.
pub fn is_kernel_line(line: &str) -> bool {
    line.starts_with("[kernel ")
}

/// File the userland half of one captured console line under `stdout`.
///
/// The kernel drains its own records straight to the backend rather than
/// through any process's line buffer, so a record can follow bytes a program
/// left unterminated and the host's splitter joins the two. Splitting the
/// record back off is what has always let a `printf` with no newline reach a
/// capture at all — and it is why `71_macro_empty_arg` passes most runs and
/// not all: when the next writer is *userland* rather than the kernel there is
/// no `[kernel ` to cut at. That half is `common::console`'s, on the boot
/// config's own list of who else may speak; this is only the kernel's.
fn push_user_half(line: &str, stdout: &mut String) {
    if is_kernel_line(line) {
        return;
    }
    match line.find("[kernel ") {
        Some(idx) => stdout.push_str(&line[..idx]),
        None => stdout.push_str(line),
    }
    stdout.push('\n');
}

/// The in-guest runner's end-of-test marker. Matched anywhere in the line, not
/// as a prefix: the virtio-console is shared and not line-atomic, so a daemon
/// mid-`println!` pushes the marker into the middle of its line. Anchoring on
/// the prefix made the harness miss the marker and time out — measured at 1 in
/// 120 audio boots, where it looked like a guest hang rather than a lost line.
const END_MARKER: &str = "===TEST_END ";

impl QemuInstance {
    /// Build everything and boot QEMU with test binaries in the initrd.
    /// `test_crate` is the path to the test crate (must contain a `system.toml`).
    pub fn boot(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
    ) -> Self {
        Self::boot_with_options(test_crate, c_tests, rust_tests, BootOptions::default())
    }

    pub fn boot_with_options(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
        options: BootOptions,
    ) -> Self {
        if let Some(staged) = &options.boot_image {
            refuse_a_staged_image_this_boot_did_not_ask_for(staged, &options);
        }
        let mut features: Vec<&str> = kernel_of(&options);
        if options.debug_wait {
            features.push(toyos_build::build::DEBUG_KERNEL_BUILD);
        }
        BOOTS.fetch_add(1, Ordering::Relaxed);
        if !features.is_empty() {
            FEATURE_BOOTS.fetch_add(1, Ordering::Relaxed);
        }

        let test_dir = super::lane::dir();
        let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);

        // Named for the boot rather than for the process. Two guests handed one
        // image file is not a slow test, it is a guest reading bytes another
        // boot is in the middle of writing — and the lane directory alone would
        // not settle it, since one test may hold two instances at once.
        //
        // **A staged image builds nothing.** What this call would have built is
        // the image the guest does not boot, and building it anyway cost a
        // kernel build the run then reported as one it had made — see
        // [`refuse_a_staged_image_this_boot_did_not_ask_for`] for what that
        // report was worth.
        let boot_image = match &options.boot_image {
            Some(staged) => staged.clone(),
            None => {
                let disk = build_boot_image_with(
                    test_crate,
                    c_tests,
                    rust_tests,
                    &features,
                    options.kernel_params,
                    options.debug_wait,
                );
                let path = test_dir.join(format!("boot-{seq}.img"));
                fs::write(&path, &disk).expect("Failed to write test boot image");
                path
            }
        };
        let own_boot_image = options.boot_image.is_none().then(|| boot_image.clone());

        // Named by size, so two profiles that disagree about the device do
        // not hand each other a filesystem formatted for the wrong one. Reused
        // across the boots of one lane and shared with no other — which is what
        // `super::lane` is for, and why this is not a per-boot name.
        //
        // One live guest per image, claimed here rather than discovered from
        // QEMU's stderr after the second process has already exited — see
        // [`NvmeClaim`].
        let nvme_bytes = options.profile.shape().nvme_bytes;
        let nvme_image = match &options.nvme_image {
            Some(path) => path.clone(),
            // A profile with no controller gets no backing file either; the
            // path is never passed to QEMU.
            None if nvme_bytes == 0 => test_dir.join("no-nvme"),
            None => {
                let path = test_dir.join(format!("test-nvme-{nvme_bytes}.img"));
                if !path.exists() {
                    toyos_build::build::create_sparse(&path, nvme_bytes);
                }
                path
            }
        };
        let nvme = if nvme_bytes == 0 {
            NvmeClaim::unattached(&nvme_image)
        } else {
            NvmeClaim::take(&nvme_image).unwrap_or_else(|why| panic!("[qemu] {why}"))
        };

        // Named by size and block size for the same reason the namespace is:
        // a stamped image is stamped for one geometry, and handing it to a
        // profile that declares another is the mistake the stamp exists to
        // catch rather than one to make here.
        let usb_images: Vec<PathBuf> = options
            .profile
            .usb_disks()
            .iter()
            .enumerate()
            .map(|(i, disk)| match options.usb_images.get(i) {
                Some(path) => path.clone(),
                None => {
                    let path =
                        test_dir.join(format!("test-usb-{}-{}.img", disk.bytes, disk.lba_bytes));
                    if !path.exists() {
                        let file = fs::File::create(&path).expect("create the USB disk image");
                        file.set_len(disk.bytes).expect("size the USB disk image");
                    }
                    path
                }
            })
            .collect();

        let audio_wav = test_dir.join(format!("audio-{seq}.wav"));
        let _ = fs::remove_file(&audio_wav);

        let qmp_socket = options.qmp.then(|| test_dir.join(format!("qmp-{seq}.sock")));
        if let Some(path) = &qmp_socket {
            let _ = fs::remove_file(path);
        }
        let screendump = test_dir.join(format!("screen-{seq}.ppm"));

        // Per-instance, not a fixed /tmp path: the audio gate boots dozens of
        // guests and a screen test waits on this file, so a shared one would
        // let instances read each other's early boot.
        let uart_log = test_dir.join(format!("uart-{seq}.log"));
        let _ = fs::remove_file(&uart_log);

        let qemu = qemu_command(
            &boot_image,
            nvme.path(),
            &usb_images,
            &audio_wav,
            &uart_log,
            qmp_socket.as_deref(),
            &options,
        );
        spawn_and_wait_ready(
            qemu,
            &options,
            Files {
                seq,
                audio_wav,
                uart_log,
                nvme,
                usb_images,
                qmp_socket,
                screendump,
                own_boot_image,
            },
        )
    }

    /// Capture the guest's scanout through QMP and return the decoded PPM.
    ///
    /// After a halt the guest is stopped, so the dump is stable. QEMU writes
    /// the file itself, so the only synchronization needed is the command's
    /// own reply.
    pub fn screendump(&mut self) -> super::screen::Ppm {
        let socket = self
            .qmp_socket
            .clone()
            .expect("screendump needs BootOptions { qmp: true }");
        let out = self.screendump.clone();
        let _ = fs::remove_file(&out);

        // A guest that triple-faults exits QEMU (`-no-reboot`), and the
        // socket then refuses every connect. Without this the retry loop
        // spends its full ten seconds and reports `qmp: cannot connect`,
        // which says nothing about what happened — the worst diagnostic the
        // harness produces, for the failure class the metal profile exists to
        // catch. `wait_for_ready` reports the same event properly, but a muted
        // guest never goes through it and no guest goes through it twice.
        let child = &mut self.child;
        let mut qmp = Qmp::connect_while(&socket, || {
            if let Ok(Some(status)) = child.try_wait() {
                panic!("[qemu] QEMU died before the screendump (status: {status})");
            }
        });
        qmp.execute(&format!(
            "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":\"{}\"}}}}",
            out.display()
        ));

        let bytes = fs::read(&out).expect("screendump: QEMU wrote no file");
        super::screen::Ppm::parse(&bytes)
    }

    /// Screendump until the decoded screen carries `needle`, or the timeout.
    ///
    /// Every fatal path needs this, for one of two reasons. The panic
    /// handler's own path paints after the drain that emits the report, so a
    /// marker on serial does not yet prove a paint. The halt_all_cpus paths
    /// are the other way round and once *did* need only a single dump — but a
    /// report too long for one screen now pages, so the screen a marker
    /// proves is only the first of several and any given dump may hold a
    /// different one.
    pub fn screendump_until(&mut self, needle: &str, timeout: Duration) -> super::screen::Ppm {
        self.screendump_while(timeout, Duration::from_millis(100), |dump| {
            dump.text().contains(needle)
        })
    }

    /// Screendump until `done`, or the timeout. A muted guest has no console,
    /// so this is the only way to observe that boot at all — and what it
    /// watches for is a pixel pattern, not text.
    ///
    /// Returns the last dump either way; a caller that timed out gets the
    /// screen as its diagnostic, which under metal-sim is where the kernel's
    /// boot checkpoints and panic report are.
    pub fn screendump_while(
        &mut self,
        timeout: Duration,
        interval: Duration,
        done: impl Fn(&super::screen::Ppm) -> bool,
    ) -> super::screen::Ppm {
        let deadline = Instant::now() + budget(timeout);
        loop {
            let dump = self.screendump();
            if done(&dump) || Instant::now() >= deadline {
                return dump;
            }
            thread::sleep(interval);
        }
    }

    /// Every console line the guest printed before the ready marker.
    ///
    /// The kernel's own boot lines sit in the log ring until the scheduler
    /// drains them, by which time the virtio-console is the backend — so the
    /// 16550 file holds only the bootloader, and this is the only place a
    /// host test can read what the kernel said while booting. Under
    /// [`Profile::Metal`] the 16550 is the console and carries everything;
    /// empty when [`BootOptions::mute`] takes it away.
    pub fn boot_log(&self) -> &str {
        &self.boot_log
    }

    /// Everything the guest put on the 16550 before it switched to the
    /// virtio-console — the only record a guest that died early leaves.
    pub fn uart_log(&self) -> String {
        fs::read_to_string(&self.uart_log).unwrap_or_default()
    }

    /// The wav file the virtio-sound device records into for this boot.
    /// The RIFF size fields stay 0 until QEMU exits cleanly — parse to EOF.
    pub fn audio_wav_path(&self) -> &Path {
        &self.audio_wav
    }

    /// The NVMe backing file. It is what the *device* received, so it is the
    /// only place a storage assertion can stand outside the guest's own
    /// account of itself.
    pub fn nvme_image(&self) -> &Path {
        self.nvme.path()
    }

    /// End this guest and hand back the proof its lane is free.
    ///
    /// **This is the only way to boot a replacement**, because [`LaneFree`] is
    /// the only thing a replacement can be built from and this is the only
    /// thing that makes one out of a guest. Taking `self` is the whole of it:
    /// `qemu = boot()` launched the new QEMU while the old instance still held
    /// the lane's `test-nvme-*.img` open for write, the new one exited 1 on
    /// QEMU's own lock, and `wait_for_ready`'s panic escaped the shared block —
    /// 129 of one run's 131 reds carried that one sentence on 2026-08-17.
    /// Deterministic, not a race in the sense of a window: the old guest is
    /// always still alive at that point, so every shared-boot reboot since the
    /// mechanism landed on 2026-08-08 died this way.
    pub fn shutdown(self) -> LaneFree {
        drop(self);
        LaneFree(())
    }

    /// The data disks' backing files, which is what the *devices* received.
    /// The guest's own account of a write it made is the thing under test, so
    /// it cannot also be the evidence.
    pub fn usb_images(&self) -> &[PathBuf] {
        &self.usb_images
    }

    pub fn stdin_mut(&mut self) -> &mut BufWriter<ChildStdin> {
        &mut self.stdin
    }

    pub fn flush_stdin(&mut self) {
        self.stdin.flush().expect("Failed to flush QEMU stdin");
    }

    /// Keep collecting serial output for `dur` after a test has returned.
    /// soundd flushes its final stats window when the last client leaves,
    /// which races the client process's exit — so the line the audio gate
    /// reads lands on either side of `===TEST_END===`.
    /// **Not scaled by the width**, and it is the one duration in this file that
    /// is not. Callers use it to *pace* — "let the guest run for 400 ms and tell
    /// me what it said" — so multiplying it does not buy a slow guest more room,
    /// it buys the test a longer sleep. `metal_sim_pointer_churn` has
    /// twenty-four of these; scaled, they made it an 86 s job at width 8 and the
    /// critical path of the whole phase.
    pub fn drain_serial(&mut self, dur: Duration) -> String {
        self.drain_for(dur, |_| false)
    }

    /// Drain until `line` reads true of a line just seen, or until the guest
    /// goes quiet for the rest of `dur`.
    ///
    /// A guest that is *shut down* ends a plain [`Self::drain_serial`] the
    /// moment QEMU exits and the reader disconnects, so the ceiling there costs
    /// nothing. A guest the fatal path has halted does not exit — every CPU is
    /// stopped and the process stays up — so the drain pays the whole ceiling
    /// waiting for a machine that will never speak again. `double_fault_stack`
    /// spent twenty seconds of every run that way, which was 80% of it.
    ///
    /// Here the duration *is* a liveness ceiling — the marker is what ends
    /// it — so it scales.
    pub fn drain_until(&mut self, dur: Duration, line: impl Fn(&str) -> bool) -> String {
        self.drain_for(budget(dur), line)
    }

    fn drain_for(&mut self, dur: Duration, line: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + dur;
        let mut out = String::new();
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return out;
            };
            match self.rx.recv_timeout(remaining) {
                Ok(seen) => {
                    out.push_str(&seen);
                    out.push('\n');
                    if line(&seen) {
                        return out;
                    }
                }
                Err(RecvTimeoutError::Timeout) => return out,
                Err(RecvTimeoutError::Disconnected) => return out,
            }
        }
    }

    /// Wait for `marker` on the console, or the timeout.
    ///
    /// A console is a stream and this consumes it: every line up to and
    /// including the marker is taken from whatever reads next.
    pub fn wait_for_console(&mut self, marker: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + budget(timeout);
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.rx.recv_timeout(left) {
                Ok(line) if line.contains(marker) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    /// Send `command` and wait for `marker` on the console.
    ///
    /// For a guest that will never report `===TEST_END`, which is any guest
    /// the fatal path has run through: every CPU is halted by the time the
    /// marker arrives.
    pub fn command_until(&mut self, command: &str, marker: &str, timeout: Duration) -> bool {
        writeln!(self.stdin, "{command}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");
        self.wait_for_console(marker, timeout)
    }

    /// The QMP socket this instance opened. Injection needs it, and it needs
    /// `BootOptions { qmp: true }`.
    pub fn qmp_socket(&self) -> &Path {
        self.qmp_socket.as_ref().expect("qmp_socket needs BootOptions { qmp: true }")
    }

    pub fn run_test(&mut self, name: &str, timeout: Duration) -> TestResult {
        self.run_test_hooked(name, timeout, "", |_| {})
    }

    /// `run_test`, with `action` run once the guest prints `ready_line`.
    ///
    /// The hook is inside the read loop because that is the only place the
    /// two facts meet: the guest is holding the keyboard claim, and the host has
    /// not injected yet. A sleep would be a guess in both directions.
    pub fn run_test_hooked(
        &mut self,
        name: &str,
        timeout: Duration,
        ready_line: &str,
        action: impl FnOnce(&Path),
    ) -> TestResult {
        let mut action = Some(action);
        self.run_test_paced(name, timeout, |socket, line| {
            if ready_line.is_empty() || !line.contains(ready_line) {
                return;
            }
            if let Some(action) = action.take() {
                action(socket.expect("run_test_hooked needs BootOptions { qmp: true }"));
            }
        })
    }

    /// `run_test`, with `step` run on every console line the guest prints.
    ///
    /// [`Self::run_test_hooked`] injects a whole sequence in one call and holds
    /// the reader while it does, so the host runs at its own speed and what
    /// reaches the guest is whatever survived the queues in between — a packet
    /// the guest was never given reads exactly like one it lost. A step driven
    /// by the guest's own output can stay behind it, which is how an injection
    /// test costs a slow guest wall-clock instead of a verdict.
    pub fn run_test_paced(
        &mut self,
        name: &str,
        timeout: Duration,
        mut step: impl FnMut(Option<&Path>, &str),
    ) -> TestResult {
        writeln!(self.stdin, "run {name}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");

        let mut fire =
            |line: &str, socket: Option<&PathBuf>| step(socket.map(PathBuf::as_path), line);

        // `run <name> [args...]`, and the markers carry only the binary name.
        let want = name.split_whitespace().next().unwrap_or(name);

        let timeout = budget(timeout);
        let start = Instant::now();
        let mut stdout = String::new();
        let mut serial = String::new();
        // Every line seen before this test announced itself. Kept, never
        // dropped — `TestResult::before` is the argument.
        let mut before = String::new();
        let mut in_test = false;
        // **Which of the two things the ceiling caught.** A test's `timeout` is
        // a liveness guard and never a verdict, and until now its expiry said
        // only how many seconds had passed — `metal_sim_client_death` 364 s,
        // `metal_sim_window_drag` 355 s, `desktop_audio_client` 354 s and
        // `blocked_dump` 329 s in run `31250706113`, four reds indistinguishable
        // from four slow tests. The console tells them apart for free, and the
        // fix `1cf7fee` made to the waits *inside* a test never reached this
        // one: a guest that has said nothing for [`GUEST_QUIET`] has stopped,
        // and one still talking at the ceiling has not.
        let mut last_line = Instant::now();
        let mut lines = 0usize;
        // **The line on which the kernel said it was dying, if it ever did.**
        // The first one only: a crash report's later lines carry the spelling
        // too, and the header is the one worth quoting. What it buys is in
        // [`ceiling_verdict`] — until it existed, a Rust `panic!` in the kernel
        // matched nothing here, the machine halted, and the whole guard expired
        // onto a verdict that said the guest had stopped answering.
        let mut dying: Option<String> = None;

        loop {
            if let Some(error) = ceiling_verdict(
                dying.as_deref(),
                start.elapsed(),
                timeout,
                last_line.elapsed(),
                lines,
            ) {
                // The window in the order the guest wrote it: `before` holds
                // every line up to `===TEST_START===` and `serial` everything
                // after, so a kernel that died before this test announced
                // itself has its report found in the first and one that died
                // during it in the second.
                let error = WaitVerdict::new(error, &[&before, &serial]);
                return TestResult {
                    name: name.to_string(),
                    exit_code: None,
                    stdout,
                    serial,
                    before,
                    error: Some(error),
                    started: in_test,
                };
            }

            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    last_line = Instant::now();
                    lines += 1;
                    fire(&line, self.qmp_socket.as_ref());
                    if dying.is_none()
                        && super::serial::died(&line) == Some(super::serial::Died::Kernel)
                    {
                        dying = Some(line.clone());
                    }
                    if line.contains(&format!("===TEST_START {want}===")) {
                        in_test = true;
                    } else if let Some(at) = line.find(END_MARKER) {
                        let rest = &line[at + END_MARKER.len()..];
                        let rest = rest.split_once("===").map_or(rest, |(head, _)| head);
                        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                        // **A marker naming another test is the previous one's**,
                        // and taking it was `issues/build/`'s cascade:
                        // one timed-out test left the guest still producing its
                        // output, every later member of the block read a window
                        // that opened on it, and 110 of 238 went red on an
                        // "actual" that was verbatim the previous expectation.
                        // The name has been on the wire the whole time.
                        if parts[0] != want {
                            continue;
                        }
                        // Everything before the marker is what some console
                        // writer had said without a newline when the runner
                        // printed; it is still real output and the audio gate
                        // reads soundd's stats out of it.
                        //
                        // **And it goes to `stdout` as well, because the writer
                        // is usually the test's own child.** A program whose
                        // output does not end in a newline — `printf("%d", …)`
                        // and nothing after it — has its last bytes flushed by
                        // `ConsoleObject::drop` with no terminator, so the
                        // runner's `===TEST_END` lands on the same line the
                        // host's splitter builds. Filing that head under
                        // `serial` alone is how `71_macro_empty_arg` came back
                        // with an *empty* capture against an expected `17` —
                        // the half no filter over whole lines reaches, and
                        // `common::console` has the rest of it. Nothing is
                        // dropped either way; this only stops the capture from
                        // losing its own tail.
                        if at > 0 && in_test {
                            let head = &line[..at];
                            serial.push_str(head);
                            serial.push('\n');
                            push_user_half(head, &mut stdout);
                        }
                        let (exit_code, error) = if parts.len() > 1 {
                            if let Some(code_str) = parts[1].strip_prefix("exit=") {
                                (code_str.parse::<i32>().ok(), None)
                            } else if let Some(err) = parts[1].strip_prefix("error=") {
                                (None, Some(err.to_string()))
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        };
                        // The guest's runner said this one, so the capture is
                        // handed over for the same reason: a runner reporting
                        // an error on a machine whose kernel had already died
                        // is reporting the smaller of the two facts.
                        let error = error.map(|e| WaitVerdict::new(e, &[&before, &serial]));
                        return TestResult {
                            name: name.to_string(),
                            exit_code,
                            stdout,
                            serial,
                            before,
                            error,
                            started: in_test,
                        };
                    } else if !in_test {
                        // **The window between two tests, kept rather than
                        // dropped.** See [`TestResult::before`].
                        before.push_str(&line);
                        before.push('\n');
                    } else if in_test {
                        serial.push_str(&line);
                        serial.push('\n');
                        push_user_half(&line, &mut stdout);
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    // QEMU going away is a sentence about the host process, and
                    // a guest whose kernel panicked on the way out wrote down
                    // the reason first.
                    let error =
                        WaitVerdict::new(String::from("QEMU disconnected"), &[&before, &serial]);
                    return TestResult {
                        name: name.to_string(),
                        exit_code: None,
                        stdout,
                        serial,
                        before,
                        error: Some(error),
                        started: in_test,
                    };
                }
            }
        }
    }
}

impl Drop for QemuInstance {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        // **Reaped, not merely signalled.** The `NvmeClaim` field is released
        // after this body returns, and what makes that release true rather than
        // hopeful is that the process whose descriptors hold QEMU's write lock
        // on the image is gone by the time it happens.
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.audio_wav);
        // **The 16550's log outlives the guest, because it is the one channel
        // that exists before the console does.** Firmware, the bootloader and
        // the kernel up to the backend switch write here and nowhere else, so a
        // boot that dies before virtio-console comes up leaves this file and an
        // empty capture — which is exactly the shape `issues/diagnostics/`
        // records as looking like a kernel that never started. 1.4 KB on a
        // healthy `tests/testcases` boot, measured, against the hundreds of
        // megabytes of per-boot image beside it.
        //
        // Deleting it here is also why `ci.yml`'s "what a red run left" step had
        // never uploaded one byte: run `31252989653` reds four shards and
        // publishes twelve duration files and no scratch artifact at all, which
        // reads as "there was nothing to keep" rather than "it was deleted
        // before the step ran".
        let _ = fs::remove_file(&self.screendump);
        if let Some(socket) = &self.qmp_socket {
            let _ = fs::remove_file(socket);
        }
        // A per-boot image is hundreds of megabytes and a full run makes ~76 of
        // them; the shared name used to make that one file.
        if let Some(image) = &self.own_boot_image {
            let _ = fs::remove_file(image);
        }
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A QMP session. Line-delimited JSON: greeting, `qmp_capabilities`, then
/// commands; the reply carrying `return` is the completion signal. A handful
/// of commands with fixed shapes does not justify a JSON dependency.
struct Qmp {
    stream: std::os::unix::net::UnixStream,
    pending: Vec<u8>,
}

impl Qmp {
    fn connect(socket: &Path) -> Self {
        Self::connect_while(socket, || {})
    }

    /// `on_retry` runs between connect attempts. It is where a caller holding
    /// the QEMU process turns "connection refused" into "QEMU is gone, and
    /// here is its exit status" — see [`QemuInstance::screendump`].
    fn connect_while(socket: &Path, mut on_retry: impl FnMut()) -> Self {
        use std::os::unix::net::UnixStream;
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(e) => {
                    on_retry();
                    assert!(
                        Instant::now() < deadline,
                        "qmp: cannot connect to {}: {e}",
                        socket.display()
                    );
                    thread::sleep(Duration::from_millis(50));
                }
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut qmp = Self { stream, pending: Vec::new() };
        qmp.await_reply("\"QMP\"");
        qmp.execute("{\"execute\":\"qmp_capabilities\"}");
        qmp
    }

    fn await_reply(&mut self, want: &str) {
        use std::io::Read;
        let start = Instant::now();
        loop {
            if let Some(pos) =
                self.pending.windows(want.len()).position(|w| w == want.as_bytes())
            {
                self.pending.drain(..pos + want.len());
                return;
            }
            // A refused command never produces a `return`, so without this the
            // wait spends its whole timeout and reports `qmp: read failed` —
            // which says nothing about the command QEMU declined or why.
            if let Some(at) = self.pending.windows(7).position(|w| w == b"\"error\"") {
                panic!(
                    "qmp: refused while waiting for {want}: {}",
                    String::from_utf8_lossy(&self.pending[at..])
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "qmp: no {want} in reply: {}",
                String::from_utf8_lossy(&self.pending)
            );
            let mut buf = [0u8; 4096];
            let n = self.stream.read(&mut buf).expect("qmp: read failed");
            assert!(n > 0, "qmp: socket closed waiting for {want}");
            self.pending.extend_from_slice(&buf[..n]);
        }
    }

    fn execute(&mut self, command: &str) {
        self.stream.write_all(command.as_bytes()).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.await_reply("\"return\"");
    }

    /// `execute`, keeping what the command answered with. Only the human
    /// monitor answers with anything; every other command here returns `{}`.
    fn execute_capturing(&mut self, command: &str) -> String {
        use std::io::Read;
        self.stream.write_all(command.as_bytes()).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.await_reply("\"return\"");
        let rest = loop {
            if let Some(at) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=at).collect();
                break String::from_utf8_lossy(&line).into_owned();
            }
            let mut buf = [0u8; 4096];
            let n = self.stream.read(&mut buf).expect("qmp: read failed");
            assert!(n > 0, "qmp: socket closed mid-reply");
            self.pending.extend_from_slice(&buf[..n]);
        };
        let Some(body) = rest.split_once('"').map(|(_, tail)| tail) else {
            return String::new();
        };
        let body = body.rsplit_once('"').map_or(body, |(head, _)| head);
        let mut out = String::with_capacity(body.len());
        let mut chars = body.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => {}
                Some(escaped) => out.push(escaped),
                None => break,
            }
        }
        out
    }
}

/// An open QMP connection to QEMU's human monitor, for the questions QMP has
/// no command of its own for.
pub struct QmpMonitor(Qmp);

impl QmpMonitor {
    pub fn open(socket: &Path) -> Self {
        Self(Qmp::connect(socket))
    }

    /// Run `command` in the human monitor and return what it printed.
    pub fn human(&mut self, command: &str) -> String {
        self.0.execute_capturing(&format!(
            "{{\"execute\":\"human-monitor-command\",\"arguments\":\
             {{\"command-line\":\"{command}\"}}}}"
        ))
    }
}

/// An open QMP connection for injecting input.
///
/// One connection rather than one per event, because QEMU delivers each
/// `input-send-event` as its own input sync — so a thousand pointer packets
/// is a thousand commands, and a thousand reconnects on top of that is the
/// difference between a second and a minute.
pub struct QmpInput(Qmp);

impl QmpInput {
    pub fn open(socket: &Path) -> Self {
        Self(Qmp::connect(socket))
    }

    fn send(&mut self, body: &[String]) {
        if body.is_empty() {
            return;
        }
        self.0.execute(&format!(
            "{{\"execute\":\"input-send-event\",\"arguments\":{{\"events\":[{}]}}}}",
            body.join(",")
        ));
    }

    /// Every key transition in `events` as one batch, so a chord like Shift+B
    /// arrives as a chord rather than as a race.
    pub fn keys(&mut self, events: &[(&str, bool)]) {
        let body: Vec<String> = events
            .iter()
            .map(|(qcode, down)| {
                format!(
                    "{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":{{\"type\":\"qcode\",\"data\":\"{qcode}\"}}}}}}"
                )
            })
            .collect();
        self.send(&body);
    }

    /// Type `text` on the guest's keyboard, one character at a time.
    ///
    /// The gap between characters is the wire's, not a settling delay for the
    /// assertion: the i8042 carries one scancode per interrupt and the guest
    /// has to drain each before the next, which is the same reason
    /// `metal_sim_input` spaces its five keys.
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            let (qcode, shift) = qcode(ch);
            if shift {
                self.keys(&[("shift", true), (qcode, true), (qcode, false), ("shift", false)]);
            } else {
                self.keys(&[(qcode, true), (qcode, false)]);
            }
            thread::sleep(Duration::from_millis(15));
        }
    }

    /// `times` relative moves of `dx`, all in one command.
    ///
    /// QEMU syncs its input once per command and its PS/2 device *accumulates*
    /// motion between syncs, so this is one packet carrying the sum however
    /// many moves it names — the deterministic form of what a host holding more
    /// packets outstanding than that device's queue meets by accident.
    pub fn mouse_merged(&mut self, dx: i32, times: usize) {
        let body: Vec<String> = (0..times)
            .map(|_| format!("{{\"type\":\"rel\",\"data\":{{\"axis\":\"x\",\"value\":{dx}}}}}"))
            .collect();
        self.send(&body);
    }

    /// One pointer packet: relative motion and/or a button transition.
    pub fn mouse(&mut self, dx: i32, dy: i32, button: Option<(&str, bool)>) {
        let mut body: Vec<String> = Vec::new();
        if let Some((name, down)) = button {
            body.push(format!(
                "{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"{name}\"}}}}"
            ));
        }
        for (axis, value) in [("x", dx), ("y", dy)] {
            if value != 0 {
                body.push(format!(
                    "{{\"type\":\"rel\",\"data\":{{\"axis\":\"{axis}\",\"value\":{value}}}}}"
                ));
            }
        }
        self.send(&body);
    }
}

/// The QEMU qcode for `ch`, and whether Shift is held to produce it.
///
/// A US layout, because that is what `kernel/src/keyboard.rs` boots with. Only
/// the characters a console test types: an unmapped one panics rather than
/// being dropped, since a command missing a character is a test asserting on
/// output nothing was ever asked to produce.
fn qcode(ch: char) -> (&'static str, bool) {
    const LOWER: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    const DIGIT: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    match ch {
        'a'..='z' => (LOWER[ch as usize - 'a' as usize], false),
        'A'..='Z' => (LOWER[ch as usize - 'A' as usize], true),
        '0'..='9' => (DIGIT[ch as usize - '0' as usize], false),
        ' ' => ("spc", false),
        '\n' => ("ret", false),
        '-' => ("minus", false),
        '_' => ("minus", true),
        '.' => ("dot", false),
        '/' => ("slash", false),
        '&' => ("7", true),
        _ => panic!("no qcode for {ch:?}; add it rather than typing something else"),
    }
}

pub fn qmp_send_keys(socket: &Path, events: &[(&str, bool)]) {
    QmpInput::open(socket).keys(events);
}

/// An open QMP connection for attaching and detaching devices while the guest
/// runs — QEMU's own `device_add`/`device_del`, which is what a person
/// plugging something in looks like from the host side.
///
/// Its own type rather than more methods on [`QmpInput`], and never open at the
/// same time as one: a `-qmp unix:…,server` socket serves one monitor, so a
/// caller that needs both alternates. A type called `QmpInput` with
/// `device_add` on it would also be describing the wrong thing.
pub struct QmpDevices(Qmp);

impl QmpDevices {
    pub fn open(socket: &Path) -> Self {
        Self(Qmp::connect(socket))
    }

    /// Attach `driver` on `bus` as `id`, with `extra` naming any further
    /// properties. Every value is a bare JSON string, which is what every
    /// property these tests set happens to be.
    pub fn add(&mut self, driver: &str, bus: &str, id: &str, extra: &[(&str, &str)]) {
        let mut args = format!("\"driver\":\"{driver}\",\"bus\":\"{bus}\",\"id\":\"{id}\"");
        for (key, value) in extra {
            args.push_str(&format!(",\"{key}\":\"{value}\""));
        }
        self.0.execute(&format!("{{\"execute\":\"device_add\",\"arguments\":{{{args}}}}}"));
    }

    pub fn del(&mut self, id: &str) {
        self.0
            .execute(&format!("{{\"execute\":\"device_del\",\"arguments\":{{\"id\":\"{id}\"}}}}"));
    }

    /// Give QEMU an image to back a device that is not on the machine yet, so
    /// a hot-plugged disk needs nothing in argv. A disk declared at boot is a
    /// disk the guest could have enumerated at boot.
    pub fn blockdev_add(&mut self, node: &str, image: &Path) {
        self.0.execute(&format!(
            "{{\"execute\":\"blockdev-add\",\"arguments\":{{\"node-name\":\"{node}\",\
             \"driver\":\"raw\",\"file\":{{\"driver\":\"file\",\"filename\":\"{}\"}}}}}}",
            image.display()
        ));
    }
}

/// The argv `options` would launch QEMU with, built against placeholder
/// paths. A profile's claim about which devices exist is a claim about this
/// list and nothing else — no screendump can see a device that is present but
/// unused — so this is what a profile assertion has to read.
pub fn profile_argv(options: &BootOptions) -> Vec<String> {
    let p = Path::new("/nonexistent");
    let usb: Vec<PathBuf> = options.profile.usb_disks().iter().map(|_| p.to_path_buf()).collect();
    qemu_command(p, p, &usb, p, p, None, options)
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn qemu_command(
    boot_image: &Path,
    nvme_image: &Path,
    usb_images: &[PathBuf],
    audio_wav: &Path,
    uart_log: &Path,
    qmp_socket: Option<&Path>,
    options: &BootOptions,
) -> Command {
    let shape = options.profile.shape();
    assert!(
        !options.mute || !shape.virtio.present(),
        "mute removes the only console a virtio profile has"
    );

    let repo = compile::repo_root();
    let ovmf_dir = repo.join("ovmf");

    let mut qemu = Command::new("qemu-system-x86_64");

    let kvm = toyos_build::kvm_usable();
    if kvm {
        qemu.arg("-accel").arg("kvm");
    }

    // Without this QEMU runs its default-device pass whenever no network
    // option is given, which is exactly and only the Metal profile: measured
    // on QEMU 11.0.2, an e1000e at 00:02.0 with a slirp backend, an empty
    // ide-cd on the ich9-ahci, and an isa-parallel — none of them declared by
    // anything, none of them visible to an argv assertion, and the first of
    // them enough to make netd claim a NIC on the machine whose whole point is
    // that it has none. `-net none` and `-nic none` are gone in QEMU 11; this
    // is the option that does it, and it leaves i8042/ps2-kbd/ps2-mouse alone.
    qemu.arg("-nodefaults");

    // `kernel-irqchip=split` only when there is a unit: interrupt remapping
    // needs the userspace half of the irqchip, and a machine with no unit has
    // no reason to be built differently from the one it has always been.
    let mut machine = String::from("q35");
    if !options.i8042 {
        machine.push_str(",i8042=off");
    }
    if shape.iommu.is_some() {
        machine.push_str(",kernel-irqchip=split");
    }

    if let Some(base) = options.rtc_base {
        qemu.arg("-rtc").arg(format!("base={base}"));
    }

    qemu.arg("-machine")
        .arg(&machine)
        .arg("-cpu")
        .arg(if kvm { "host,+rdrand,+smap,+fsgsbase,+x2apic,+smep" } else { "qemu64,+rdrand,+smap,+fsgsbase,+x2apic,+smep" })
        .arg("-smp")
        .arg(options.smp.to_string())
        .arg("-m")
        .arg("4G")
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=0,file={},readonly=on",
            ovmf_dir.join("OVMF_CODE-pure-efi.fd").display()
        ))
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=1,file={},readonly=on",
            ovmf_dir.join("OVMF_VARS-pure-efi.fd").display()
        ))
        .arg("-drive")
        .arg(format!(
            "if=none,id=stick,format=raw,file={}",
            boot_image.display()
        ));

    // Ahead of every other `-device`: QEMU gives a PCI function the bypassing
    // address space unless the unit exists when the function is created, so a
    // unit emitted after the devices it is meant to decode is a unit that
    // decodes nothing — the vacuity trap, in its harness-side form.
    if let Some(unit) = shape.iommu {
        qemu.arg("-device").arg(format!(
            "intel-iommu,intremap={},caching-mode=on,aw-bits={}",
            if unit.intremap { "on" } else { "off" },
            unit.aw_bits
        ));
    }

    for controller in shape.xhci {
        qemu.arg("-device").arg(*controller);
    }

    // The data disks' own arguments, emitted either side of the boot stick's
    // `-device`. QEMU hands out ports in the order devices are created, so this
    // is the only thing that decides which disk the guest enumerates first.
    // Each carries a device id as well as a drive id, because a test that
    // unplugs one over QMP has to be able to name it.
    let data_sticks: Vec<Vec<String>> = shape
        .usb_disks
        .iter()
        .enumerate()
        .map(|(i, disk)| {
            vec![
                "-drive".to_string(),
                format!(
                    "if=none,id={},format=raw,file={}{}",
                    usb_drive_id(i),
                    usb_images[i].display(),
                    if disk.readonly { ",readonly=on" } else { "" }
                ),
                "-device".to_string(),
                format!(
                    "usb-storage,bus={1},drive={2},id={3},logical_block_size={0},\
                     physical_block_size={0}",
                    disk.lba_bytes,
                    shape.storage_bus,
                    usb_drive_id(i),
                    usb_device_id(i),
                ),
            ]
        })
        .collect();
    for (disk, args) in shape.usb_disks.iter().zip(&data_sticks) {
        if disk.before_boot_stick {
            qemu.args(args);
        }
    }

    qemu.arg("-device")
        .arg(format!(
            "usb-storage,bus={},drive=stick,id={BOOT_STICK_ID},bootindex=0",
            shape.storage_bus
        ))
        .arg("-vga")
        .arg(shape.vga)
        .arg("-display")
        .arg("none")
        .arg("-no-reboot");
    if let Some(mb) = shape.vgamem_mb {
        qemu.arg("-global").arg(format!("VGA.vgamem_mb={mb}"));
    }

    // Controller and namespace as two devices rather than QEMU's implicit
    // one, so the logical block size is something a profile states instead of
    // something the default decides — and so that stating *zero* bytes gives
    // the guest no controller at all, rather than an empty one. A machine
    // with no NVMe is a shape, and the argv is the only place it is visible:
    // no console line and no screendump can see a device that is absent.
    if shape.nvme_bytes != 0 {
        qemu.arg("-drive")
            .arg(format!(
                "if=none,id=nvme0,format=raw,file={}",
                nvme_image.display()
            ))
            .arg("-device")
            .arg("nvme,serial=deadbeef,id=nvme0ctl")
            .arg("-device")
            .arg(format!(
                "nvme-ns,drive=nvme0,bus=nvme0ctl,logical_block_size={0},physical_block_size={0}",
                shape.nvme_lba_bytes
            ));
    }

    // The mass-storage devices beside the boot stick, and the only ones a test
    // may write to: the boot stick is on the same bus and carries the image the
    // guest is running from. Their logical block sizes are stated rather than
    // left to the default for the same reason the namespace's is.
    for (disk, args) in shape.usb_disks.iter().zip(&data_sticks) {
        if !disk.before_boot_stick {
            qemu.args(args);
        }
    }

    for dev in shape.usb {
        qemu.arg("-device").arg(*dev);
    }

    if !shape.hda.is_empty() {
        // The same wav backend virtio-sound gets, so gate A's ground truth —
        // what the *device* received — transfers with no new instrument. A boot
        // that plays nothing leaves an empty file and costs nothing.
        //
        // **`timer-period` is 1000 µs here and 5000 for virtio-sound, and that
        // is an instrument repair rather than a difference in the audio path.**
        // At 5000 the capture of a 3 s 440 Hz tone comes back with eight phase
        // discontinuities, at frames 2703-2705, 2821-2823 and 2939-2940 —
        // *identical positions across six runs whose audio content differed*,
        // which is a capture that drops samples on a fixed cadence and not a
        // guest that plays them wrong. QEMU's `hda-codec` holds its own output
        // ring and discards what overruns it, and shortening the host's drain
        // interval is what stops the overrun. Measured on this host, QEMU
        // 11.0.3: 8 breaks at 5000, 0 at 1000, with the guest's own counters
        // (1127 periods submitted, no underruns, no drains) identical either
        // way and identical to the virtio arm's.
        qemu.arg("-audiodev").arg(format!(
            "wav,id=hdaaud,path={},timer-period=1000",
            audio_wav.display()
        ));
        for dev in shape.hda {
            qemu.arg("-device").arg(*dev);
        }
    }

    if shape.virtio.present() {
        qemu.arg("-netdev")
            .arg("user,id=net0")
            .arg("-device")
            .arg(match shape.virtio {
                Virtio::NicWithoutMsix => "virtio-net-pci-non-transitional,netdev=net0,vectors=0",
                _ => "virtio-net-pci-non-transitional,netdev=net0",
            });
        if shape.virtio.sound() {
            // virtio-sound records everything the guest plays into a per-boot
            // wav for glitch analysis; timer-period matches the interactive
            // config in src/qemu.rs so test timing represents what users hear.
            qemu.arg("-audiodev")
                .arg(format!(
                    "wav,id=audio0,path={},timer-period=5000",
                    audio_wav.display()
                ))
                .arg("-device")
                .arg("virtio-sound-pci,audiodev=audio0,streams=1");
        }
        qemu
            // virtio-console on stdio is the primary I/O channel; UART goes to
            // a temp file so early-boot logs and panic fallback still land
            // somewhere when the kernel switches backends.
            .arg("-serial")
            .arg(format!("file:{}", uart_log.display()))
            .arg("-chardev")
            .arg("stdio,id=cs0,signal=off")
            .arg("-device")
            .arg("virtio-serial-pci-non-transitional,id=virtio-serial0,max_ports=1")
            .arg("-device")
            .arg("virtconsole,chardev=cs0,id=console0");
    } else if options.mute {
        qemu.arg("-serial").arg("none");
    } else {
        // The 16550 *is* the console here: no virtio-serial exists, so the
        // kernel's log ring drains to it and the guest reads its commands
        // off it. signal=off matches the virtio console above, so a ^C in
        // the stream reaches the guest rather than killing QEMU.
        qemu.arg("-chardev")
            .arg("stdio,id=uart0,signal=off")
            .arg("-serial")
            .arg("chardev:uart0");
    }

    if options.gdb_stub {
        qemu.arg("-s");
    }
    if let Some(socket) = qmp_socket {
        qemu.arg("-qmp")
            .arg(format!("unix:{},server,nowait", socket.display()));
    }

    qemu
}

/// Every file one boot owns, so that adding another does not lengthen a
/// parameter list eight paths long.
struct Files {
    seq: u32,
    audio_wav: PathBuf,
    uart_log: PathBuf,
    nvme: NvmeClaim,
    usb_images: Vec<PathBuf>,
    qmp_socket: Option<PathBuf>,
    screendump: PathBuf,
    own_boot_image: Option<PathBuf>,
}

fn spawn_and_wait_ready(mut qemu: Command, options: &BootOptions, files: Files) -> QemuInstance {
    let Files {
        seq,
        audio_wav,
        uart_log,
        nvme,
        usb_images,
        qmp_socket,
        screendump,
        own_boot_image,
    } = files;

    qemu.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("[qemu {seq}] Launching QEMU...");
    }
    let mut child = qemu.spawn().expect("Failed to launch QEMU");

    let stdin = BufWriter::new(child.stdin.take().unwrap());
    let stdout = child.stdout.take().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let reader_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut full_log = String::new();
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    full_log.push_str(&line);
                    full_log.push('\n');
                    // Here rather than in a caller's capture, because no caller
                    // holds every line: `boot_log` ends at the ready marker and
                    // a `TestResult` begins at `===TEST_START===`. The census is
                    // cumulative, so what the suite's summary wants is the last
                    // one of the boot, whichever of those windows it fell in.
                    super::irqcensus::observe(seq, &line);
                    if VERBOSE.load(Ordering::Relaxed) {
                        // The boot's own number, because `--nocapture` on a
                        // wide run is several guests talking into one terminal
                        // and an unattributed line is worse than no line.
                        eprintln!("[serial {seq}] {line}");
                    }
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        full_log
    });

    // A muted guest has no console at all, so there is no marker to wait for:
    // the caller polls the framebuffer. Blocking here would only time out.
    let boot_log = if options.mute {
        String::new()
    } else {
        wait_for_ready(&mut child, &rx, options, &uart_log)
    };

    // Counted from here rather than from the spawn: every panic inside
    // `wait_for_ready` kills the child on its way out and never builds a value
    // to drop, so a guest that failed to come up must not be left on the books.
    LIVE.fetch_add(1, Ordering::SeqCst);
    QemuInstance {
        child,
        stdin,
        rx,
        _reader_thread: reader_thread,
        audio_wav,
        uart_log,
        nvme,
        usb_images,
        qmp_socket,
        screendump,
        own_boot_image,
        boot_log,
    }
}

/// Returns every line seen on the way to the marker — see [`QemuInstance::boot_log`].
fn wait_for_ready(
    child: &mut Child,
    rx: &Receiver<String>,
    options: &BootOptions,
    uart_log: &Path,
) -> String {
    let no_timeout = options.debug_wait;
    let ready = options.ready_marker;
    let panic_aborts = ready == DEFAULT_READY;
    // Ten seconds per guest this phase may have up, and never fewer than two
    // guests' worth — the tree runs 15-25 suites a day across several agents,
    // so one guest on a quiet host stopped being
    // the regime some time before this did. Measured on 2026-08-03 with other
    // agents building: two boots exceeded the flat ten seconds, one of them in a
    // phase running a single guest.
    //
    // A wedge costs that much longer to report and nothing else. No test asserts
    // on how long a boot took by *this* clock: `i8042_absent` and
    // `xhci_slow_connect` do assert on boot timing and read the guest's own
    // stamps, and both are in the serial tail.
    //
    // Scaled by the host too, and the first boot of a run is the one that
    // cannot be: nothing has been measured yet, so it gets the flat number and
    // every boot after it gets the corrected one. Two boot timeouts in CI run
    // `31233476555` were this — `console: ready` and `compositor: ready`, on a
    // runner where the same boots take twice what they take here.
    let (num, den) = host_scale();
    let boot_timeout = Duration::from_secs(10) * WIDTH.load(Ordering::SeqCst).max(2) * num / den;
    let start = Instant::now();
    let mut seen = String::new();
    loop {
        if !no_timeout && start.elapsed() > boot_timeout {
            let _ = child.kill();
            // With what it did say. A timeout that discards the console is the
            // one failure in this harness that arrives with no evidence at all,
            // and "the guest printed nothing" and "the guest printed sixty
            // lines and then stopped" are different machines.
            panic!(
                "[qemu] Boot timed out waiting for {ready}; the console carried:\n{}",
                if seen.is_empty() { "nothing at all".to_string() } else { seen.clone() }
            );
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(line) if line.contains(ready) => {
                seen.push_str(&line);
                seen.push('\n');
                if VERBOSE.load(Ordering::Relaxed) {
                    eprintln!("[qemu] Reached {ready}");
                }
                break;
            }
            // **A death nothing left on this machine can come back from.** The
            // kernel's own, or a process the kernel killed — before the ready
            // marker the second is as fatal as the first, because whatever died
            // was `init` or one of its children and nothing else is going to
            // reach the marker.
            //
            // A process that ended *itself* is not on that list, and the
            // difference is not academic: `sshd` panics across boots that then
            // come up perfectly
            // (`issues/build/sshd-panics-when-netd-exits-before-it-binds.md`).
            // The words are the same words — `panicked at` — and who wrote the
            // line is the whole of what tells them apart. `super::serial::died`
            // is where that is decided, for this wait and for [`await_guest`]
            // and [`QemuInstance::run_test_paced`] alike, so the three cannot
            // drift into disagreeing about a spelling.
            Ok(ref line)
                if panic_aborts
                    && !no_timeout
                    && matches!(
                        super::serial::died(line),
                        Some(super::serial::Died::Kernel | super::serial::Died::Faulted)
                    ) =>
            {
                let mut crash_msg = line.clone();
                let drain_deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < drain_deadline {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(bt_line) => {
                            crash_msg.push('\n');
                            crash_msg.push_str(&bt_line);
                        }
                        Err(_) => break,
                    }
                }
                let _ = child.kill();
                panic!("[qemu] Init process crashed during boot:\n{crash_msg}");
            }
            Ok(line) => {
                seen.push_str(&line);
                seen.push('\n');
                continue;
            }
            // A guest that dies before virtio-console init never reaches
            // stdio at all; the UART file is the only channel it has.
            Err(RecvTimeoutError::Timeout) => {
                if !panic_aborts
                    && fs::read_to_string(uart_log).is_ok_and(|s| s.contains(ready))
                {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                let status = child.wait();
                panic!("[qemu] QEMU died before {ready} (status: {status:?})");
            }
        }
    }
    record_boot(start.elapsed());
    seen
}
