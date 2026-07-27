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

/// Enable SSE/SSE2+ by setting CR4.OSFXSR and CR4.OSXMMEXCPT.
/// Must be called on each CPU before any SSE instructions execute.
pub fn enable_sse() {
    unsafe {
        asm!(
            "mov {0}, cr4",
            "or {0}, 0x600",   // bit 9 = OSFXSR, bit 10 = OSXMMEXCPT
            "mov cr4, {0}",
            out(reg) _,
            options(nostack),
        );
    }
}

/// Enable SMEP (Supervisor Mode Execution Prevention).
/// When enabled, the kernel cannot execute code on user-accessible pages.
/// Must be called on each CPU during init.
pub fn enable_smep() {
    // Check CPUID leaf 7, subleaf 0, EBX bit 7
    let ebx: u32;
    unsafe {
        asm!(
            "push rbx",
            "mov eax, 7",
            "xor ecx, ecx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
            options(nomem),
        );
    }
    if ebx & (1 << 7) == 0 {
        crate::log!("cpu: SMEP not supported, skipping");
        return;
    }
    unsafe {
        asm!(
            "mov {0}, cr4",
            "or {0}, 1 << 20",  // CR4.SMEP
            "mov cr4, {0}",
            out(reg) _,
            options(nostack),
        );
    }
    crate::log!("cpu: SMEP enabled");
}

/// Enable SMAP (Supervisor Mode Access Prevention).
/// When enabled, kernel code cannot access user pages unless RFLAGS.AC=1 (set by STAC).
/// Must be called on each CPU during init.
pub fn enable_smap() {
    // Check CPUID leaf 7, subleaf 0, EBX bit 20
    // rbx cannot be used as an inline asm operand in Rust, so save/restore manually.
    let ebx: u32;
    unsafe {
        asm!(
            "push rbx",
            "mov eax, 7",
            "xor ecx, ecx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
            options(nomem),
        );
    }
    if ebx & (1 << 20) == 0 {
        crate::log!("cpu: SMAP not supported, skipping");
        return;
    }
    unsafe {
        asm!(
            "mov {0}, cr4",
            "or {0}, 1 << 21",  // CR4.SMAP
            "mov cr4, {0}",
            "clac",             // clear AC — kernel cannot access user pages by default
            out(reg) _,
            options(nostack),
        );
    }
    crate::log!("cpu: SMAP enabled");
}

/// Enable FSGSBASE instructions (rdfsbase, rdgsbase, wrfsbase, wrgsbase).
/// Must be called on each CPU during init, before any FSGSBASE instruction is used.
pub fn enable_fsgsbase() {
    unsafe {
        asm!(
            "mov {0}, cr4",
            "or {0}, 1 << 16",  // CR4.FSGSBASE
            "mov cr4, {0}",
            out(reg) _,
            options(nostack),
        );
    }
}

/// Enable PCID + INVPCID if both are supported. Returns true if enabled.
/// PCID without INVPCID is not useful — we need INVPCID for targeted flushes.
/// Must be called on each CPU. CR3 must have PCID 0 when called.
pub fn enable_pcid() -> bool {
    // CPUID leaf 1, ECX bit 17 = PCID
    let ecx: u32;
    unsafe {
        asm!(
            "push rbx",
            "mov eax, 1",
            "cpuid",
            "mov {0:e}, ecx",
            "pop rbx",
            out(reg) ecx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
            options(nomem),
        );
    }
    if ecx & (1 << 17) == 0 { return false; }

    // CPUID leaf 7, subleaf 0, EBX bit 10 = INVPCID
    let ebx: u32;
    unsafe {
        asm!(
            "push rbx",
            "mov eax, 7",
            "xor ecx, ecx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
            options(nomem),
        );
    }
    if ebx & (1 << 10) == 0 { return false; }

    unsafe {
        asm!(
            "mov {0}, cr4",
            "or {0}, 1 << 17",  // CR4.PCIDE
            "mov cr4, {0}",
            out(reg) _,
            options(nostack),
        );
    }
    true
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

// --- Port I/O ---

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
