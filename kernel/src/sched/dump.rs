//! Ctrl+Alt+D: what every CPU is holding, and what nothing is holding at all.
//!
//! The one instrument for a machine that has stopped without panicking. It
//! answers three questions that look identical from outside and have different
//! causes — a thread parked on a deadline that never fired, a thread parked on
//! a deadline nobody could ever reach, and a thread no CPU has at all — and it
//! is designed to be read off a photograph of a panel, so the verdict is the
//! last thing printed and the report takes the screen.
//!
//! **No CPU may read another's scheduler state.** `CpuSched` is `!Sync` by
//! design, so this is a request rather than a walk: the asking CPU marks every
//! sibling, kicks it, and each one prints its own tasks from `drain_irqs` at
//! the top of its next pass. A CPU that does not reach a pass inside the
//! budget is named, and *that is a finding* — it is the only way this report
//! can say "cpu 3 is not scheduling at all".
//!
//! Nothing here allocates, nothing waits on a lock it could find held, and
//! every list is bounded. See `issues/diagnostics/` for what it was built
//! to settle.
//!
//! **This report cannot describe the state it is summoned to describe, and the
//! deadline columns are where that bites.** Asking is a keystroke, a keystroke
//! is an interrupt, and an interrupt is exactly what a halted CPU was waiting
//! for — so by the time any CPU prints a line it has already taken a pass,
//! re-armed its timer and fired whatever was due. A machine frozen on an
//! unfired deadline therefore reports `0 OVERDUE`: not because its deadlines
//! were healthy, but because summoning the report repaired them. Everything
//! under `== deadlines:` postdates the repair.
//!
//! What survives is identity and place — which threads exist, which CPU holds
//! each, which never ran, which CPUs did not answer — because waking a CPU does
//! not move a task between containers. To learn what the *frozen* machine
//! looked like, capture it before touching it: `info registers -a` over QMP
//! gives every vCPU's `RIP` and `HLT` with nothing woken (`CLAUDE.md`,
//! Debugging). That capture settled #156 and this report's deadline columns
//! would have said the opposite.
//!
//! It is also what the NMI probe below buys and a kick does not: an answer that
//! does not require the CPU to schedule in order to give it.
//!
//! **The panel is the deliverable, and it is bracketed and held.** `request`
//! marks the log before its first line and after its last, so what the console
//! paints is this report rather than the newest screenful of a ring every
//! process writes into — and `panic_console::hold_report` puts it back for as
//! long as the hold lasts, because a compositor that is still composing does
//! not know the kernel drew and will overwrite it inside a frame.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::{apic, percpu, smp};
use crate::sched::payload::{SCHED_BLOCKED, SCHED_READY, SCHED_RUNNING};
use crate::time::{Budget, Duration, Floor};

use super::driver;
use super::MAX_CPUS;

/// A deadline further out than this is not a wait, it is arithmetic that got
/// away: no kernel site parks for an hour, and a `saturating_add` that
/// overflowed lands at `u64::MAX` nanoseconds, which is 584 years.
///
/// A [`Floor`] rather than any of the waiting kinds: nothing waits for it and
/// nothing expires. It is a predicate *on* another duration, which is the one
/// thing that kind is for.
const ABSURD_HORIZON: Floor = Floor::policy(
    Duration::from_secs(3_600),
    "no kernel site parks for an hour, and an overflowed saturating_add lands 584 years out",
);

/// How long the asking CPU waits for its siblings before naming the silent
/// ones. It spends this with preemption off, which is what any bounded wait in
/// `drain_irqs` costs; a quarter second is far past a scheduler pass and far
/// short of anything a person notices after pressing a key.
const ANSWER_BUDGET: Budget = Budget::of(
    Duration::from_millis(250),
    "the silent CPUs are named and their part of the report is missing",
);

/// Ordinary parked lines one CPU may print. A line the verdict depends on —
/// overdue, absurd — is never counted against this, so truncation cannot hide
/// the thing being looked for.
const LINES_PER_CPU: u32 = 16;

/// Census lines, which carry only the threads the parked lists do not.
const CENSUS_LINES: u32 = 16;

/// How long the census retries the process table. Whoever holds it in the
/// ordinary case — a spawn, an exit — is finished inside microseconds, and
/// giving up on the first refusal costs the owner the half of the report that
/// names a thread no CPU has. Whoever holds it in the case this facility is
/// for is not going to finish, which is what the ceiling is for.
const TABLE_BUDGET: Budget = Budget::of(
    Duration::from_millis(20),
    "the summary says the census is missing rather than naming the threads no CPU has",
);

/// How long a silent CPU gets to answer the NMI. Two orders of magnitude below
/// the kick's budget because an NMI needs nothing of the target but the
/// interrupt itself: no pass, no lock, no scheduler state. A CPU that has not
/// answered in a millisecond is not going to.
const NMI_BUDGET: Budget = Budget::of(
    Duration::from_millis(1),
    "the CPU is reported silent with no instruction pointer beside it",
);

static IN_PROGRESS: AtomicBool = AtomicBool::new(false);
static OWES: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// The NMI handshake, in two arrays because the handler may not allocate, may
/// not log and may not take a lock (`arch/idt/nmi.rs`): it stores and clears,
/// and the CPU that asked does the rest.
static NMI_OWES: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static NMI_RIP: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// What the CPUs report, summed as each one answers. Reset by `request`
/// before any CPU is asked.
mod tally {
    use core::sync::atomic::AtomicU32;

    pub static PARKED: AtomicU32 = AtomicU32::new(0);
    pub static READY: AtomicU32 = AtomicU32::new(0);
    /// Killed threads unwinding, or waiting on a CPU to unwind. **A container
    /// of its own**, because their state words read `Ready` and the census
    /// counts them as such, so a verdict built without this one reports them as
    /// held by nobody.
    pub static DYING: AtomicU32 = AtomicU32::new(0);
    pub static RUNNING: AtomicU32 = AtomicU32::new(0);
    pub static NO_DEADLINE: AtomicU32 = AtomicU32::new(0);
    pub static PENDING: AtomicU32 = AtomicU32::new(0);
    pub static OVERDUE: AtomicU32 = AtomicU32::new(0);
    pub static ABSURD: AtomicU32 = AtomicU32::new(0);
    pub static UNPRINTED: AtomicU32 = AtomicU32::new(0);

    pub const ALL: [&AtomicU32; 9] = [
        &PARKED,
        &READY,
        &DYING,
        &RUNNING,
        &NO_DEADLINE,
        &PENDING,
        &OVERDUE,
        &ABSURD,
        &UNPRINTED,
    ];
}

/// What a parked task's deadline says about it. The three-way split this whole
/// facility exists to make legible.
#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    /// No deadline: only an event ends this wait, which is what a server does.
    Event,
    /// A deadline still in the future.
    Pending,
    /// A deadline that has passed. The timer that should have ended this wait
    /// did not fire.
    Overdue,
    /// A deadline no wait could have meant.
    Absurd,
}

impl Verdict {
    fn of(deadline: Option<u64>, now: u64) -> Self {
        match deadline {
            None => Self::Event,
            Some(at) if at <= now => Self::Overdue,
            Some(at) if at - now > ABSURD_HORIZON.nanos() => Self::Absurd,
            Some(_) => Self::Pending,
        }
    }

    /// Whether this line must survive truncation.
    fn is_anomaly(self) -> bool {
        matches!(self, Self::Overdue | Self::Absurd)
    }

    fn count(self) {
        match self {
            Self::Event => &tally::NO_DEADLINE,
            Self::Pending => &tally::PENDING,
            Self::Overdue => &tally::OVERDUE,
            Self::Absurd => &tally::ABSURD,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
}

fn online_cpus() -> usize {
    (smp::cpu_count() as usize).min(MAX_CPUS)
}

/// Ctrl+Alt+D. Called from `drain_irqs` on whichever CPU decoded the key, and
/// from nowhere else.
pub fn request() {
    // `pass` owns the one level; a `Lock` guard would add another. The whole
    // design below — `try_lock`, bounded waits, no allocation — assumes this
    // runs holding nothing, and this is what keeps that true as `drain_irqs`
    // grows.
    let depth = crate::preempt::count();
    assert!(depth <= 1, "the blocked-task dump ran under a lock: preempt depth {depth}");
    if IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }
    for counter in tally::ALL {
        counter.store(0, Ordering::Relaxed);
    }

    let cpus = online_cpus();
    let me = percpu::cpu_id() as usize;
    // The bracket is two instants rather than two byte positions in one stream,
    // because there is no one stream any more: a byte position has no meaning
    // across shards. It is also exact where a byte range was not — the dump's
    // own records are stamped by this same clock, so nothing a sibling CPU logs
    // meanwhile can widen it.
    let from = crate::clock::nanos_since_boot();
    log!("=== blocked-task dump: {cpus} cpu(s), and this report takes the screen ===");

    // A CPU number, not a walk of `OWES`: it is compared against `me`, and
    // `OWES` is `MAX_CPUS` long whatever `cpus` is.
    #[allow(clippy::needless_range_loop)]
    for cpu in 0..cpus {
        if cpu != me {
            OWES[cpu].store(true, Ordering::Release);
        }
    }
    // Kick after every flag is set, so a CPU that answers instantly cannot
    // find its own flag still unwritten and go back to sleep.
    for cpu in 0..cpus {
        if cpu != me {
            apic::kick_cpu(cpu as u32);
        }
    }

    report_this_cpu();

    // A pass reached by preemption cannot be holding a `Lock` — taking one
    // raises the preempt count — so spinning here cannot be blocking a sibling
    // on a lock this CPU's interrupted context owns.
    let deadline = crate::clock::nanos_since_boot().saturating_add(ANSWER_BUDGET.nanos());
    let mut silent = 0;
    loop {
        if (0..cpus).all(|cpu| !OWES[cpu].load(Ordering::Acquire)) {
            break;
        }
        if crate::clock::nanos_since_boot() >= deadline {
            let mut asked = [false; MAX_CPUS];
            for cpu in 0..cpus {
                if OWES[cpu].swap(false, Ordering::AcqRel) {
                    silent += 1;
                    asked[cpu] = true;
                    log!("  cpu{cpu} !! no answer: it did not reach a scheduler pass");
                }
            }
            probe_silent(&asked, cpus);
            break;
        }
        core::hint::spin_loop();
    }

    let census = census();
    summary(cpus, silent, census);
    crate::drivers::panic_console::paint_report(from, crate::clock::nanos_since_boot());
    IN_PROGRESS.store(false, Ordering::Release);
}

/// Ask each CPU that ignored its kick where it is, with the one interrupt it
/// cannot mask.
///
/// A kick that goes unanswered has three causes and the report cannot act on
/// any of them, because they look identical from here: the CPU is spinning with
/// `IF` clear, it is halted and its kick was never delivered, or it is wedged
/// below the interrupt layer entirely. An NMI separates all three in one round
/// — a `rip` in a spin loop, a `rip` at the `hlt`, or no answer at all — and
/// that is the whole reason this exists.
///
/// Bounded and lock-free on both sides. The handler stores one word; this
/// symbolizes it afterwards, from a context that may take the ring lock.
fn probe_silent(asked: &[bool; MAX_CPUS], cpus: usize) {
    let any = (0..cpus).any(|cpu| asked[cpu]);
    if !any {
        return;
    }
    for cpu in 0..cpus {
        if asked[cpu] {
            NMI_RIP[cpu].store(0, Ordering::Relaxed);
            NMI_OWES[cpu].store(true, Ordering::Release);
        }
    }
    // Every flag set before any NMI goes out, for the same reason the kicks are
    // batched above: an instant answer must not find its own flag unwritten.
    // A CPU number, not a walk of `asked`: it is the APIC id the NMI goes to.
    #[allow(clippy::needless_range_loop)]
    for cpu in 0..cpus {
        if asked[cpu] {
            apic::send_nmi(cpu as u32);
        }
    }

    let deadline = crate::clock::nanos_since_boot().saturating_add(NMI_BUDGET.nanos());
    while (0..cpus).any(|cpu| asked[cpu] && NMI_OWES[cpu].load(Ordering::Acquire)) {
        if crate::clock::nanos_since_boot() >= deadline {
            break;
        }
        core::hint::spin_loop();
    }

    for cpu in 0..cpus {
        if !asked[cpu] {
            continue;
        }
        if NMI_OWES[cpu].swap(false, Ordering::AcqRel) {
            log!("  cpu{cpu} !! no NMI answer either: wedged below the interrupt layer");
        } else {
            log!("  cpu{cpu} NMI answered, it is here:");
            crate::symbols::resolve_kernel(NMI_RIP[cpu].load(Ordering::Acquire));
        }
    }
}

/// Stage the one machine state this report exists to describe and that QEMU
/// cannot produce: a CPU that ignores a kick.
///
/// Nothing on the host side can make a guest CPU deaf. QEMU delivers every IPI
/// it is given, and a guest that stops scheduling stops for reasons — a spin
/// with `IF` clear, a lock nobody releases — that are properties of the code
/// under test rather than of the machine, so there is no `-device` and no
/// monitor command that stages one. A kernel feature is the only actuator, and
/// it replaces the *state* rather than the verdict: the victim really does
/// disable interrupts and really does spin, so the kick really is unanswered
/// and the NMI really is what reaches it.
///
/// Bounded and self-healing on purpose. The window is longer than
/// [`ANSWER_BUDGET`] so the CPU is named silent, and short enough that it
/// rejoins and the guest shuts down cleanly — which is itself part of the
/// assertion, since a CPU the NMI merely interrupted must come back.
#[cfg(feature = "boot-actuators")]
pub(super) fn deaf_window() {
    /// Late enough that the machine is up and every CPU has joined.
    const ARM_AT_NS: u64 = 3_000_000_000;
    /// Comfortably past [`ANSWER_BUDGET`], so "silent" is not a race — and
    /// bounded, so the CPU rejoins and the guest still shuts down.
    const DEAF_NS: u64 = 400_000_000;
    /// How long cpu0 waits for the victim to reach its idle loop and go deaf.
    ///
    /// **A kicked CPU does not arrive at the top of its loop promptly and there
    /// is no bound to give.** The measurement below was taken when the pass it
    /// was finishing ran `flush_log_file_if_affordable` with preemption off, on
    /// a machine whose `/log` is a USB device — a string of bulk transfers.
    /// **That statement is deleted at log architecture L6** and the conclusion
    /// is not: `drain_irqs` still reaches USB enumeration from a pass, so a
    /// kicked CPU still has no bound, and this is now the record of how large
    /// "no bound" was measured to be rather than of its only cause. CI run
    /// `31284962381` measured 251 ms of it — cpu0 gave up at 11.417 s and the
    /// victim went deaf at 11.568 s, so nothing was staged, no dump ran, and a
    /// CPU sat deaf for 400 ms for nobody. So this is generous and, more to the
    /// point, no longer the whole answer: expiring it leaves the machine as it
    /// found it and lets the next pass ask again.
    const ACK_BUDGET_NS: u64 = 1_000_000_000;

    const IDLE: u64 = 0;
    const ASKED: u64 = 1;
    const DEAF: u64 = 2;

    static STAGE: AtomicU64 = AtomicU64::new(IDLE);
    static FIRED: AtomicBool = AtomicBool::new(false);

    let cpus = online_cpus();
    if cpus < 2 {
        return;
    }
    let me = percpu::cpu_id() as usize;
    let victim = cpus - 1;

    if me == victim {
        if STAGE
            .compare_exchange(ASKED, DEAF, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let began = crate::clock::nanos_since_boot();
            // The counter and not the nanoseconds, because half of what this
            // actuator stages is *where* the CPU is: `nanos_since_boot` divides
            // 128 bits, which is a call into `compiler_builtins`, and a probe
            // that samples the rip then names `u128_div_rem` for a CPU that
            // never left this loop. `rdtsc` and a 64-bit compare inline.
            let until = crate::clock::tsc_deadline(DEAF_NS);
            // The actuator's whole content. Interrupts come back on below and
            // the loop is bounded by the clock. Not an `IrqGuard`, for the
            // reason `driver::execute`'s idle arm gives: this must *set* IF on
            // the way out, and a CPU that reached the idle loop through panic
            // recovery has IF already 0 for a guard to restore.
            crate::arch::cpu::disable_interrupts();
            while crate::arch::cpu::rdtsc() < until {
                core::hint::spin_loop();
            }
            crate::arch::cpu::enable_interrupts();
            // The victim is the only thing that can witness its own return, and
            // half of what the probe claims is that an NMI interrupts a CPU
            // rather than killing it.
            let deaf_ms = (crate::clock::nanos_since_boot() - began) / 1_000_000;
            log!("dump-deaf-cpu: cpu{me} rejoined after {deaf_ms}ms deaf");
        }
        return;
    }
    if me != 0 || crate::clock::nanos_since_boot() < ARM_AT_NS {
        return;
    }
    if FIRED.swap(true, Ordering::AcqRel) {
        return;
    }
    // Drive the whole sequence from here rather than across idle-loop
    // iterations: cpu0 may halt between two of them, and the window it has to
    // ask inside is only as long as the victim stays deaf.
    STAGE.store(ASKED, Ordering::Release);
    apic::kick_cpu(victim as u32);
    let deadline = crate::clock::nanos_since_boot().saturating_add(ACK_BUDGET_NS);
    while STAGE.load(Ordering::Acquire) != DEAF {
        if crate::clock::nanos_since_boot() >= deadline {
            // Take the ask back, and only then give up. The CAS is what makes
            // the give-up safe rather than a second race: if it fails the
            // victim has just gone deaf and the window is open after all, so
            // this asks for the report instead of leaving a deaf CPU nobody
            // looked at.
            if STAGE
                .compare_exchange(ASKED, IDLE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                log!("dump-deaf-cpu: cpu{victim} did not reach its idle loop in time; asking again");
                FIRED.store(false, Ordering::Release);
                return;
            }
            break;
        }
        core::hint::spin_loop();
    }
    request();
}

/// The NMI handler's whole contribution: where this CPU was. Called from
/// `arch/idt/nmi.rs` and from nowhere else.
///
/// Stores unconditionally rather than only when owed. An NMI this kernel did
/// not send is a fact worth keeping too, and the alternative — reading the flag
/// first — is a branch on state the sender owns, from a context that cannot
/// afford to be wrong about it.
pub fn note_nmi(rip: u64) {
    let me = percpu::cpu_id() as usize;
    if me >= MAX_CPUS {
        return;
    }
    NMI_RIP[me].store(rip, Ordering::Release);
    NMI_OWES[me].store(false, Ordering::Release);
}

/// Print this CPU's own tasks if it was asked to. Called from `drain_irqs` on
/// every CPU, every pass.
pub fn serve_if_owed() {
    let me = percpu::cpu_id() as usize;
    if me >= MAX_CPUS || !OWES[me].load(Ordering::Acquire) {
        return;
    }
    report_this_cpu();
    OWES[me].store(false, Ordering::Release);
}

fn report_this_cpu() {
    let cpu = percpu::cpu_id();
    let now = crate::clock::nanos_since_boot();

    match driver::running_id() {
        Some(id) => {
            tally::RUNNING.fetch_add(1, Ordering::Relaxed);
            log!("  cpu{cpu} running pid={} tid={}", id.0.raw(), id.1.raw());
        }
        None => log!("  cpu{cpu} running nothing"),
    }
    let ready = driver::ready_len() as u32;
    tally::READY.fetch_add(ready, Ordering::Relaxed);

    // **The dying list, which no reader of this dump could see.** A killed
    // thread's word says `Ready(cpu)`, so the census below counts it — while
    // `ready_len()` counts only `rq` and `for_each_parked` walks only `parked`,
    // which made every teardown in flight an `unheld` false positive. The whole
    // point of the verdict is to tell "a task nothing will ever run" from a
    // busy machine, and this is a task that runs *next*.
    let mut dying = 0u32;
    let read_dying = driver::for_each_dying(|id| {
        dying += 1;
        tally::DYING.fetch_add(1, Ordering::Relaxed);
        log!("  cpu{cpu} pid={} tid={} unwinding (killed)", id.0.raw(), id.1.raw());
    });
    if !read_dying {
        log!("  cpu{cpu} !! a pass owns its scheduler state; nothing read from it");
    }

    let mut ordinary = 0u32;
    let read = driver::for_each_parked(|task| {
        tally::PARKED.fetch_add(1, Ordering::Relaxed);
        let verdict = Verdict::of(task.deadline, now);
        verdict.count();
        if !verdict.is_anomaly() {
            ordinary += 1;
            if ordinary > LINES_PER_CPU {
                tally::UNPRINTED.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        log!(
            "  cpu{cpu} {}pid={} tid={} {} parked {} {}{}",
            if verdict.is_anomaly() { "!! " } else { "" },
            task.id.0.raw(),
            task.id.1.raw(),
            task.class.name(),
            Ms(now.saturating_sub(task.since)),
            Deadline { verdict, deadline: task.deadline, now },
            if task.rt { " rt" } else { "" },
        );
    });
    if !read {
        log!("  cpu{cpu} !! a pass owns its scheduler state; nothing read from it");
    }
    if ready > 0 {
        log!("  cpu{cpu} {ready} task(s) queued and not running");
    }
}

/// What the census found, for the summary to compare against the CPUs.
#[derive(Default, Clone, Copy)]
struct Census {
    read: bool,
    threads: u32,
    running: u32,
    ready: u32,
    blocked: u32,
    zombie: u32,
    unscheduled: u32,
    never_ran: u32,
}

/// Every thread the process table knows, and one line for each that the CPUs'
/// parked lists do not already carry.
///
/// A thread whose state word says Ready is either about to run or has never
/// run at all, and `cpu_ns == 0` is the difference. That is the whole reason
/// this half exists: a task no CPU ever picked up appears in nobody's parked
/// list, so a report built from the schedulers alone cannot see it.
fn census() -> Census {
    let mut c = Census::default();
    let mut printed = 0u32;
    let deadline = crate::clock::nanos_since_boot().saturating_add(TABLE_BUDGET.nanos());
    c.read = walk_threads(deadline, |thread| {
        c.threads += 1;
        if thread.zombie.is_some() {
            c.zombie += 1;
            return;
        }
        // **A kernel thread is named whatever it is doing, and it is the one
        // exception to the rule below.** `klogd`, `usbd` and `iod` are almost
        // always blocked, so the CPUs' parked lines are where they appear — and
        // those lines carry a pid and a tid and no name. On a machine that has
        // gone quiet the question is *which* of the three is stuck, and a pid is
        // not an answer to it.
        let kernel = crate::sched::kthread::is_kernel_task(crate::scheduler::TaskId(
            thread.pid,
            thread.tid,
        ));
        let (bucket, tag) = match thread.sched {
            Some(SCHED_RUNNING) => (&mut c.running, kernel.then_some("kernel")),
            Some(SCHED_BLOCKED) => (&mut c.blocked, kernel.then_some("kernel")),
            Some(SCHED_READY) if thread.cpu_ns == 0 => {
                c.never_ran += 1;
                (&mut c.ready, Some("!! ready and has never run"))
            }
            Some(SCHED_READY) => (&mut c.ready, Some("ready")),
            _ => (&mut c.unscheduled, Some("!! no scheduler record")),
        };
        *bucket += 1;
        // Blocked and running threads are the CPUs' lines; printing them again
        // would push the ones only this half can see off the page.
        let Some(tag) = tag else { return };
        // The three kernel threads do not count against the budget and cannot
        // flood it: `sched::kthread::MAX_KERNEL_TASKS` is the ceiling, and a
        // shipping kernel's is three. Counting them would let a machine with
        // enough ready threads push the very lines this exception exists for
        // off the page.
        if !kernel {
            printed += 1;
            if printed > CENSUS_LINES {
                return;
            }
        }
        log!(
            "  {tag}: pid={} tid={} {} cpu={}",
            thread.pid.raw(),
            thread.tid.raw(),
            Named { process: thread.process, thread: thread.thread },
            Ms(thread.cpu_ns),
        );
    });
    c
}

/// `process::try_for_each_thread` until `deadline`. Separated so the retry is
/// one loop rather than a condition tangled into the walk.
fn walk_threads(deadline: u64, mut f: impl FnMut(crate::process::ThreadCensus<'_>)) -> bool {
    loop {
        if crate::process::try_for_each_thread(&mut f) {
            return true;
        }
        if crate::clock::nanos_since_boot() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
}

/// The verdict, printed last because it is the only part that needs every CPU
/// to have answered — and because the panel shows the newest page, so last is
/// what a photograph catches.
fn summary(cpus: usize, silent: u32, c: Census) {
    let answered = cpus - silent as usize;
    // Where this machine's interrupts have been landing, before the verdict and
    // after the per-CPU lines. A CPU that answered nothing still reports here:
    // the counters are its own `PerCpu`'s and a sibling reads them, so this part
    // of the report needs nothing of the CPU it describes — which is exactly the
    // question a silent CPU leaves open.
    crate::irq_census::log_census();
    // Every count in the summary is written before the word it counts, so a
    // reader — and the gate — takes the field from the word rather than from a
    // position in the line.
    if !c.read {
        log!("== census: the process table is held; no thread census this dump");
    } else {
        log!(
            "== census: {} thread(s) — {} running, {} ready, {} blocked, {} zombie, {} unscheduled",
            c.threads, c.running, c.ready, c.blocked, c.zombie, c.unscheduled,
        );
    }
    log!(
        "== sched: {answered}/{cpus} cpu(s) answered — {} running, {} queued, {} unwinding, \
         {} parked",
        tally::RUNNING.load(Ordering::Relaxed),
        tally::READY.load(Ordering::Relaxed),
        tally::DYING.load(Ordering::Relaxed),
        tally::PARKED.load(Ordering::Relaxed),
    );
    log!(
        "== deadlines: {} event-only, {} pending, {} OVERDUE, {} ABSURD",
        tally::NO_DEADLINE.load(Ordering::Relaxed),
        tally::PENDING.load(Ordering::Relaxed),
        tally::OVERDUE.load(Ordering::Relaxed),
        tally::ABSURD.load(Ordering::Relaxed),
    );
    // The three-way split, said in one line, and degraded field by field
    // rather than withdrawn: a report that answers two of three questions is
    // worth having, and a photograph of "incomplete" is worth nothing.
    //
    // `unheld` compares what the state words claim against what the CPUs
    // actually hold. A thread the words call blocked or ready that no CPU has
    // is one nothing will ever run, and it is the whole reason the census is
    // here — the schedulers alone cannot see a task none of them was given.
    let overdue = tally::OVERDUE.load(Ordering::Relaxed);
    let absurd = tally::ABSURD.load(Ordering::Relaxed);
    if c.read {
        // `DYING` is in the sum for the same reason `READY` is: the census
        // counted those threads under `ready`, and a container the CPU half
        // cannot see is exactly what `unheld` is meant to name — so leaving it
        // out reports a healthy teardown as a lost task.
        let scheduled = tally::PARKED.load(Ordering::Relaxed)
            + tally::READY.load(Ordering::Relaxed)
            + tally::DYING.load(Ordering::Relaxed)
            + tally::RUNNING.load(Ordering::Relaxed);
        let claimed = c.running + c.ready + c.blocked;
        log!(
            "== VERDICT: {overdue} overdue, {absurd} absurd, {} unheld, {} never ran{}",
            claimed.saturating_sub(scheduled),
            c.never_ran,
            if answered == cpus { "" } else { " (of the cpus that answered)" },
        );
    } else {
        log!(
            "== VERDICT: {overdue} overdue, {absurd} absurd — the process table would not \
             open, so nothing here can say what no cpu holds",
        );
    }
    // The one thread the report cannot ask about by walking a queue, because
    // what it is doing is the reason this report is readable at all. `lost` is
    // the number a reader of the console can never derive: it names the lines
    // that are not there.
    let (drained, lost, parks) = crate::log::console::stats();
    log!("== klogd: {drained} record(s) drained, {lost} lost, {parks} park(s)");
    let unprinted = tally::UNPRINTED.load(Ordering::Relaxed);
    if unprinted > 0 {
        log!("== {unprinted} ordinary parked task(s) not listed; every anomaly is");
    }
    log!("=== end of dump ===");
}

/// A thread by the two names it has. Most threads never take a name of their
/// own, and `soundd/` reads as a truncation rather than as an absence.
struct Named<'a> {
    process: &'a str,
    thread: &'a str,
}

impl core::fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.thread.is_empty() {
            true => write!(f, "{}", self.process),
            false => write!(f, "{}:{}", self.process, self.thread),
        }
    }
}

/// Milliseconds, because every duration in this report is one and a bare
/// nanosecond count is unreadable on a photograph.
struct Ms(u64);

impl core::fmt::Display for Ms {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}ms", self.0 / 1_000_000)
    }
}

struct Deadline {
    verdict: Verdict,
    deadline: Option<u64>,
    now: u64,
}

impl core::fmt::Display for Deadline {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match (self.verdict, self.deadline) {
            (Verdict::Event, _) => write!(f, "no deadline"),
            (Verdict::Pending, Some(at)) => write!(f, "due in {}", Ms(at - self.now)),
            (Verdict::Overdue, Some(at)) => {
                write!(f, "OVERDUE by {}", Ms(self.now.saturating_sub(at)))
            }
            (Verdict::Absurd, Some(at)) => write!(f, "ABSURD, due in {}", Ms(at - self.now)),
            // `Verdict::of` reads the same `Option` this does, so a verdict
            // that names a deadline cannot come with none.
            (_, None) => write!(f, "no deadline"),
        }
    }
}
