//! What a crashing CPU may touch before it trusts the machine, and the evidence
//! it leaves when it has nowhere left to report.
//!
//! Two dead ends end a crash with no report of their own. The **reentry guard**
//! fires when the panic *report* panics, and the **`DOUBLE PANIC`** arm fires
//! when a panic arrives on a CPU that is already inside a fault or a report.
//! Until 2026-08-20 the machine's complete last words in the second case were
//! the two words `DOUBLE PANIC`, and in the first the one sentence
//! `PANIC REENTRY: CPU halted` — not what the first crash was, not where, and
//! not what the second one was
//! (`issues/panic-path/a-double-panic-at-boots-edge-says-nothing-but-its-name.md`,
//! finding 2). The one class of crash that is by definition two bugs deep was
//! the one class that left no evidence at all.
//!
//! **The evidence is copied at the moment the first crash begins, before
//! anything is printed.** [`record_panic`] runs as the first statement of the
//! panic handler and [`record_fault`] as the first statement of
//! `fatal_exception` after the state swap — both ahead of every formatter, every
//! lock and every device this kernel has. What they do is a bounded byte copy
//! into a static reserved when the image was linked: no allocation, no lock, no
//! `core::fmt`, no page that can be absent, and nothing that can itself panic.
//! Everything downstream of them may die without taking the report with it,
//! which is the whole property — a mechanism that captured the first crash
//! *while* reporting it would be a mechanism whose failure mode is this issue.
//!
//! **What it deliberately does not attempt.** No unwinding, and no second
//! formatting pass: the message is taken from
//! [`core::panic::PanicMessage::as_str`], which is `Some` exactly when the panic
//! carries a literal and needs no runtime formatting, and is otherwise left out
//! by name. Running `core::fmt` here would put a `Display` impl — free to lock,
//! to allocate and to panic — inside the one mechanism whose value is that it
//! cannot fail; the formatted text is `crash_report`'s job and reaches the
//! record ring one instruction later. So `expect("…")` and `assert_eq!` leave a
//! location and no message, and `panic!("…")`, `assert!` and `unwrap()` leave
//! both.
//!
//! **The slot holds this CPU's *first* unfinished crash.** [`claim`] is a
//! compare-exchange from `None`, so the second crash — the one that is doing the
//! reporting, and whose `PanicInfo` is live on its own stack — never overwrites
//! the first. [`forget`] releases it wherever a CPU declares itself normal
//! again, so a *recovered* panic cannot be reported an hour later as somebody
//! else's first event. The demand-paging return does not release it and does not
//! need to: nothing that captures can reach that path without passing one of
//! [`forget`]'s three call sites, and [`apic_id`] is a `CPUID` — not a thing to
//! put on the fault path this kernel takes millions of.
//!
//! **Where the report goes.** [`last_words`] streams straight to the 16550
//! through `serial::panic_raw`: no lock, no ring, no percpu, bounded per byte.
//! That is the one channel a dead end may use, because the guard the log's
//! console drain takes is exactly what the first crash may be holding — and a
//! second panic that arrives while the first holds it is one plausible way a
//! first panic becomes a double. The `DOUBLE PANIC` arm *also* says it as a
//! record, because that is the only channel a machine with no serial port has:
//! the on-screen panel and the virtio-console both read records, and the sighting
//! this module answers reached the wire that way. Raw first, so a wedge in the
//! record path costs the second copy and never the first. On a UART-only machine
//! the report therefore arrives twice, which is the same trade
//! `log::console::drain_bypassed` already makes: twice beats never on a machine
//! that is halting.
//!
//! The two renderings differ on purpose — the raw one carries APIC ids and hex,
//! the record is the readable line a panel shows — and they are written ten
//! lines apart in one function so they cannot drift into disagreeing about the
//! evidence.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::arch::cpu;
use crate::arch::percpu::CpuFaultState;
use crate::drivers::serial;

/// Slots in every per-CPU array here: an APIC id masked to six bits.
///
/// See [`apic_id`] for why CPUID and not the percpu block, and
/// [`PANIC_DEPTH`] for what masking costs.
const SLOTS: usize = 64;

/// Per-CPU panic-reentry depth, indexed by x2APIC id (masked). The panic path
/// must not trust GS/percpu: a corrupted percpu block makes `swap_fault_state`
/// itself fault, re-entering the panic handler in an unbounded recursion that
/// smashes the stack down through the heap. CPUID is the only per-CPU
/// discriminator that needs no memory access and no enabled unit at all.
///
/// A single global flag was rejected: it would stay set after a *recovered*
/// panic and silently swallow every later, independent panic report, and a
/// panic on one CPU would mask a concurrent first panic on another. Masking
/// the APIC id to 64 slots only means colliding CPUs share a guard — a
/// concurrent panic on both halts the second, which halt_all_cpus would do
/// moments later anyway.
static PANIC_DEPTH: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

/// This CPU's APIC id, from CPUID.
///
/// It used to be `rdmsr(IA32_X2APIC_APICID)` under a comment claiming APs
/// enable x2APIC before running any panicking kernel code. They do not:
/// `apic::init_ap` is three calls after `percpu::init_ap` in `ap_entry`, and
/// that MSR is `#GP` until it has run. Every panic an AP took in between —
/// `pat::init`'s assertion, `control_regs`', the FSGSBASE one that has been
/// there all along — faulted *inside the reentry guard*, before the guard was
/// armed, and the machine triple-faulted with the whole boot still unflushed in
/// the log ring: no report, no backtrace, no serial output at all.
///
/// Leaf 0x1F and leaf 0xB give the full x2APIC id and leaf 1 the 8-bit initial
/// one; the slot is masked to 64 either way, so the fallback loses nothing this
/// array was keeping.
pub fn apic_id() -> u32 {
    let (max_leaf, _, _, _) = cpu::cpuid(0, 0);
    for leaf in [0x1F, 0x0B] {
        if max_leaf >= leaf {
            let (_, ebx, _, edx) = cpu::cpuid(leaf, 0);
            // Reaching the leaf index is not the existence test: SDM Vol. 2A,
            // `CPUID` leaf 0BH, requires `EBX[15:0]` non-zero as well, and a CPU
            // whose maximum leaf covers it without implementing it answers zero
            // in every register. That is not a distinguisher — every CPU would
            // take slot 0 and share one guard — and it is reached by skipping
            // the leaf-1 fallback that is correct for exactly that machine.
            if ebx & 0xFFFF != 0 {
                return edx;
            }
        }
    }
    let (_, ebx, _, _) = cpu::cpuid(1, 0);
    ebx >> 24
}

/// This CPU's reentry depth.
pub fn depth_slot() -> &'static AtomicU32 {
    &PANIC_DEPTH[apic_id() as usize & (SLOTS - 1)]
}

/// How much of a crash site this keeps.
///
/// **96 bytes of path, and the *tail* of it.** The longest source path in this
/// tree is 44 characters (`toyos-sched/sim/tests/deadline_claim_race.rs`) and
/// the longest the kernel itself can name is 39
/// (`kernel/src/drivers/panic_console/mod.rs`), but `file!()` is whatever
/// rustc was handed: CI captures carry
/// `/__w/toyos/toyos/toyos-sched/src/cpu.rs` and a `core` panic names the
/// fork's own `library/core/src/…` under whatever directory the checkout is in.
/// The head of such a path is the build host and the tail is the identity, so
/// what overflows is cut off the front and marked.
///
/// **128 bytes of message.** A record's own bound is `MAX_RECORD_MESSAGE`, 992,
/// and this is not a record: it holds a panic literal, which in this kernel's
/// corpus is one sentence. A message longer than this is cut at a character
/// boundary and the location — the part that is always a lead — is unaffected.
///
/// 260 bytes a slot, 16,640 for the machine, in `.bss` beside
/// `panic_console`'s three 32 KiB snapshots. No alignment padding, because
/// nothing writes these from a hot path: the two writers are a panic and a
/// fatal exception.
const FILE_BYTES: usize = 96;
const MSG_BYTES: usize = 128;

/// What a slot holds, or that it holds nothing.
#[derive(Clone, Copy)]
#[repr(u8)]
enum Kind {
    None = 0,
    Panic = 1,
    Fault = 2,
}

impl Kind {
    fn of(raw: u8) -> Self {
        match raw {
            1 => Self::Panic,
            2 => Self::Fault,
            // A slot nothing claimed, or one whose byte is not a kind at all.
            // The report says "nothing captured" either way, which is true of
            // both and is the only honest answer to a corrupted one.
            _ => Self::None,
        }
    }
}

/// One CPU's first unfinished crash.
///
/// Every field is an atomic because there is no `unsafe` in this module and
/// none is needed: the stores compile to the same `mov`s a plain array would
/// take. The ordering is `Relaxed` throughout and that is not a weakening —
/// one CPU writes its own slot and the same CPU reads it, with interrupts
/// masked from the capture to the report, and no other CPU ever looks.
///
/// The lengths are stored *after* the bytes, so a slot read halfway through a
/// fill — an NMI that panics between the claim and the copy — reports a short
/// string rather than the previous crash's tail.
struct Evidence {
    kind: AtomicU8,
    file_len: AtomicU8,
    msg_len: AtomicU8,
    /// The path did not fit and its head is what was dropped.
    file_cut: AtomicU8,
    /// Which CPU filled this in, unmasked, so that two CPUs sharing a slot is
    /// visible in the report rather than a quiet lie.
    apic: AtomicU32,
    line: AtomicU32,
    column: AtomicU32,
    rip: AtomicU64,
    cr2: AtomicU64,
    error_code: AtomicU64,
    file: [AtomicU8; FILE_BYTES],
    /// A panic's literal message, or a fault's vector name.
    msg: [AtomicU8; MSG_BYTES],
}

impl Evidence {
    const fn new() -> Self {
        Self {
            kind: AtomicU8::new(Kind::None as u8),
            file_len: AtomicU8::new(0),
            msg_len: AtomicU8::new(0),
            file_cut: AtomicU8::new(0),
            apic: AtomicU32::new(0),
            line: AtomicU32::new(0),
            column: AtomicU32::new(0),
            rip: AtomicU64::new(0),
            cr2: AtomicU64::new(0),
            error_code: AtomicU64::new(0),
            file: [const { AtomicU8::new(0) }; FILE_BYTES],
            msg: [const { AtomicU8::new(0) }; MSG_BYTES],
        }
    }
}

static FIRST: [Evidence; SLOTS] = [const { Evidence::new() }; SLOTS];

fn evidence() -> &'static Evidence {
    &FIRST[apic_id() as usize & (SLOTS - 1)]
}

/// Take this CPU's slot, or decline because the crash being reported already
/// has it. The decline is the point: the *first* crash is the one nothing else
/// can recover.
fn claim(slot: &Evidence, kind: Kind) -> bool {
    slot.kind
        .compare_exchange(Kind::None as u8, kind as u8, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Copy a panic's site into this CPU's slot. The first statement of the panic
/// handler, before the early-boot branch and before the reentry guard.
pub fn record_panic(info: &core::panic::PanicInfo) {
    let slot = evidence();
    if !claim(slot, Kind::Panic) {
        return;
    }
    slot.apic.store(apic_id(), Ordering::Relaxed);
    if let Some(location) = info.location() {
        copy_tail(&slot.file, &slot.file_len, &slot.file_cut, location.file().as_bytes());
        slot.line.store(location.line(), Ordering::Relaxed);
        slot.column.store(location.column(), Ordering::Relaxed);
    }
    if let Some(message) = info.message().as_str() {
        copy_head(&slot.msg, &slot.msg_len, message.as_bytes());
    }
}

/// Copy a fatal exception into this CPU's slot, before the fault has said
/// anything about itself.
///
/// `name` is `exceptions::vector_name`'s answer and is copied rather than kept
/// as a pointer, for the same reason the path is: the emitter dereferences
/// nothing it was handed by a machine that has already failed once.
pub fn record_fault(name: &str, rip: u64, cr2: u64, error_code: u64) {
    let slot = evidence();
    if !claim(slot, Kind::Fault) {
        return;
    }
    slot.apic.store(apic_id(), Ordering::Relaxed);
    copy_head(&slot.msg, &slot.msg_len, name.as_bytes());
    slot.rip.store(rip, Ordering::Relaxed);
    slot.cr2.store(cr2, Ordering::Relaxed);
    slot.error_code.store(error_code, Ordering::Relaxed);
}

/// This CPU is out of the crash it was in: release the slot so the next one
/// captures its own first event.
///
/// Called wherever a CPU sets its fault state back to `Normal` and carries on —
/// the panic recovery, the fault recovery, and the recursive-fault arm that
/// ends a process instead of the machine.
pub fn forget() {
    evidence().kind.store(Kind::None as u8, Ordering::Relaxed);
}

/// The last `dst.len()` bytes of `src`: a path's tail is its identity and its
/// head is whichever directory this build ran in.
fn copy_tail(dst: &[AtomicU8], len: &AtomicU8, cut: &AtomicU8, src: &[u8]) {
    let mut from = src.len().saturating_sub(dst.len());
    // A cut inside a multi-byte character makes the whole line unprintable.
    while from > 0 && matches!(src.get(from), Some(b) if b & 0xC0 == 0x80) {
        from += 1;
    }
    let tail = src.get(from..).unwrap_or(&[]);
    for (slot, &b) in dst.iter().zip(tail) {
        slot.store(b, Ordering::Relaxed);
    }
    len.store(tail.len() as u8, Ordering::Relaxed);
    cut.store(u8::from(from > 0), Ordering::Relaxed);
}

/// The first `dst.len()` bytes of `src`, cut back to a character boundary.
fn copy_head(dst: &[AtomicU8], len: &AtomicU8, src: &[u8]) {
    let mut n = dst.len().min(src.len());
    while n > 0 && matches!(src.get(n), Some(b) if b & 0xC0 == 0x80) {
        n -= 1;
    }
    for (slot, &b) in dst.iter().zip(src.get(..n).unwrap_or(&[])) {
        slot.store(b, Ordering::Relaxed);
    }
    len.store(n as u8, Ordering::Relaxed);
}

/// A slot's bytes as text, in the caller's own buffer.
fn read<'a>(src: &[AtomicU8], len: &AtomicU8, out: &'a mut [u8]) -> &'a str {
    let n = (len.load(Ordering::Relaxed) as usize).min(out.len()).min(src.len());
    for (byte, slot) in out.iter_mut().zip(src.iter()) {
        *byte = slot.load(Ordering::Relaxed);
    }
    core::str::from_utf8(out.get(..n).unwrap_or(&[])).unwrap_or("<not utf-8>")
}

fn state_name(state: CpuFaultState) -> &'static str {
    match state {
        CpuFaultState::Normal => "Normal",
        CpuFaultState::PageFault => "PageFault",
        CpuFaultState::Fatal => "Fatal",
        CpuFaultState::Panic => "Panic",
    }
}

/// What the second panic's message says when it was formatted at runtime and
/// this module refused to run a formatter to find out.
const NOT_CAPTURED: &str = "<formatted at runtime; not captured>";

/// This CPU's last words: the crash it was already inside, and the panic that
/// has just ended it.
///
/// `header` names the dead end. `prev` is the fault state the arriving panic
/// found, where the caller has one — the reentry guard runs *before* the state
/// swap and deliberately does not read percpu to get one. `second` is that
/// panic, still live on this stack, so its site and literal are read straight
/// out of it rather than captured.
///
/// `on_the_record` also says it as an `alert!`. False for the reentry guard,
/// whose whole premise is that the report path is the suspect; true for
/// `DOUBLE PANIC`, where the record ring is the only channel a machine with no
/// serial port has.
pub fn last_words(
    header: &str,
    prev: Option<CpuFaultState>,
    second: &core::panic::PanicInfo,
    on_the_record: bool,
) {
    let slot = evidence();
    let mut file_bytes = [0u8; FILE_BYTES];
    let mut msg_bytes = [0u8; MSG_BYTES];
    let kind = Kind::of(slot.kind.load(Ordering::Relaxed));
    let file = read(&slot.file, &slot.file_len, &mut file_bytes);
    let message = read(&slot.msg, &slot.msg_len, &mut msg_bytes);
    let cut = if slot.file_cut.load(Ordering::Relaxed) == 0 { "" } else { "..." };
    let line = slot.line.load(Ordering::Relaxed);
    let column = slot.column.load(Ordering::Relaxed);
    let rip = slot.rip.load(Ordering::Relaxed);
    let cr2 = slot.cr2.load(Ordering::Relaxed);
    let error_code = slot.error_code.load(Ordering::Relaxed);

    let (second_file, second_line, second_column) = match second.location() {
        Some(location) => (location.file(), location.line(), location.column()),
        None => ("<no location>", 0, 0),
    };
    let second_message = second.message().as_str().unwrap_or(NOT_CAPTURED);

    let raw: fn(&[u8]) = serial::panic_raw;
    raw(b"\n!!! ");
    raw(header.as_bytes());
    raw(b" !!! (apic ");
    serial::panic_raw_dec(u64::from(apic_id()));
    if let Some(prev) = prev {
        raw(b", the cpu was already in ");
        raw(state_name(prev).as_bytes());
    }
    raw(b")\n  first (apic ");
    serial::panic_raw_dec(u64::from(slot.apic.load(Ordering::Relaxed)));
    raw(b"): ");
    match kind {
        Kind::Panic => {
            raw(b"panic at ");
            raw(cut.as_bytes());
            raw(file.as_bytes());
            raw(b":");
            serial::panic_raw_dec(u64::from(line));
            raw(b":");
            serial::panic_raw_dec(u64::from(column));
            raw(b": ");
            raw(message.as_bytes());
        }
        Kind::Fault => {
            raw(message.as_bytes());
            raw(b" rip=");
            serial::panic_raw_hex(rip);
            raw(b" cr2=");
            serial::panic_raw_hex(cr2);
            raw(b" err=");
            serial::panic_raw_hex(error_code);
        }
        Kind::None => raw(b"nothing captured on this cpu"),
    }
    raw(b"\n  second: panic at ");
    raw(second_file.as_bytes());
    raw(b":");
    serial::panic_raw_dec(u64::from(second_line));
    raw(b":");
    serial::panic_raw_dec(u64::from(second_column));
    raw(b": ");
    raw(second_message.as_bytes());
    raw(b"\n");

    if !on_the_record {
        return;
    }
    // ASCII and one line, because the last reader of this is a panel that
    // renders codepoints 0x20..=0x7E and paints one record per row. Nothing
    // here formats anything but `&str` and integers, whose `Display` cannot
    // fail; what can still fail is `emit` itself, which is why the raw report
    // above has already gone out.
    let state = prev.map_or("", state_name);
    match kind {
        Kind::Panic => alert!(
            "{header}: the cpu was already in {state}; first: panic at {cut}{file}:{line}:{column}: \
             {message}; second: panic at {second_file}:{second_line}:{second_column}: \
             {second_message}"
        ),
        Kind::Fault => alert!(
            "{header}: the cpu was already in {state}; first: {message} rip={rip:#018x} \
             cr2={cr2:#018x} err={error_code:#018x}; second: panic at \
             {second_file}:{second_line}:{second_column}: {second_message}"
        ),
        Kind::None => alert!(
            "{header}: the cpu was already in {state}; first: nothing captured on this cpu; \
             second: panic at {second_file}:{second_line}:{second_column}: {second_message}"
        ),
    }
}
