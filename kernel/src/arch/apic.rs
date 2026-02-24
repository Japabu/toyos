use crate::arch::paging;
use crate::drivers::mmio::Mmio;
use crate::log;
use crate::sync::Lock;

// Local APIC register offsets
const LAPIC_ID: u64 = 0x020;
const LAPIC_SVR: u64 = 0x0F0;
const LAPIC_ICR_LOW: u64 = 0x300;
const LAPIC_ICR_HIGH: u64 = 0x310;
const LAPIC_EOI: u64 = 0x0B0;

static LAPIC: Lock<Option<Mmio>> = Lock::new(None);

/// Initialize the BSP's Local APIC at the given physical address.
pub fn init(base_addr: u32) {
    let addr = base_addr as u64;
    paging::map_kernel(addr, 0x1000);
    let lapic = Mmio::new(addr);

    // Enable LAPIC: set SVR bit 8 (software enable) + spurious vector 0xFF
    lapic.write_u32(LAPIC_SVR, lapic.read_u32(LAPIC_SVR) | (1 << 8) | 0xFF);

    *LAPIC.get_mut() = Some(lapic);
    log!("LAPIC: enabled (ID {})", id());
}

fn lapic() -> Mmio {
    LAPIC.get().expect("LAPIC not initialized")
}

pub fn id() -> u8 {
    (lapic().read_u32(LAPIC_ID) >> 24) as u8
}

/// Send INIT IPI to the specified APIC ID.
pub fn send_init(apic_id: u8) {
    let l = lapic();
    l.write_u32(LAPIC_ICR_HIGH, (apic_id as u32) << 24);
    l.write_u32(LAPIC_ICR_LOW, 0x4500); // delivery=INIT, level=assert
}

/// Send Startup IPI (SIPI) with the given vector (trampoline page number).
pub fn send_sipi(apic_id: u8, vector: u8) {
    let l = lapic();
    l.write_u32(LAPIC_ICR_HIGH, (apic_id as u32) << 24);
    l.write_u32(LAPIC_ICR_LOW, 0x4600 | vector as u32); // delivery=Startup
}

pub fn eoi() {
    lapic().write_u32(LAPIC_EOI, 0);
}
