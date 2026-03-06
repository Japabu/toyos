pub mod apic;
pub mod cpu;
pub mod gdt;
pub mod idt;
pub mod paging;
pub mod percpu;
pub mod smp;
pub mod syscall;

const IA32_FS_BASE: u32 = 0xC000_0100;

pub fn read_fs_base() -> u64 {
    cpu::rdmsr(IA32_FS_BASE)
}

pub fn set_fs_base(value: u64) {
    cpu::wrmsr(IA32_FS_BASE, value);
}
