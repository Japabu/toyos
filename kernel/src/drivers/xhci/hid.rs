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
    /// The root-hub port this device is on, which is what a disconnect names.
    /// The slot id cannot serve: the controller frees it when the slot is
    /// disabled, and the port is what the register reports about.
    pub port_idx: u8,
    /// This device's block in the DMA pool: where its interrupt ring and its
    /// EP0 ring live, and where the controller writes the output context whose
    /// Endpoint State field a recovery has to read.
    pub block: usize,
    pub int_ep_dci: u8,
    /// The endpoint address out of the device's own configuration descriptor.
    /// The DCI beside it is the *controller's* number for the same endpoint and
    /// means nothing to the device, so it is the address that goes in a
    /// CLEAR_FEATURE(ENDPOINT_HALT).
    pub ep_addr: u8,
    pub int_ring: TrbRing,
    /// The device's control ring, kept past enumeration for the same reason a
    /// mass-storage device keeps its: clearing a halt is a control transfer, so
    /// a bound HID is something the driver may still have to talk to.
    pub ep0_ring: TrbRing,
    /// The eight-byte DMA slot the interrupt endpoint delivers reports into.
    /// A [`crate::mm::Dma`] view and not a `*mut u8` beside its own physical
    /// address: it carries the length, so the two accesses below are bounded
    /// against the slot rather than against `report_size`'s own honesty.
    pub report: crate::mm::Dma<'static>,
    pub report_size: u32,
    pub role: HidRole,
    /// This keyboard's last report. Per device, because a report is a snapshot
    /// of one keyboard and diffing it against another's synthesizes releases
    /// for keys that are still physically down.
    pub prev_report: [u8; 8],
    /// The completion code this device's interrupt endpoint broke with, until
    /// something has restarted it. Read and cleared by
    /// [`XhciController::recover_endpoints`], never by the code that sets it —
    /// see that function for why the two cannot be the same place.
    ///
    /// [`XhciController::recover_endpoints`]: super::XhciController::recover_endpoints
    pub broke_with: Option<u32>,
    /// Transfers this endpoint has failed *in a row*. A delivered report
    /// clears it, so a device that glitches once an hour is never let go for
    /// it, and one that fails every transfer is let go on its own service
    /// interval — see [`super::MAX_HID_FAILURES`].
    pub failures: u8,
    /// Completions this endpoint has produced, which nothing but the injection
    /// below counts.
    // Counted in every build and compared in none but the test kernel's: the
    // `xhci-hid-break-*` arms are what read it, and a counter that only exists
    // when the actuator does would make the count itself a second code path.
    #[cfg_attr(not(feature = "boot-actuators"), allow(dead_code))]
    pub completions: u32,
}

impl HidDevice {
    /// What this device is called in every line about it.
    ///
    /// Two names and not the descriptor's three: a mouse and a tablet bind
    /// identically and `mouse::handle_report` tells them apart by report
    /// length, so a line saying "tablet" would be naming what the descriptor
    /// claimed rather than what the driver has.
    pub fn kind(&self) -> &'static str {
        match self.role {
            HidRole::Keyboard => "keyboard",
            HidRole::Pointer(_) => "pointer",
        }
    }

    pub fn dispatch_report(&mut self) {
        let mut buf = [0u8; 8];
        let size = self.report_size as usize;
        // Bounded twice: `copy_to` refuses `size > 8`, which is the slot
        // `bind_hid` allocated, and `report_size` is 4, 6 or 8 by the `match`
        // that set it. A copy and not a borrow into DMA memory; the transfer has
        // completed, since `dispatch_report` runs off a Transfer Event, and the
        // endpoint is not requeued until `requeue` below.
        self.report.copy_to(0, &mut buf[..size]);
        // Wake only when the decode actually queued something. A report
        // identical to the last one produces no event, and waking watchers
        // for it made readiness disagree with `has_data()` — which froze the
        // compositor for as long as a key was held.
        let queued = match self.role {
            HidRole::Keyboard => keyboard::handle_report(&mut self.prev_report, &buf[..size]) != 0,
            HidRole::Pointer(source) => mouse::handle_report(source, &buf[..size]) != 0,
        };
        if queued {
            self.wake();
        }
    }

    /// Release everything this device was holding, on its way off the bus.
    ///
    /// A zero *report* rather than `keyboard::release_all`: the held set is the
    /// union across every keyboard on the machine, and this device's own
    /// `prev_report` is the only record of which of those keys are its. A
    /// report holding nothing synthesizes exactly those releases, through the
    /// same merge every other report of this device took — so the keyboard
    /// beside it keeps the keys it is holding.
    ///
    /// The pointer half gives the button-table entry back as well, which is
    /// what makes a device that is plugged in again cost the machine nothing.
    pub fn unbind(&mut self) {
        let queued = match self.role {
            HidRole::Keyboard => keyboard::handle_report(&mut self.prev_report, &[0u8; 8]) != 0,
            HidRole::Pointer(source) => mouse::unbind(source),
        };
        if queued {
            self.wake();
        }
    }

    /// Wake whoever is waiting on this device's kind of event. Both halves of
    /// the pair, always: the queue a blocked `sys_read` parks on and the ring
    /// watchers `process_poll_add` registered, which nothing in the type
    /// system pairs.
    fn wake(&self) {
        let (watchers, source) = match self.role {
            HidRole::Keyboard => {
                keyboard::wake_waiters();
                (keyboard::inbox_watchers(), crate::inbox::Source::Keyboard)
            }
            HidRole::Pointer(_) => {
                mouse::wake_waiters();
                (mouse::inbox_watchers(), crate::inbox::Source::Mouse)
            }
        };
        if !watchers.is_empty() {
            crate::inbox::complete_pending_for_event(&watchers, source);
        }
    }

    pub fn requeue(&mut self, db_base: &Mmio) {
        let mut trb = Trb::ZERO;
        trb.param = self.report.phys();
        trb.status = self.report_size;
        trb.control = TRB_NORMAL | (1 << 5); // IOC
        self.int_ring.enqueue(trb);
        fence(Ordering::Release);
        db_base.write_u32(self.slot_id as u64 * 4, self.int_ep_dci as u32);
    }
}

/// Take one completion away from the device that earned it and hand the driver
/// a stall in its place, once per HID interrupt endpoint.
///
/// **A kernel feature because nothing on the host side can stage it.** QEMU's
/// `usb-hid` completes every interrupt TRB it is given: `usb_hid_handle_data`
/// answers an IN token on endpoint 1 with a report or with NAK and has no path
/// to `USB_RET_STALL` for it, and no device, machine or `-device` property adds
/// one. `device_add`/`device_del` cannot reach it either — an unplug is a
/// disconnect, which is a different event with a different recovery.
///
/// What is real is everything the recovery reads and everything it does: the
/// TRB was on the ring, the controller ran it, the transfer event is the
/// controller's own, the ring is left holding no TRB, the Endpoint State the
/// recovery branches on is read out of the controller's output context, and
/// every command it issues is really answered.
///
/// What is replaced is the completion code **and the report that transfer
/// delivered**, which is the half that keeps the gate from being vacuous: a
/// staged failure carries a real mouse movement into the report buffer, so a
/// driver that dispatched it anyway would publish a delta it never earned and
/// the gate would pass against the defect it is aimed at. Taking the bytes away
/// leaves what a failed transfer leaves — nothing delivered — so the motion the
/// gate measures can only have crossed an endpoint that was restarted. Same
/// reason `usb-transport-break` skips a wait rather than forging a CSW.
#[cfg(feature = "boot-actuators")]
impl HidDevice {
    /// Which completion is taken. The first is the shape the T14 showed — a
    /// freshly configured endpoint whose very first transfer fails, before the
    /// device has ever delivered — and the fourth is the mid-stream shape,
    /// where a device that has been working stops. They are different states
    /// of the driver and neither is a weaker version of the other.
    fn break_at() -> Option<u32> {
        if crate::actuator::xhci_hid_break_first() {
            Some(1)
        } else if crate::actuator::xhci_hid_break_late() {
            Some(4)
        } else {
            None
        }
    }

    pub fn stage_break(&mut self, code: u32) -> u32 {
        self.completions += 1;
        if Self::break_at() != Some(self.completions) {
            return code;
        }
        // This actuator's whole job is to leave the slot as a stalled endpoint
        // would have. Bounded against the 8-byte slot. Exclusive for the same
        // reason as `dispatch_report`: this runs on the completion, before the
        // endpoint is requeued.
        self.report.subview(0, self.report_size as usize).zero();
        super::CC_STALL
    }
}
