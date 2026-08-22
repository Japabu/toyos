//! One instruction each: the bottom of this kernel's x86-64 machine access.
//!
//! **Every `unsafe` block in this file is irreducible in the same way, and it is
//! the only reason they are all still here.** Each is one instruction that Rust
//! has no operation for — an MSR, a control register, a port, a TLB entry, a
//! flag — so there is no lower layer to push it into and no safe spelling to
//! prefer. What each block's own comment adds is the *instruction's* own
//! requirement: what makes it fault, and which half of that the function
//! signature above it discharges.
//!
//! The split between `pub fn` and `pub unsafe fn` here is that second half. A
//! function is `unsafe` when the caller can choose a value that breaks the
//! machine — `write_cr0`, `write_cr4`, `write_cr3`, `lidt`, `ltr`, `wbinvd`,
//! `wrmsr`, `outb`, `outw`, `wrfsbase` — and safe when it cannot.
//!
//! **Two wrappers take a caller-chosen value and are safe anyway, and each
//! argues it in its own doc comment rather than in a `SAFETY:` block.**
//! [`invlpg`] and [`invpcid`] *discard* translations, which is the direction
//! that cannot make an access unsound; keeping a stale entry is what would.
//! `invpcid`'s two faults are removed rather than argued away — the `#GP` on a
//! descriptor type above 3 is unrepresentable ([`Invpcid`]) and the `#UD` on a
//! CPU without the feature is an argument the caller can only get by asking
//! ([`PcidActive`]). [`inb`] is safe because a read has no value for a caller to
//! get wrong.
//!
//! **Where an `unsafe fn` here has a closed set of callers, the honest form is
//! one safe wrapper that discharges the choice, not an `unsafe` block apiece.**
//! `arch::apic::Reg` is that for the eighteen x2APIC `wrmsr` calls, and
//! `mm::paging::activate_kernel` is the pattern it follows. Where the set is not
//! closed — a port a device's firmware description names, an MSR that *is* the
//! machine's one declaration — the block sits at the call and its `SAFETY:`
//! says why that value is the right one.

use core::arch::asm;

use super::control_regs::PcidActive;

#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: `rdmsr` reads no memory and writes only `eax`/`edx`, which are
    // declared as outputs. It is `#GP` on an MSR this CPU does not implement,
    // and that is a property of `msr` the caller chose — every caller in this
    // tree names an architectural MSR constant, and `control_regs` asks CPUID
    // before touching the ones that are optional.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack));
    }
    (high as u64) << 32 | low as u64
}

/// # Safety
/// Both operands reach the CPU unchecked, and the `#GP` on an unimplemented MSR
/// or a reserved-bit value is the least of what the caller owns. `IA32_LSTAR` is
/// `SYSCALL`'s only entry point; `IA32_GS_BASE` is where every `gs:` access in
/// this kernel lands; `IA32_EFER`'s `NXE` decides whether bit 63 of every live
/// paging entry is a permission or a reserved bit, so clearing it makes W^X
/// silently not exist. The caller answers for which register it is writing and
/// for what the machine holds afterwards.
#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack));
}

#[inline]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: reads the time-stamp counter into the two declared outputs and
    // touches nothing else. `CR4.TSD` is clear in `control_regs`'s declaration,
    // so it is not even privileged.
    unsafe {
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    (hi as u64) << 32 | lo as u64
}

#[inline]
pub fn rdrand() -> u64 {
    let val: u64;
    // SAFETY: `rdrand` writes one register and CF; the loop retries until CF is
    // set, which is the architecture's "the value is good". No memory operand,
    // and no caller-chosen value at all.
    unsafe {
        asm!(
            "2: rdrand {val}",
            "jnc 2b",
            val = out(reg) val,
            options(nomem, nostack),
        );
    }
    val
}

#[inline]
pub fn read_rsp() -> u64 {
    let rsp: u64;
    // SAFETY: a register-to-register `mov` into the declared output. `nostack`
    // holds because it reads `rsp` rather than using it.
    unsafe {
        asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack));
    }
    rsp
}

/// The direction flag, asked at a place in Ring 0 where it must be clear.
///
/// **The instrument for the one kernel-wide stray writer this tree has had.**
/// No gate clears `DF` — an interrupt or trap gate clears `TF`, `NT`, `RF`, `VM`
/// and `IF`, and `SYSCALL` clears what `IA32_FMASK` names — while
/// `compiler_builtins::mem::memmove` sets it across three `rep` string operations
/// for every overlapping copy the kernel makes. Every `memcpy`, `memset` and
/// forward `memmove` executed while it is set writes the `n` bytes *below* its
/// destination instead of at it, which is a writer of real, pointer-shaped data
/// at addresses nothing meant to touch.
///
/// Its own build, because it is a `pushfq` and a test on the pass path, the
/// syscall path and the trap path, and a kernel carrying it is not the kernel a
/// rate was measured on. It reads and decides nothing.
#[cfg(feature = "df-witness")]
pub fn direction_flag_set() -> bool {
    let rflags: u64;
    // SAFETY: a push and a matching pop, balanced, so `rsp` is where it was.
    // Deliberately no `nomem` — the pair uses the stack — which is also why
    // there is no `nostack`.
    unsafe {
        asm!("pushfq", "pop {}", out(reg) rflags, options(nomem));
    }
    rflags & 0x400 != 0
}

/// The panic [`direction_flag_set`] exists to raise, at the site that asked.
///
/// **It clears the flag before it says anything, and that is not tidiness.** The
/// first draft did not, and the control (`df-witness-mutate`, one `std` at the
/// head of a pass) never printed a word: both CPUs died at
/// `rip=0x0d8d480000e02c3d` with `rbp=0x8d48000e1f3215ff` — values whose bytes
/// are `48 8d 0d` and `ff 15`, x86 instruction encodings, which is kernel *text*
/// copied into a stack by the report's own formatting running backwards. A
/// reporter that runs with the flag set destroys its report, and the wreckage is
/// the same shape as the class it is reporting on.
#[cfg(feature = "df-witness")]
#[cold]
#[inline(never)]
pub fn df_witness(site: &str) {
    if !direction_flag_set() {
        return;
    }
    // SAFETY: the observation is already made; everything below this line is
    // `core::fmt` and the log, which the ABI says may not run with it set.
    unsafe { asm!("cld", options(nomem, nostack)) };
    crate::hw::report_contexts(read_rsp(), None);
    panic!(
        "DF WITNESS: cpu{} reached {site} with the direction flag set. \
         `compiler_builtins::mem::memmove`'s overlapping-copy path holds it across \
         `rep movsb`/`rep movsq`/`rep movsb` with interrupts enabled, and it is the one \
         `std` a linear disassembly of this kernel's `.text` puts on an executable path; \
         every `rep movs`/`rep stos` reached from here writes backwards.",
        crate::arch::percpu::cpu_id(),
    );
}

/// CPUID with both index registers, `rbx` saved by hand because Rust reserves
/// it as a general operand.
pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    // SAFETY: `cpuid` faults for no input — an unsupported leaf answers with the
    // highest basic leaf's registers, which every caller here guards against by
    // asking leaf 0 first. The `push rbx`/`pop rbx` pair is balanced, so the
    // register LLVM reserves is returned unchanged; `nomem` is honest because
    // the stack slot is written and read inside the block.
    unsafe {
        asm!(
            "push rbx",
            "cpuid",
            "mov {ebx:e}, ebx",
            "pop rbx",
            ebx = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            options(nomem),
        );
    }
    (eax, ebx, ecx, edx)
}

#[inline]
pub fn read_cr0() -> u64 {
    let value: u64;
    // SAFETY: reading a control register in Ring 0 touches no memory and cannot
    // fault; the kernel is never anywhere else.
    unsafe { asm!("mov {}, cr0", out(reg) value, options(nomem, nostack)); }
    value
}

#[inline]
pub fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: `read_cr0`'s argument.
    unsafe { asm!("mov {}, cr4", out(reg) value, options(nomem, nostack)); }
    value
}

/// # Safety
/// Every bit of CR0 is written, so the caller owns the whole machine
/// configuration rather than one flag of it —
/// [`control_regs`](super::control_regs) is where that decision lives.
#[inline]
pub unsafe fn write_cr0(value: u64) {
    asm!("mov cr0, {}", in(reg) value, options(nostack));
}

/// # Safety
/// A bit this CPU does not define is `#GP`, and clearing `PAE` or `LA57` in long
/// mode is `#GP` too. So is taking `PCIDE` from 0 to 1 while `CR3[11:0]` is
/// non-zero (SDM Vol. 3A §4.10.1). [`control_regs`](super::control_regs) is the
/// only caller: it asks CPUID first, and both of its call sites run on the
/// kernel address space, whose PCID is 0.
#[inline]
pub unsafe fn write_cr4(value: u64) {
    asm!("mov cr4, {}", in(reg) value, options(nostack));
}

/// # Safety
/// Writes back and invalidates every cache level. Only correct inside a no-fill
/// window — `CR0.CD` set and `CR0.NW` clear — which is SDM Vol. 3A §11.5.3 for a
/// plain cache-mode change and §11.11.8's MTRR procedure for the PAT write
/// [`pat::init`](super::pat::init) wraps in one.
#[inline]
pub unsafe fn wbinvd() {
    asm!("wbinvd", options(nostack, preserves_flags));
}

/// Clear `RFLAGS.AC`, so a supervisor access to a user page faults under SMAP.
///
/// `#UD` on a CPU without SMAP.
#[inline]
pub fn clac() {
    // SAFETY: clears one `RFLAGS` bit and touches no memory. `#UD` on a CPU
    // without SMAP, which is why the one caller (`control_regs::init`) reaches
    // it only after CPUID reported SMAP and `CR4.SMAP` was actually set.
    unsafe { asm!("clac", options(nomem, nostack)); }
}

#[inline]
pub fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: `read_cr0`'s argument. The *value* is meaningful only on a `#PF`,
    // which is `arch::idt`'s problem and not this instruction's.
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack)); }
    value
}

#[inline]
pub fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: `read_cr0`'s argument.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack)); }
    value
}

/// # Safety
/// The caller must ensure the value is a valid CR3.
#[inline]
pub unsafe fn write_cr3(value: u64) {
    asm!("mov cr3, {}", in(reg) value, options(nostack));
}

/// Discard this CPU's translation for one linear address.
///
/// **Safe with an argument nothing checks, and the reason is the direction.**
/// `invlpg` takes the address as a *tag* and never dereferences it: an unmapped
/// or non-canonical one invalidates nothing and does not fault. Discarding a
/// translation cannot make a memory access unsound — keeping a stale one is what
/// would — so there is no value of `addr` a caller can get wrong in the
/// direction this module's `unsafe` exists for.
#[inline]
pub fn invlpg(addr: u64) {
    // SAFETY: one instruction with no memory operand the compiler can see. The
    // doc comment above is the whole argument and it is why this wrapper is
    // safe; irreducible because Rust has no operation for a TLB entry.
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack)); }
}

/// What one [`invpcid`] discards — the descriptor type, SDM Vol. 3A §4.10.4.1.
///
/// **A type and not a number, because `INVPCID` is `#GP` on a type above 3.**
/// That is the one way a caller could break the instruction, and here it is
/// unrepresentable rather than checked.
///
/// Three variants and not four: type 3 — every PCID *except* the global entries
/// — has no issuer in this kernel, because there are no global entries to except
/// (`PAGE_GLOBAL` appears nowhere, which `mm::paging`'s header states). This is
/// the closed set of what this kernel discards, and a fourth variant nothing
/// constructs would be dead code with a discriminant.
#[derive(Clone, Copy)]
#[repr(u64)]
pub enum Invpcid {
    /// One linear address, in one PCID.
    Address = 0,
    /// Every entry tagged with one PCID, global pages excepted.
    SinglePcid = 1,
    /// Every entry in every PCID, global pages included.
    AllIncludingGlobal = 2,
}

/// Discard TLB entries by descriptor type.
///
/// **Safe for [`invlpg`]'s reason, and it takes two removals to be so.** The
/// `#GP` on a descriptor type above 3 is gone by construction ([`Invpcid`]).
/// The `#UD` on a CPU without the feature is not a value a caller passes — it is
/// a fact about the machine — so this takes the answer to that question as an
/// argument ([`PcidActive`]) rather than a comment asking every caller to have
/// asked it. Unlike `rdfsbase`'s `#UD`, it cannot be discharged by pointing at
/// `CR4_REQUIRED`: `PCIDE` is in `CR4_OPTIONAL`.
///
/// A panic here would be the wrong refusal even so — `flush_tlb_all` is reached
/// from vector 0xFE's handler and from `Lock::lock`'s spin, which `arch::tlb`'s
/// header requires to take no lock and to be safe from anywhere — so the
/// impossible call has no spelling instead of a message.
#[inline]
pub fn invpcid(_have: PcidActive, kind: Invpcid, pcid: u16, addr: u64) {
    let desc: [u64; 2] = [pcid as u64, addr];
    // SAFETY: the descriptor operand is the local `desc` — sixteen bytes the CPU
    // reads and nothing writes, declared `readonly`. The instruction's two
    // faults are the doc comment's subject and both are already gone by the time
    // this line runs. Irreducible: Rust has no operation for a TLB entry.
    unsafe {
        asm!(
            "invpcid {0}, [{1}]",
            in(reg) kind as u64,
            in(reg) desc.as_ptr(),
            options(nostack, readonly),
        );
    }
}

/// # Safety
/// The pointer must reference a valid IDT descriptor.
#[inline]
pub unsafe fn lidt(ptr: *const u8) {
    asm!("lidt [{}]", in(reg) ptr, options(nostack));
}

/// # Safety
/// The selector must reference a valid TSS entry in the GDT.
#[inline]
pub unsafe fn ltr(selector: u16) {
    asm!("ltr {:x}", in(reg) selector as u64, options(nostack));
}

#[inline]
pub fn enable_interrupts() {
    // SAFETY: one `RFLAGS` bit, no memory. Whether IF *should* be set here is
    // the caller's — this is the instruction, and `hw::IrqGuard` is the type for
    // callers that want it restored rather than set.
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

#[inline]
pub fn disable_interrupts() {
    // SAFETY: `enable_interrupts`'s argument. Masking interrupts cannot make
    // memory access unsound; leaving them masked is a latency bug, not one.
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}

#[inline]
pub fn rdfsbase() -> u64 {
    let val: u64;
    // SAFETY: reads the FS base into the declared output. `#UD` without
    // `CR4.FSGSBASE`, which `control_regs` puts in `CR4_REQUIRED` and asserts
    // on every CPU before any context switch runs.
    unsafe {
        asm!("rdfsbase {}", out(reg) val, options(nomem, nostack));
    }
    val
}

/// # Safety
/// `val` becomes this CPU's FS base, and it is `#GP` if non-canonical. `#UD`
/// without `CR4.FSGSBASE` is `rdfsbase`'s argument and not the caller's:
/// `control_regs` puts the bit in `CR4_REQUIRED` and asserts it on every CPU
/// before any context switch runs. The kernel dereferences nothing through
/// `fs:`, so what the value decides is where the *running thread's* thread-local
/// accesses land — the caller owns which thread that is.
#[inline]
pub unsafe fn wrfsbase(val: u64) {
    asm!("wrfsbase {}", in(reg) val, options(nomem, nostack));
}

pub fn halt() -> ! {
    loop {
        // SAFETY: two instructions, no memory. Irreducible **by sequence**:
        // `cli` must precede `hlt` with no boundary between, or an interrupt
        // taken in the gap wakes a CPU this function promised never returns.
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}

/// # Safety
/// The instruction cannot fault in Ring 0 — `CR4.UMIP` does not cover port I/O
/// and there is no I/O permission bitmap, because `Tss::iopb_offset` is past the
/// segment limit — so what the caller owns is not a fault but a *device*. Any
/// legacy port is reachable, including ones that program a controller to write
/// memory, and the port and the byte together are the command. The caller
/// answers for which device answers at `port` and for what the byte tells it to
/// do.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    asm!("out dx, al", in("dx") port, in("al") value);
}

/// One byte from an I/O port.
///
/// **Safe, and the reason is that there is no value.** A read carries nothing a
/// caller can get wrong in [`outb`]'s direction: it commands no device and
/// writes no memory. A port whose read pops a queue — the 8042's data register
/// is one — still cares which port it is, and that is the driver's correctness
/// rather than the machine's soundness.
#[inline]
pub fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: one instruction into the declared output, no memory operand and no
    // fault in Ring 0 (`outb`'s `# Safety` carries why). The doc comment above
    // is why this direction needs no caller obligation.
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port);
    }
    value
}

/// # Safety
/// [`outb`]'s contract, sixteen bits wide.
#[inline]
pub unsafe fn outw(port: u16, value: u16) {
    asm!("out dx, ax", in("dx") port, in("ax") value);
}

/// One I/O bus cycle of delay, for a device that needs one between two commands.
#[inline]
pub fn io_wait() {
    // SAFETY: `outb` asks its caller to own the port and the byte. Port 0x80 is
    // the POST diagnostic port: nothing on a machine of this kernel's era
    // decodes it, which is what makes a write to it the architectural way to
    // spend a bus cycle rather than a command to anything. Zero is the value,
    // and no device reads it back.
    unsafe { outb(0x80, 0) };
}
