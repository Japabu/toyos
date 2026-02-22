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
    let cr2: u64;
    unsafe {
        asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
    }
    cr2
}

/// # Safety
/// The caller must ensure the value is a valid PML4 physical address.
#[inline]
pub unsafe fn write_cr3(value: u64) {
    asm!("mov cr3, {}", in(reg) value, options(nostack));
}

#[inline]
pub fn flush_tlb() {
    unsafe {
        asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _, options(nostack));
    }
}

/// # Safety
/// The pointer must reference a valid GDT descriptor.
#[inline]
pub unsafe fn lgdt(ptr: *const u8) {
    asm!("lgdt [{}]", in(reg) ptr, options(nostack));
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

pub fn halt() -> ! {
    loop {
        unsafe {
            asm!("cli; hlt", options(nomem, nostack));
        }
    }
}
