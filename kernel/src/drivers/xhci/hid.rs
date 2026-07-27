use core::ptr::copy_nonoverlapping;
use core::sync::atomic::{fence, Ordering};

use crate::{keyboard, mouse};
use super::{Mmio, Trb, TrbRing, TRB_NORMAL};

#[derive(Clone, Copy, PartialEq)]
pub enum HidType {
    Keyboard,
    Mouse,
    Tablet,
}

pub struct HidDevice {
    pub slot_id: u8,
    pub int_ep_dci: u8,
    pub int_ring: TrbRing,
    pub report_phys: u64,
    pub report_ptr: *mut u8,
    pub report_size: u32,
    pub hid_type: HidType,
}

impl HidDevice {
    pub fn dispatch_report(&self) {
        let mut buf = [0u8; 8];
        let size = self.report_size as usize;
        unsafe { copy_nonoverlapping(self.report_ptr as *const u8, buf.as_mut_ptr(), size); }
        match self.hid_type {
            HidType::Keyboard => {
                keyboard::handle_report(&buf[..size]);
                crate::scheduler::push_event(crate::scheduler::EventSource::Keyboard);
                let watchers = keyboard::io_uring_watchers();
                if !watchers.is_empty() {
                    crate::io_uring::complete_pending_for_event(
                        &watchers,
                        crate::scheduler::EventSource::Keyboard,
                    );
                }
            }
            HidType::Mouse | HidType::Tablet => {
                mouse::handle_report(&buf[..size]);
                crate::scheduler::push_event(crate::scheduler::EventSource::Mouse);
                let watchers = mouse::io_uring_watchers();
                if !watchers.is_empty() {
                    crate::io_uring::complete_pending_for_event(
                        &watchers,
                        crate::scheduler::EventSource::Mouse,
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
