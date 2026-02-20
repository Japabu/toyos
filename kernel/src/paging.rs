use core::arch::asm;

const PAGE_PRESENT: u64 = 1 << 0;
const PAGE_WRITE: u64 = 1 << 1;
const PAGE_USER: u64 = 1 << 2;
const PAGE_SIZE: u64 = 1 << 7; // PS bit (1GB or 2MB page)

/// Set the User/Supervisor bit on ALL present page table entries,
/// allowing ring 3 code to access all mapped memory.
pub fn set_all_user_accessible() {
    let cr3: u64;
    unsafe {
        asm!("mov {}, cr3", out(reg) cr3);

        // Clear CR0.WP (bit 16) so ring 0 can write to read-only pages.
        // UEFI maps page table structures as read-only; we need to modify them.
        let cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0);
        asm!("mov cr0, {}", in(reg) cr0 & !(1u64 << 16));
    }

    let pml4 = (cr3 & 0x000F_FFFF_FFFF_F000) as *mut u64;

    unsafe {
        for i in 0..512 {
            let pml4e = pml4.add(i).read_volatile();
            if pml4e & PAGE_PRESENT == 0 { continue; }
            pml4.add(i).write_volatile(pml4e | PAGE_USER | PAGE_WRITE);

            let pdpt = (pml4e & 0x000F_FFFF_FFFF_F000) as *mut u64;
            for j in 0..512 {
                let pdpte = pdpt.add(j).read_volatile();
                if pdpte & PAGE_PRESENT == 0 { continue; }
                pdpt.add(j).write_volatile(pdpte | PAGE_USER | PAGE_WRITE);
                if pdpte & PAGE_SIZE != 0 { continue; } // 1GB huge page

                let pd = (pdpte & 0x000F_FFFF_FFFF_F000) as *mut u64;
                for k in 0..512 {
                    let pde = pd.add(k).read_volatile();
                    if pde & PAGE_PRESENT == 0 { continue; }
                    pd.add(k).write_volatile(pde | PAGE_USER | PAGE_WRITE);
                    if pde & PAGE_SIZE != 0 { continue; } // 2MB large page

                    let pt = (pde & 0x000F_FFFF_FFFF_F000) as *mut u64;
                    for l in 0..512 {
                        let pte = pt.add(l).read_volatile();
                        if pte & PAGE_PRESENT == 0 { continue; }
                        pt.add(l).write_volatile(pte | PAGE_USER | PAGE_WRITE);
                    }
                }
            }
        }

        // Flush TLB
        asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _);
    }
}
