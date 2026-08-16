//! Every duration in this kernel is one of a closed set of *kinds*, and the
//! constructor of each demands what justifies it.
//!
//! `specs/completion-architecture-spec.md` §3 is the law and this file is its
//! implementation. A bare `u64` of nanoseconds is not a thing: it says nothing
//! about where the number came from, and — the part that actually costs — it
//! says nothing about what happens when it runs out. Three of the four
//! behaviours a kernel duration can have at expiry are *different failures*
//! (a device that broke, a machine that broke, an answer that got smaller), and
//! a type that cannot tell them apart is a type that lets an implementer pick
//! the wrong one silently.
//!
//! | kind | what it is | where the number comes from | expiry means |
//! |---|---|---|---|
//! | [`Bound`] | the contract that says the thing will happen | a device register, or a cited spec section | the device broke — a named error, never a retry |
//! | [`Cadence`] | how often a thing may be re-done | what makes that rate affordable | nothing; it is a rate |
//! | [`Tripwire`] | a duration whose expiry is a **panic** | how long is absurd | the machine is broken; fail fast |
//! | [`Deadline`] | an absolute [`Instant`] a **caller** chose | userland, or a caller's own arithmetic | the caller's business |
//! | [`Floor`] | a bound on *another* duration | policy, stated between two bounds | nothing; nothing waits |
//! | [`Budget`] | a wall-clock allowance | what the answer costs when it is spent | a degraded answer, named |
//! | [`Delay`] | a duration the caller **spends** | a spec that mandates it, or what is being measured | nothing; the spending is the point |
//!
//! **Seven, where §3 wrote six, and the seventh is this sweep's own finding.**
//! §3.1 added `Floor` and §3.3 added `Budget` the same way: the sweep is what
//! makes the taxonomy total, and a duration that fits nothing is a finding
//! rather than a licence to invent a citation. [`Delay`] is what the six could
//! not hold — a mandatory hardware settle (`CODEC_DETECT_NS`, D3hot recovery,
//! the SDM's INIT/SIPI delays) and a calibration window (`clock::init`,
//! `apic::init_timer`) are durations the CPU *spends*, not durations something
//! is waited for. Nothing expires; there is no error, no panic and no degraded
//! answer, because the elapsing **is** the success path. Classifying them as
//! `Bound`s would have made "expiry means the device broke" false of a third of
//! the `Bound`s in the tree.
//!
//! **RT7, and what it actually buys.** There is no `Bound::from_nanos`, no
//! `Tripwire::from_nanos` and no way to build any kind from a magnitude alone:
//! every constructor takes the justification as a `&'static str` beside the
//! number. A number nobody can cite is a [`Tripwire`] or it does not exist.
//!
//! **What is not a duration**, stated because the sweep had to decide it twice:
//! a spin *count* is not one — `serial.rs`'s `PANIC_LOCK_SPIN_LIMIT` and
//! `THRE_SPIN_LIMIT`, `sync.rs`'s 50M/500M — even where a doc comment prices it
//! in seconds. Neither is an *instant* stored in a static (`FIRST_IRQ_NS`,
//! `NEXT_REPORT_NS`, `LOG_DURABLE_NS`): those are [`Instant`]s the machine
//! remembers, and what a kind classifies is the *interval* somebody chose.
//!
//! This file names nothing outside `core`. That is a requirement rather than a
//! style: `kernel-loom` compiles it a second time so the completion core's
//! records can carry an [`Instant`] into a model, and a dependency on a subject
//! would stop that compiling. `clock::now` is the one bridge to the machine's
//! clock and it lives in `clock.rs`, not here.

use core::fmt;
use core::ops::{Add, Sub};

/// A point on the machine's monotonic clock, in the `nanos_since_boot` domain.
///
/// [`clock::now`](crate::clock::now) is the only thing that mints one from the
/// hardware. `Add<Duration>` and `Sub<Instant>` are the whole of its
/// arithmetic: there is no `Add<Instant>` and no coercion from [`Duration`], so
/// "relative" and "absolute" cannot be confused by accident, which is the one
/// mistake this pair exists to make unrepresentable.
///
/// Both operations saturate. Guest builds run with overflow checks on
/// (`kernel/Cargo.toml`'s `[profile.toyos]`), a deadline computed near
/// `u64::MAX` is a real input from a caller that asked to sleep forever, and a
/// panic inside a wait's arithmetic is a worse answer than a wait that ends at
/// the end of time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Instant(u64);

impl Instant {
    /// The reading `crate::clock::nanos_since_boot` produced. Not `pub` to the
    /// world by accident: it is how `clock.rs` mints one and how a value that
    /// crossed the syscall boundary as a raw `u64` re-enters the type system.
    pub const fn from_nanos_since_boot(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Back to the domain every existing `u64` call site speaks. Named rather
    /// than a field so the direction is deliberate: an `Instant` that becomes
    /// a `u64` has left the type system that separates it from a duration.
    pub const fn nanos_since_boot(self) -> u64 {
        self.0
    }
}

impl Add<Duration> for Instant {
    type Output = Instant;
    fn add(self, rhs: Duration) -> Instant {
        Instant(self.0.saturating_add(rhs.0))
    }
}

impl Sub<Instant> for Instant {
    type Output = Duration;
    fn sub(self, rhs: Instant) -> Duration {
        Duration(self.0.saturating_sub(rhs.0))
    }
}

/// An amount of time. The magnitude a kind carries, and never a kind itself:
/// a `Duration` on its own says nothing about what happens when it runs out,
/// which is exactly why the seven types below exist.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Duration(u64);

impl Duration {
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub const fn from_micros(micros: u64) -> Self {
        Self(micros * 1_000)
    }

    pub const fn from_millis(millis: u64) -> Self {
        Self(millis * 1_000_000)
    }

    pub const fn from_secs(secs: u64) -> Self {
        Self(secs * 1_000_000_000)
    }

    pub const fn nanos(self) -> u64 {
        self.0
    }

    /// For the log lines that already print milliseconds. Truncating, like the
    /// `/ 1_000_000` it replaces.
    pub const fn millis(self) -> u64 {
        self.0 / 1_000_000
    }
}

impl fmt::Display for Duration {
    /// Milliseconds where that reads, nanoseconds where the number is smaller
    /// than one. Nothing in the kernel prints a duration on a hot path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 >= 1_000_000 {
            write!(f, "{}ms", self.millis())
        } else {
            write!(f, "{}ns", self.nanos())
        }
    }
}

/// An absolute instant a caller chose to stop waiting at.
///
/// **Total over the whole `u64` range, with no sentinel branch anywhere.**
/// [`Deadline::never`] is the end of time and [`Deadline::passed`] is the
/// beginning of it; neither is a magic number a call site can collide with,
/// because a `Deadline` cannot be built from a bare integer at all.
///
/// That is the whole reason this is not a `u64` newtype with a public
/// constructor. `scheduler::block_on`'s contract used to be "`deadline = 0`
/// means no timeout", and `io_uring::enter` carried a *third* reading of the
/// same word — relative `0` mapped to absolute `1`, and `1` mapped back to `0`.
/// A site left passing `0` through a change of that convention goes from "block
/// forever" to "return immediately", which is a busy loop and not a compile
/// error, and no test asserts on it. With three named constructors every site
/// says which it meant.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Deadline(u64);

impl Deadline {
    /// Stop at this instant.
    pub const fn at(instant: Instant) -> Self {
        Self(instant.0)
    }

    /// Do not stop. `u64::MAX` nanoseconds is 584 years, and the machine's
    /// clock saturates rather than wrapping, so nothing reaches it.
    pub const fn never() -> Self {
        Self(u64::MAX)
    }

    /// Already over: evaluate once, then answer. The non-blocking shape.
    pub const fn passed() -> Self {
        Self(0)
    }

    /// Whether `now` is at or past it. `never()` is unreachable by
    /// construction and `passed()` is true for every reading.
    pub const fn reached(self, now: Instant) -> bool {
        now.0 >= self.0
    }

    pub const fn is_never(self) -> bool {
        self.0 == u64::MAX
    }

    /// The absolute nanosecond the scheduler's `Nanos` wants. Callers that
    /// must not arm an infinite one ask [`Deadline::is_never`] first.
    pub const fn nanos(self) -> u64 {
        self.0
    }
}

/// The contract that says the thing will happen.
///
/// The number comes from a device register or a cited specification section,
/// and its expiry means the device broke: a named error, never a retry.
#[derive(Clone, Copy)]
pub struct Bound {
    limit: Duration,
    cite: &'static str,
}

impl Bound {
    // `Bound::from_register` — the NVMe `CAP.TO` shape §3 names beside
    // `from_spec` — is **not here yet, and that is the tree's dead-code rule
    // rather than an omission**. `nvme.rs:429` reads `cap` into a local and
    // never takes the `TO` field out of it, so C10 is the first chunk with a
    // register to cite; it writes the constructor beside its first caller,
    // three lines, and the kind does not change.

    /// A bound a specification states. `cite` is the section.
    pub const fn from_spec(limit: Duration, cite: &'static str) -> Self {
        Self { limit, cite }
    }

    pub const fn duration(self) -> Duration {
        self.limit
    }

    pub const fn nanos(self) -> u64 {
        self.limit.nanos()
    }
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.limit, self.cite)
    }
}

/// How often a thing may be re-done, and what makes that rate affordable.
///
/// Nothing expires: a `Cadence` is a rate. §3's first draft defined it as "how
/// fast the bit can physically change", which describes a register poll and
/// none of the cost budgets and log-rate limits the kernel actually has; the
/// definition is the widened one.
#[derive(Clone, Copy)]
pub struct Cadence {
    period: Duration,
    why: &'static str,
}

impl Cadence {
    pub const fn every(period: Duration, why: &'static str) -> Self {
        Self { period, why }
    }

    pub const fn duration(self) -> Duration {
        self.period
    }

    pub const fn nanos(self) -> u64 {
        self.period.nanos()
    }
}

impl fmt::Display for Cadence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.period, self.why)
    }
}

/// A duration whose expiry is a **panic**: the machine is broken, fail fast.
///
/// The constructor's argument is *how long is absurd*, not where the number
/// came from — a tripwire that could be cited would be a [`Bound`].
///
/// **A tripwire smaller than one bounded term of what it covers is not a
/// tripwire**, and the failure mode is a kernel that panics on a healthy
/// machine. "Absurd" is a claim about the whole path: every [`Bound`] a slow
/// but working machine may spend on the way has to fit inside it, and the
/// largest of those is usually a device's. `scheduler::retire_task`'s `GIVE_UP`
/// is the worked example — 1 s against a path containing two of xHCI's own 2 s
/// deadlines, and it fired on the owner's T14 at 949 s of uptime.
#[derive(Clone, Copy)]
pub struct Tripwire {
    limit: Duration,
    absurd_because: &'static str,
}

impl Tripwire {
    pub const fn absurd(limit: Duration, absurd_because: &'static str) -> Self {
        Self { limit, absurd_because }
    }

    pub const fn duration(self) -> Duration {
        self.limit
    }

    pub const fn nanos(self) -> u64 {
        self.limit.nanos()
    }
}

impl fmt::Display for Tripwire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.limit, self.absurd_because)
    }
}

/// A wall-clock allowance whose expiry is a **degraded answer**.
///
/// The dump's three budgets are the shape: none of them panics, each gives up
/// a field of the report and says so, and that is exactly right for a
/// diagnostic on a machine already known to be broken. So is a boot scan that
/// stops looking and reports what it found.
#[derive(Clone, Copy)]
pub struct Budget {
    limit: Duration,
    degraded: &'static str,
}

impl Budget {
    /// `degraded` is what the caller answers with when it is spent.
    pub const fn of(limit: Duration, degraded: &'static str) -> Self {
        Self { limit, degraded }
    }

    pub const fn duration(self) -> Duration {
        self.limit
    }

    pub const fn nanos(self) -> u64 {
        self.limit.nanos()
    }
}

impl fmt::Display for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.limit, self.degraded)
    }
}

/// A duration used as a bound on *another* duration rather than as a wait.
///
/// Nothing expires; there is no caller and no register. `MIN_ONE_SHOT_NS` is
/// why this kind has to exist: it is the LAPIC one-shot floor every arm is
/// clamped to, its own doc says "Policy, not physics", and an implementer
/// applying RT7 with only four kinds finds it unconstructible and deletes it —
/// which reopens #156, a CPU gone off the T14 on eight boots of eight.
#[derive(Clone, Copy)]
pub struct Floor {
    least: Duration,
    why: &'static str,
}

impl Floor {
    /// `why` states what bounds it below and above; a floor with neither is a
    /// number somebody liked.
    pub const fn policy(least: Duration, why: &'static str) -> Self {
        Self { least, why }
    }

    pub const fn nanos(self) -> u64 {
        self.least.nanos()
    }
}

impl fmt::Display for Floor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.least, self.why)
    }
}

/// A duration the caller **spends**. Nothing expires and nothing is waited
/// for: the elapsing is the success path.
///
/// Two shapes, and both are real in this tree. A device specification mandates
/// a settle the driver may not skip — 25 frames after `CRST`, 10 ms out of
/// D3hot, the SDM's delay between INIT and SIPI — and a calibration spends a
/// window in order to *measure* something across it. Neither can fail, which
/// is what separates this from every other kind here.
#[derive(Clone, Copy)]
pub struct Delay {
    span: Duration,
    why: &'static str,
}

impl Delay {
    /// A settle a specification mandates. `cite` is the section.
    pub const fn from_spec(span: Duration, cite: &'static str) -> Self {
        Self { span, why: cite }
    }

    /// A window spent measuring something across it. `why` says what the
    /// length buys — accuracy against boot time, always.
    pub const fn to_measure(span: Duration, why: &'static str) -> Self {
        Self { span, why }
    }

    pub const fn nanos(self) -> u64 {
        self.span.nanos()
    }
}

impl fmt::Display for Delay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.span, self.why)
    }
}
