use core::arch::asm;

#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nomem, nostack));
    }
    (high as u64) << 32 | low as u64
}

#[inline]
pub fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high, options(nomem, nostack));
    }
}

#[inline]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    (hi as u64) << 32 | lo as u64
}

#[inline]
pub fn rdrand() -> u64 {
    let val: u64;
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
    unsafe { asm!("mov {}, cr0", out(reg) value, options(nomem, nostack)); }
    value
}

#[inline]
pub fn read_cr4() -> u64 {
    let value: u64;
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
    unsafe { asm!("clac", options(nomem, nostack)); }
}

#[inline]
pub fn read_cr2() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack)); }
    value
}

#[inline]
pub fn read_cr3() -> u64 {
    let value: u64;
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack)); }
    value
}

/// # Safety
/// The caller must ensure the value is a valid CR3.
#[inline]
pub unsafe fn write_cr3(value: u64) {
    asm!("mov cr3, {}", in(reg) value, options(nostack));
}

#[inline]
pub fn invlpg(addr: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack)); }
}

/// INVPCID — invalidate TLB entries by type.
/// Type 0: single (pcid, addr). Type 1: all for pcid. Type 2: all PCIDs.
#[inline]
pub fn invpcid(kind: u64, pcid: u64, addr: u64) {
    let desc: [u64; 2] = [pcid, addr];
    unsafe {
        asm!(
            "invpcid {0}, [{1}]",
            in(reg) kind,
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
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

#[inline]
pub fn disable_interrupts() {
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}

#[inline]
pub fn rdfsbase() -> u64 {
    let val: u64;
    unsafe {
        asm!("rdfsbase {}", out(reg) val, options(nomem, nostack));
    }
    val
}

#[inline]
pub fn wrfsbase(val: u64) {
    unsafe {
        asm!("wrfsbase {}", in(reg) val, options(nomem, nostack));
    }
}

pub fn halt() -> ! {
    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}

#[inline]
pub fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value);
    }
}

#[inline]
pub fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port);
    }
    value
}

#[inline]
pub fn outw(port: u16, value: u16) {
    unsafe {
        asm!("out dx, ax", in("dx") port, in("ax") value);
    }
}

#[inline]
pub fn io_wait() {
    outb(0x80, 0);
}
