use core::arch::asm;

#[repr(C, align(16))]
struct Gdt {
    entries: [u64; 3],
}

static GDT: Gdt = Gdt {
    entries: [
        0x0000_0000_0000_0000, // null
        0x00AF_9A00_0000_FFFF, // 64-bit code: G=1, L=1, P=1, DPL=0, Execute/Read
        0x00CF_9200_0000_FFFF, // 64-bit data: G=1, D=1, P=1, DPL=0, Read/Write
    ],
};

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_DS: u16 = 0x10;

pub fn init() {
    let ptr = GdtPointer {
        limit: (core::mem::size_of_val(&GDT.entries) - 1) as u16,
        base: GDT.entries.as_ptr() as u64,
    };

    unsafe {
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
    }
}
