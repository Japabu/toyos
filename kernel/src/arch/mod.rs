pub mod apic;
pub mod cpu;
#[allow(dead_code)]
pub mod debug;
pub mod gdt;
pub mod idt;
pub mod percpu;
pub mod smp;
pub mod syscall;

const IA32_FS_BASE: u32 = 0xC000_0100;

pub fn read_fs_base() -> u64 {
    cpu::rdmsr(IA32_FS_BASE)
}
