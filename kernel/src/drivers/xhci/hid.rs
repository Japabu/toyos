use core::ptr::copy_nonoverlapping;
use core::sync::atomic::{fence, Ordering};

use crate::{keyboard, mouse};
use super::{Mmio, Trb, TrbRing, TRB_NORMAL};

/// What a configuration descriptor's HID interface said it was. Parse-time
/// only: the three differ in report size and in whether SET_PROTOCOL applies,
/// and in nothing a bound device does.
#[derive(Clone, Copy, PartialEq)]
pub enum HidType {
    Keyboard,
    Mouse,
    Tablet,
}

/// What a *bound* device is, which is a coarser question — the two pointer
/// kinds dispatch identically, and `mouse::handle_report` tells them apart by
/// report length. The source is carried rather than derived from the slot id,
/// which is per controller and therefore not a machine-wide name for a device.
#[derive(Clone, Copy)]
pub enum HidRole {
    Keyboard,
    Pointer(mouse::PointerSource),
}

pub struct HidDevice {
    pub slot_id: u8,
    pub int_ep_dci: u8,
    pub int_ring: TrbRing,
    pub report_phys: u64,
    pub report_ptr: *mut u8,
    pub report_size: u32,
    pub role: HidRole,
    /// This keyboard's last report. Per device, because a report is a snapshot
    /// of one keyboard and diffing it against another's synthesizes releases
    /// for keys that are still physically down.
    pub prev_report: [u8; 8],
}

impl HidDevice {
    pub fn dispatch_report(&mut self) {
        let mut buf = [0u8; 8];
        let size = self.report_size as usize;
        unsafe { copy_nonoverlapping(self.report_ptr as *const u8, buf.as_mut_ptr(), size); }
        // Wake only when the decode actually queued something. A report
        // identical to the last one produces no event, and waking watchers
        // for it made readiness disagree with `has_data()` — which froze the
        // compositor for as long as a key was held.
        match self.role {
            HidRole::Keyboard => {
                if keyboard::handle_report(&mut self.prev_report, &buf[..size]) == 0 {
                    return;
                }
                keyboard::wake_waiters();
                let watchers = keyboard::io_uring_watchers();
                if !watchers.is_empty() {
                    crate::io_uring::complete_pending_for_event(
                        &watchers,
                        crate::io_uring::Source::Keyboard,
                    );
                }
            }
            HidRole::Pointer(source) => {
                if mouse::handle_report(source, &buf[..size]) == 0 {
                    return;
                }
                mouse::wake_waiters();
                let watchers = mouse::io_uring_watchers();
                if !watchers.is_empty() {
                    crate::io_uring::complete_pending_for_event(
                        &watchers,
                        crate::io_uring::Source::Mouse,
                    );
                }
            }
        }
    }

    pub fn requeue(&mut self, db_base: &Mmio) {
        let mut trb = Trb::ZERO;
        trb.param = self.report_phys;
        trb.status = self.report_size;
        trb.control = TRB_NORMAL | (1 << 5); // IOC
        self.int_ring.enqueue(trb);
        fence(Ordering::Release);
        db_base.write_u32(self.slot_id as u64 * 4, self.int_ep_dci as u32);
    }
}
