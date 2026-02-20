use core::arch::asm;
use core::mem::size_of;

// 64-bit TSS (104 bytes)
#[repr(C, packed)]
pub struct Tss {
    reserved0: u32,
    pub rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iopb_offset: u16,
}

static mut TSS: Tss = Tss {
    reserved0: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iopb_offset: size_of::<Tss>() as u16,
};

#[repr(C, align(16))]
struct Gdt {
    entries: [u64; 7],
}

// GDT layout:
//   0x00: null
//   0x08: kernel code64 (DPL=0)
//   0x10: kernel data   (DPL=0)
//   0x18: user data     (DPL=3)
//   0x20: user code64   (DPL=3)
//   0x28: TSS low       (filled at runtime)
//   0x30: TSS high      (filled at runtime)
static mut GDT: Gdt = Gdt {
    entries: [
        0x0000_0000_0000_0000, // null
        0x00AF_9A00_0000_FFFF, // kernel code64: G=1, L=1, P=1, DPL=0, Execute/Read
        0x00CF_9200_0000_FFFF, // kernel data:   G=1, D=1, P=1, DPL=0, Read/Write
        0x00CF_F200_0000_FFFF, // user data:     G=1, D=1, P=1, DPL=3, Read/Write
        0x00AF_FA00_0000_FFFF, // user code64:   G=1, L=1, P=1, DPL=3, Execute/Read
        0,                      // TSS low (runtime)
        0,                      // TSS high (runtime)
    ],
};

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;

const TSS_SEL: u16 = 0x28;

pub fn init() {
    unsafe {
        // Set TSS.RSP0 to current kernel stack (for interrupts from ring 3)
        let rsp: u64;
        asm!("mov {}, rsp", out(reg) rsp);
        core::ptr::write_unaligned(&raw mut TSS.rsp0, rsp);

        // Build 16-byte TSS descriptor
        let tss_addr = &raw const TSS as u64;
        let tss_limit = (size_of::<Tss>() - 1) as u64;

        let low = (tss_limit & 0xFFFF)
            | ((tss_addr & 0xFFFF) << 16)
            | (((tss_addr >> 16) & 0xFF) << 32)
            | (0x89u64 << 40) // P=1, DPL=0, Type=0x9 (64-bit TSS Available)
            | (((tss_limit >> 16) & 0xF) << 48)
            | (((tss_addr >> 24) & 0xFF) << 56);
        let high = tss_addr >> 32;

        GDT.entries[5] = low;
        GDT.entries[6] = high;

        let ptr = GdtPointer {
            limit: (size_of::<[u64; 7]>() - 1) as u16,
            base: (&raw const GDT.entries) as *const u64 as u64,
        };

        asm!(
            "lgdt [{}]",
            // Reload CS via far return
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            // Reload data segment registers
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov fs, {ds:x}",
            "mov gs, {ds:x}",
            "mov ss, {ds:x}",
            in(reg) &ptr,
            cs = in(reg) KERNEL_CS as u64,
            ds = in(reg) KERNEL_DS as u64,
            tmp = lateout(reg) _,
        );

        // Load TSS
        asm!("ltr {0:x}", in(reg) TSS_SEL as u64);
    }
}

/// Returns a pointer to TSS.RSP0 so execute() can update it before entering ring 3.
pub fn tss_rsp0_ptr() -> *mut u64 {
    unsafe { &raw mut TSS.rsp0 as *mut u64 }
}
