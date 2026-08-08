//! Raises one CPU exception from Ring 3, named by `argv[1]`.
//!
//! Every arm here is a plain userland instruction sequence — no syscall, no
//! privileged state. If the vector has no IDT gate the CPU escalates to #DF and
//! the machine halts, which is what `fault_gates` exists to catch.
//!
//! An arm that returns prints `survived` plus the status word that says why,
//! so a surviving arm is evidence rather than a shrug: the CPU either never
//! flagged the condition, or flagged it and declined to trap.

fn main() {
    let kind = std::env::args().nth(1).expect("usage: fault_gate_child <kind>");
    match kind.as_str() {
        "de" => divide_by_zero(),
        "de_overflow" => divide_overflow(),
        "ss" => stack_fault(),
        "ss_rsp" => stack_fault_via_rsp(),
        "mf" => x87_exception(),
        "xm" => simd_exception(),
        "ac" => alignment_check(),
        other => panic!("unknown fault kind {other}"),
    }
    println!("survived {kind}");
}

/// #DE (0). `div` with a zero divisor.
#[inline(never)]
fn divide_by_zero() {
    let divisor = std::hint::black_box(0u64);
    unsafe {
        core::arch::asm!(
            "div {d}",
            d = in(reg) divisor,
            inout("rax") 1u64 => _,
            inout("rdx") 0u64 => _,
            options(nomem, nostack),
        );
    }
}

/// #DE (0) again, by the other route: a quotient that does not fit. Same
/// vector, and it is the one a divisor check does not cover.
#[inline(never)]
fn divide_overflow() {
    let divisor = std::hint::black_box(1u64);
    unsafe {
        core::arch::asm!(
            "div {d}",
            d = in(reg) divisor,
            inout("rax") 0u64 => _,
            inout("rdx") 1u64 => _,
            options(nomem, nostack),
        );
    }
}

/// #SS (12). A non-canonical address reached through RBP, whose default
/// segment is SS. RSP is left alone so a machine that does not raise it here
/// still has a stack to print `survived` from.
///
/// Arrives as **#GP** under TCG, which raises `EXCP0D_GPF` for every
/// non-canonical access and models #SS for none of them. On metal the SDM
/// gives #SS for an SS-relative one, so this arm exercises the #GP gate here
/// and the #SS gate there.
#[inline(never)]
fn stack_fault() {
    let bad = std::hint::black_box(0x8000_0000_0000_0000u64);
    unsafe {
        core::arch::asm!(
            "push rbp",
            "mov rbp, {b}",
            "mov {t}, [rbp]",
            "pop rbp",
            b = in(reg) bad,
            t = out(reg) _,
        );
    }
}

/// #SS (12) again, through the stack pointer itself rather than RBP. Nothing
/// after this can run: RSP is gone whether or not the CPU faults.
#[inline(never)]
fn stack_fault_via_rsp() {
    let bad = std::hint::black_box(0x8000_0000_0000_0000u64);
    unsafe {
        core::arch::asm!(
            "mov rsp, {b}",
            "push rax",
            b = in(reg) bad,
            options(nomem),
        );
    }
}

/// #MF (16). Unmask the x87 invalid-operation exception, compute 0/0, then
/// `fwait` — the exception is raised on the next waiting instruction, not on
/// the divide. Needs CR0.NE.
#[inline(never)]
fn x87_exception() {
    let cw: u16 = 0x037E;
    let mut sw: u16 = 0;
    unsafe {
        core::arch::asm!(
            "fninit",
            "fldcw [{cw}]",
            "fldz",
            "fldz",
            "fdivp",
            "fwait",
            "fnstsw [{sw}]",
            "fninit",
            cw = in(reg) &cw,
            sw = in(reg) &mut sw,
            options(nostack),
        );
    }
    println!("  x87 status after 0/0 with IM unmasked: {sw:#06x}");
}

/// #XM (19). Unmask the SSE invalid-operation exception in MXCSR and compute
/// 0.0/0.0. CR4.OSXMMEXCPT is set, so the architecture delivers this — TCG
/// does not, and the MXCSR it prints is the proof: IE raised with IM clear and
/// no trap taken.
#[inline(never)]
fn simd_exception() {
    let unmasked: u32 = 0x0000_1F00;
    let default: u32 = 0x0000_1F80;
    let mut after: u32 = 0;
    unsafe {
        core::arch::asm!(
            "ldmxcsr [{m}]",
            "xorpd xmm0, xmm0",
            "divsd xmm0, xmm0",
            "stmxcsr [{a}]",
            "ldmxcsr [{d}]",
            m = in(reg) &unmasked,
            d = in(reg) &default,
            a = in(reg) &mut after,
            out("xmm0") _,
            options(nostack),
        );
    }
    println!("  MXCSR after 0.0/0.0 with IM unmasked: {after:#010x}");
}

/// #AC (17). RFLAGS.AC plus a misaligned load. CR0.AM gates it as well and is
/// clear — firmware leaves it so and nothing sets it — which is what the
/// printed readback separates from a `popfq` that did not take.
#[inline(never)]
fn alignment_check() {
    let buf = [0u8; 16];
    let misaligned = unsafe { buf.as_ptr().add(1) } as *const u32;
    let flags: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "or qword ptr [rsp], 0x40000",
            "popfq",
            "mov {t:e}, [{p}]",
            "pushfq",
            "pop {f}",
            "push {f}",
            "and qword ptr [rsp], 0xFFFFFFFFFFFBFFFF",
            "popfq",
            p = in(reg) misaligned,
            t = out(reg) _,
            f = out(reg) flags,
        );
    }
    println!("  RFLAGS.AC readback after a misaligned load: {}", (flags >> 18) & 1);
}
