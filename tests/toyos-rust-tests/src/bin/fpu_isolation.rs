//! What a transition out of Ring 3 preserves — `specs/user-machine-state.md`.
//!
//! Three arms, all positive assertions, and each one fails on the tree that
//! came before the bracket in `kernel/src/arch/entry.rs`:
//!
//! 1. **Leak.** One process pins a distinctive FP state and exits without
//!    restoring it; the next asserts the *declared* state at its own entry.
//! 2. **Fault.** `fault_gate_child mf` dies with an unmasked x87 exception
//!    pending; the next process executes `FLDCW` — a waiting instruction — and
//!    must survive. That is the defect CI proved by one token: `std_unwind`'s
//!    victim was the `FLDCW` inside the unwinder's `restore_context`, and every
//!    ToyOS binary executes one on every panic.
//! 3. **Preservation.** One process pins a state, forces many transitions of
//!    each kind — syscall, demand page fault, timer preemption — against an
//!    FP-heavy sibling, and asserts bit-identity.
//!
//! **It is run at `smp=1`, and that is the stronger choice rather than the
//! weaker one.** The defect is a CPU register file carrying over between tasks,
//! so an arm only means anything when the two tasks share a CPU. With more CPUs
//! that is a coin flip, which is why CI's observation of it was intermittent.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use core::mem::offset_of;

use toyos_abi::syscall::{
    mmap, nanosleep, thread_join, thread_spawn, MmapFlags, MmapProt, SYS_CLOCK, SYS_EXIT,
    SYS_THREAD_EXIT,
};

/// The FXSAVE64 image, at the alignment the instruction requires.
#[repr(C, align(16))]
struct FpImage([u8; 512]);

// Field offsets, SDM Vol. 1 Table 10-2.
const OFF_FCW: usize = 0;
const OFF_FSW: usize = 2;
const OFF_FTW: usize = 4;
const OFF_MXCSR: usize = 24;
const OFF_ST0: usize = 32;
const OFF_XMM0: usize = 160;
const END_XMM: usize = 416;

/// Every x87 exception masked, extended precision, round to nearest.
const FCW_DECLARED: u16 = 0x037F;
/// Every SSE exception masked, round to nearest, no flush-to-zero.
const MXCSR_DECLARED: u32 = 0x1F80;

/// Masked exactly like the declared words — nothing here can raise anything —
/// and different from them in every field that has more than one value:
/// round-toward-zero for both, single precision for x87.
const FCW_PINNED: u16 = 0x0C7F;
const MXCSR_PINNED: u32 = 0x7F80;

/// Everything the pinning assembly reads or writes, in one object, so the
/// blocks below need one base register rather than eight.
///
/// `r15` is that register, named explicitly at every site rather than left to
/// the allocator: `clobber_abi("sysv64")` does not stop LLVM putting an
/// `in(reg)` input in a clobbered register, and it put this one in `rax`, which
/// the `syscall` loop then destroyed — the base register read back as a
/// truncated 32-bit value and the next load segfaulted.
#[repr(C, align(16))]
struct Arena {
    cw: u16,
    _pad: [u8; 2],
    mx: u32,
    /// The page-fault arm's first untouched page.
    region: u64,
    /// Sixteen distinctive XMM values, one per register.
    xmm: [u64; 32],
    before: FpImage,
    after: FpImage,
}

static mut ARENA: Arena = Arena {
    cw: FCW_PINNED,
    _pad: [0; 2],
    mx: MXCSR_PINNED,
    region: 0,
    xmm: {
        let mut v = [0u64; 32];
        let mut i = 0;
        while i < 32 {
            v[i] = 0xF9A3_0000_0000_0000 | (i as u64 + 1);
            i += 1;
        }
        v
    },
    before: FpImage([0; 512]),
    after: FpImage([0; 512]),
};

/// What this process's own state was at the first instruction of `main`.
static mut ENTRY_IMAGE: FpImage = FpImage([0; 512]);

const PAGE_2M: usize = 2 * 1024 * 1024;
/// First touches the preservation arm makes of [`DEMAND`], 2 MiB apart, so each
/// is a page of its own and each is a `#PF`.
const FAULT_TOUCHES: u64 = 2;

/// The page-fault workload, and the only kind of demand-paged memory a userland
/// program on this kernel can reach.
///
/// Not `mmap`: `sys_mmap` allocates and maps its whole region up front, so the
/// first touch of a fresh mapping faults nothing. Not `.bss` either: `toyos-ld`
/// writes it into the file, so a `PT_LOAD`'s `filesz` equals its `memsz` and
/// the loader's `Anonymous` tail is empty. What is left is a *writable
/// file-backed* page — non-zero so it lands in `.data`, and named by nothing
/// else so no relocation has touched it — which faults on first write and takes
/// the allocate-and-copy path. Four megabytes of test image is what two faults
/// cost, and that is the whole reason there are two rather than twenty.
static mut DEMAND: [u8; PAGE_2M * 2] = [1; PAGE_2M * 2];

/// Where the raw thread probe writes what it found at its very first
/// instruction. A static, because there is nothing between the trampoline's
/// `iretq` and the `fxsave64` and there must not be.
static mut THREAD_IMAGE: FpImage = FpImage([0; 512]);

/// The pin sequence: a distinctive value in every register this kernel permits
/// to exist. Shared by the two arms that need one so they cannot drift.
macro_rules! pin_state {
    () => {
        concat!(
            "fninit\n",
            "fldcw [r15]\n",
            "fld1\nfldl2t\nfldl2e\nfldpi\nfldlg2\nfldln2\nfld1\nfldpi\n",
            "ldmxcsr [r15 + {mx}]\n",
            "movdqu xmm0,  [r15 + {x} + 0*16]\n",
            "movdqu xmm1,  [r15 + {x} + 1*16]\n",
            "movdqu xmm2,  [r15 + {x} + 2*16]\n",
            "movdqu xmm3,  [r15 + {x} + 3*16]\n",
            "movdqu xmm4,  [r15 + {x} + 4*16]\n",
            "movdqu xmm5,  [r15 + {x} + 5*16]\n",
            "movdqu xmm6,  [r15 + {x} + 6*16]\n",
            "movdqu xmm7,  [r15 + {x} + 7*16]\n",
            "movdqu xmm8,  [r15 + {x} + 8*16]\n",
            "movdqu xmm9,  [r15 + {x} + 9*16]\n",
            "movdqu xmm10, [r15 + {x} + 10*16]\n",
            "movdqu xmm11, [r15 + {x} + 11*16]\n",
            "movdqu xmm12, [r15 + {x} + 12*16]\n",
            "movdqu xmm13, [r15 + {x} + 13*16]\n",
            "movdqu xmm14, [r15 + {x} + 14*16]\n",
            "movdqu xmm15, [r15 + {x} + 15*16]\n",
        )
    };
}

fn fxsave(dst: *mut FpImage) {
    unsafe {
        core::arch::asm!("fxsave64 [{}]", in(reg) dst, options(nostack));
    }
}

fn fcw(img: &FpImage) -> u16 {
    u16::from_le_bytes([img.0[OFF_FCW], img.0[OFF_FCW + 1]])
}

fn fsw(img: &FpImage) -> u16 {
    u16::from_le_bytes([img.0[OFF_FSW], img.0[OFF_FSW + 1]])
}

fn mxcsr(img: &FpImage) -> u32 {
    u32::from_le_bytes([
        img.0[OFF_MXCSR],
        img.0[OFF_MXCSR + 1],
        img.0[OFF_MXCSR + 2],
        img.0[OFF_MXCSR + 3],
    ])
}

/// The registers themselves: the x87 file and XMM0-15, contiguous in the image.
fn registers(img: &FpImage) -> &[u8] {
    &img.0[OFF_ST0..END_XMM]
}

fn main() {
    // Before anything else this process does: arm 1's observation. Its parent
    // spawned a `pin` immediately beforehand, and the assertion is that none of
    // what that left is here.
    fxsave(&raw mut ENTRY_IMAGE);

    match std::env::args().nth(1).as_deref() {
        Some("pin") => pin_and_exit(),
        Some("check") => check_entry_state(),
        Some("fldcw") => fldcw_survivor(),
        Some(other) => panic!("unknown mode {other}"),
        None => driver(),
    }
}

/// Every arm runs and every verdict is collected, rather than the first failure
/// ending the run: the negative control's whole job is to show that each arm
/// has teeth, and a run that stops at the first one proves it about one.
fn driver() {
    let mut failures: Vec<String> = Vec::new();
    for round in 0..3 {
        failures.extend(leak_arm(round).err());
        failures.extend(fault_arm(round).err());
    }
    failures.extend(preservation_arm().err());
    for f in &failures {
        println!("  FAILED: {f}");
    }
    assert!(failures.is_empty(), "{} arm(s) did not preserve the state", failures.len());
    println!("every transition out of Ring 3 preserved the whole user machine state");
}

fn require(ok: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why()) }
}

fn spawn_mode(mode: &str) -> std::process::ExitStatus {
    Command::new("/bin/test_rs_fpu_isolation")
        .arg(mode)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {mode}: {e}"))
}

/// Arm 1. `pin` leaves a state behind; `check` must not find it.
fn leak_arm(round: u32) -> Result<(), String> {
    let pinned = spawn_mode("pin");
    require(pinned.success(), || {
        format!("round {round}: the pin child exited {:?}", pinned.code())
    })?;
    let status = spawn_mode("check");
    require(status.success(), || {
        format!(
            "leak, round {round}: a process started with the previous one's FP registers \
             (exit {:?})",
            status.code(),
        )
    })
}

/// Arm 2. A process dies with an unmasked x87 exception pending; the next one
/// executes a waiting instruction and must live.
fn fault_arm(round: u32) -> Result<(), String> {
    // The child's own verdict is this arm's precondition rather than a bonus
    // check: a child that survives its `fwait` reaches the `fninit` two
    // instructions later, which masks everything again and leaves the next
    // process nothing to be protected from. Unasserted, this arm passes
    // vacuously on exactly the machine it is for.
    let child = Command::new("/bin/test_rs_fault_gate_child")
        .arg("mf")
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn fault_gate_child mf: {e}"));
    require(!child.success(), || {
        format!(
            "fault, round {round}: the #MF child lived, so it left the FPU masked and clean \
             and the arm below asserts nothing (exit {:?})",
            child.code(),
        )
    })?;
    let status = spawn_mode("fldcw");
    require(status.success(), || {
        format!(
            "fault, round {round}: FLDCW took an exception the process never caused — the \
             previous process's pending x87 exception was still on the CPU (exit {:?})",
            status.code(),
        )
    })
}

/// Load the distinctive state and leave Ring 3 in the same instruction stream,
/// so nothing between here and the syscall can disturb it.
fn pin_and_exit() -> ! {
    unsafe {
        core::arch::asm!(
            pin_state!(),
            "mov rdi, {exit}",
            "xor esi, esi",
            "syscall",
            in("r15") &raw const ARENA,
            mx = const offset_of!(Arena, mx),
            x = const offset_of!(Arena, xmm),
            exit = const SYS_EXIT,
            options(noreturn),
        );
    }
}

/// Arm 1's assertion, in two halves.
fn check_entry_state() {
    let entry = unsafe { &*(&raw const ENTRY_IMAGE) };
    assert_eq!(
        fcw(entry),
        FCW_DECLARED,
        "this process started with the previous one's x87 control word",
    );
    assert_eq!(
        mxcsr(entry),
        MXCSR_DECLARED,
        "this process started with the previous one's MXCSR",
    );
    assert_eq!(entry.0[OFF_FTW], 0, "this process started with a non-empty x87 stack");
    assert!(
        entry.0[OFF_ST0..OFF_XMM0].iter().all(|&b| b == 0),
        "this process started with the previous one's x87 registers",
    );

    // The XMM half cannot be asserted here: std's startup has already run and it
    // uses XMM. A raw thread can be asked, because between the loader's
    // trampoline and its first instruction there is nothing but an `iretq`.
    thread_entry_state();
    println!("  entry state is the declared one, in the process and in a fresh thread");
}

/// A thread whose first instruction records the whole state, so the declared
/// state can be asserted in full — XMM included.
#[unsafe(naked)]
extern "C" fn thread_probe() {
    core::arch::naked_asm!(
        "fxsave64 [rdi]",
        "mov rdi, {exit}",
        "xor esi, esi",
        "syscall",
        "ud2",
        exit = const SYS_THREAD_EXIT,
    );
}

fn thread_entry_state() {
    const STACK: usize = 2 * 1024 * 1024;
    let stack = unsafe {
        mmap(
            core::ptr::null_mut(),
            STACK,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    assert!(!stack.is_null(), "no stack for the thread probe");
    let entry: extern "C" fn() = thread_probe;
    let tid = unsafe {
        thread_spawn(
            entry as *const () as u64,
            stack as u64 + STACK as u64,
            (&raw mut THREAD_IMAGE) as u64,
            stack as u64,
        )
    };
    assert!(tid < 1_000_000, "thread_spawn refused: {tid:#x}");
    assert_eq!(thread_join(tid), 0, "thread_join failed");

    let img = unsafe { &*(&raw const THREAD_IMAGE) };
    assert_eq!(fcw(img), FCW_DECLARED, "a fresh thread inherited an x87 control word");
    assert_eq!(fsw(img), 0, "a fresh thread inherited an x87 status word");
    assert_eq!(img.0[OFF_FTW], 0, "a fresh thread inherited a non-empty x87 stack");
    assert_eq!(mxcsr(img), MXCSR_DECLARED, "a fresh thread inherited an MXCSR");
    assert!(
        registers(img).iter().all(|&b| b == 0),
        "a fresh thread inherited the previous tenant's x87 or XMM registers",
    );
}

/// Arm 2's victim: the waiting instruction the unwinder executes on every
/// panic, in a process that has never touched the FPU.
fn fldcw_survivor() {
    let cw = FCW_DECLARED;
    unsafe {
        core::arch::asm!("fldcw [{cw}]", "fwait", cw = in(reg) &cw, options(nostack));
    }
    println!("  FLDCW survived");
}

/// Arm 3. Everything from the pin to the capture is one instruction stream, so
/// nothing the compiler emits between them can touch the state under test.
///
/// **An unbracketed transition only corrupts if it switches**, and that is what
/// the sibling is for. Kernel code is soft-float, so a `#PF` that allocates a
/// page and returns disturbs nothing however unbracketed it is; what does the
/// damage is another task running Ring 3 code in between. A sibling that sleeps
/// in a short loop wakes several times inside a single 2 MiB fault — the fault
/// path runs with interrupts on and the measured cost of one is hundreds of
/// microseconds — so `need_resched` is set when `common_entry` reaches its exit
/// and the switch happens there rather than by luck.
fn preservation_arm() -> Result<(), String> {
    const SYSCALLS: u64 = 20_000;
    const SPIN: u64 = 2_000_000;
    /// Short against the fault's own cost, so several land inside one.
    const SIBLING_NAP_NS: u64 = 100_000;

    unsafe { ARENA.region = (&raw mut DEMAND) as u64 };

    let stop = Arc::new(AtomicBool::new(false));
    let sibling = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                fp_noise();
                nanosleep(SIBLING_NAP_NS);
            }
        })
    };

    unsafe {
        core::arch::asm!(
            pin_state!(),
            "fxsave64 [r15 + {before}]",

            // r13 is the counter and r14 the cursor: `clobber_abi` requires an
            // output to name its register, and these two have to outlive the
            // `syscall` that clobbers every caller-saved one.
            "mov r13, {nsys}",
            "2:",
            "mov rdi, {clock}",
            "syscall",
            "dec r13",
            "jnz 2b",

            // One write per 2 MiB page of a writable file-backed region, so
            // every iteration is exactly one #PF through `common_entry`.
            "mov r13, {npages}",
            "mov r14, [r15 + {region}]",
            "3:",
            "mov byte ptr [r14], 1",
            "add r14, {step}",
            "dec r13",
            "jnz 3b",

            // Long enough for the timer to preempt, several times over.
            "mov r13, {spin}",
            "4:",
            "dec r13",
            "jnz 4b",

            "fxsave64 [r15 + {after}]",
            // Leave the x87 stack as Rust expects to find it.
            "fninit",
            in("r15") &raw mut ARENA,
            mx = const offset_of!(Arena, mx),
            x = const offset_of!(Arena, xmm),
            region = const offset_of!(Arena, region),
            before = const offset_of!(Arena, before),
            after = const offset_of!(Arena, after),
            step = const PAGE_2M,
            npages = const FAULT_TOUCHES,
            nsys = const SYSCALLS,
            spin = const SPIN,
            clock = const SYS_CLOCK,
            out("r13") _,
            out("r14") _,
            clobber_abi("sysv64"),
        );
    }

    stop.store(true, Ordering::Relaxed);
    sibling.join().expect("the sibling thread died");

    let before = unsafe { &*(&raw const ARENA.before) };
    let after = unsafe { &*(&raw const ARENA.after) };
    let what = format!(
        "{SYSCALLS} syscalls, {FAULT_TOUCHES} page faults and a preemption spin"
    );
    require(fcw(after) == fcw(before), || {
        format!(
            "preservation: the x87 control word did not survive {what} — {:#06x} became {:#06x}",
            fcw(before),
            fcw(after),
        )
    })?;
    require(fsw(after) == fsw(before), || {
        format!("preservation: the x87 status word did not survive {what}")
    })?;
    require(after.0[OFF_FTW] == before.0[OFF_FTW], || {
        format!("preservation: the x87 tag word did not survive {what}")
    })?;
    require(mxcsr(after) == mxcsr(before), || {
        format!(
            "preservation: MXCSR did not survive {what} — {:#010x} became {:#010x}",
            mxcsr(before),
            mxcsr(after),
        )
    })?;
    let differing =
        registers(before).iter().zip(registers(after)).filter(|(a, b)| a != b).count();
    require(differing == 0, || {
        format!(
            "preservation: {differing} of {} register bytes changed across {what}",
            registers(before).len(),
        )
    })?;
    println!("  the whole state survived {what}");
    Ok(())
}

/// What the sibling does: dirty every kind of FP register there is.
fn fp_noise() {
    unsafe {
        core::arch::asm!(
            "fninit",
            "fldpi",
            "fldl2t",
            "fldln2",
            "movdqu xmm0,  [r15 + {x} + 0*16]",
            "movdqu xmm3,  [r15 + {x} + 1*16]",
            "movdqu xmm7,  [r15 + {x} + 2*16]",
            "movdqu xmm11, [r15 + {x} + 3*16]",
            "movdqu xmm15, [r15 + {x} + 4*16]",
            in("r15") &raw const ARENA,
            x = const offset_of!(Arena, xmm),
            clobber_abi("sysv64"),
        );
    }
}
