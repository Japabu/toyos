//! Everything in this driver that waits, and the three contexts where waiting
//! is correct.
//!
//! **This module is the split X2b builds, and it is a module rather than a
//! type.** `poll_if_pending` runs at the top of every scheduler pass on every
//! CPU, so nothing it reaches may spin on a device: a `USB_TIMEOUT_NS` spent
//! there is spent by every CPU that enters a pass, and pulling the boot stick
//! out of a T14 aims a filesystem sync, a page-cache fill and the scheduler at
//! the same dead device.
//!
//! A view handed to the poll would not have enforced it. Rust makes a module's
//! private items visible to its *descendants*, so `xhci::device` and
//! `xhci::hid` can name anything `xhci` keeps private — including a view's own
//! field. The primitives therefore live **below** the poll rather than beside
//! it: `wait_command`, `wait_transfer`, `settles`, `run_command`,
//! `control_transfer`, `restart_endpoint` and `settle_outstanding` are private
//! to this module, and `xhci`, `xhci::device` and `xhci::hid` are not inside
//! it. A pass that tried to wait does not compile.
//!
//! The three descendants are the three contexts:
//!
//! - [`boot`] — the scan. There is no scheduler yet, so the pass this would
//!   give itself back to does not exist.
//! - [`msc`] — a disk. `storage_read` and `storage_write` run on the thread
//!   that faulted, which is spending its own time.
//! - [`msc::bind`] — **the one door**, and the only blocking thing a scheduler
//!   pass can still reach. A disk plugged in after boot has to be brought up by
//!   somebody, its bring-up is Bulk-Only Transport, and that is a machine of
//!   its own that `specs/xhci-port-machine-plan.md` X2c gives one to. Until
//!   then the claim above holds of everything except a disk arriving after
//!   boot.

pub mod boot;
pub mod msc;

use crate::log;
use super::{deadline, enqueue_control, log_unrecoverable, Completion, Trb, TrbRing};
use super::{XhciController, EVENT_TRANSFER, EVENT_CMD_COMPLETE, USB_TIMEOUT_NS};
use super::{CC_SUCCESS, CC_SHORT_PACKET};
use toyos_xhci::recovery::{Act, NeedsConfigure, Recovery};

/// How one control transfer ended.
///
/// `Done` carries the bytes the device actually moved, because the completion
/// code cannot say: the Status Stage reports Success whether the Data Stage
/// filled the buffer or left it untouched. A `GET_DESCRIPTOR` that returned
/// nothing and one that returned all 18 bytes were the same value here, and the
/// caller printed the buffer either way — which is how a T14 port that answered
/// no descriptor at all was logged as `class=0x0 vendor=0000 product=0000`.
///
/// Three variants and no `Option`: the old `Option<u32>` had no code to carry
/// on the one path where the device never answered, so every failure line read
/// `code=Some(4)` and the reader had to know that `None` meant a timeout.
#[derive(Clone, Copy)]
enum Control {
    /// Both stages completed. `delivered` is what the device moved in the data
    /// stage, and zero for a transfer that has none.
    Done { delivered: u16 },
    /// The controller reported `code` for the named stage.
    Failed { stage: &'static str, code: u32 },
    /// The named stage never completed inside [`USB_TIMEOUT_NS`].
    Silent { stage: &'static str },
}

impl Control {
    /// Whether the device both finished the transfer and moved everything that
    /// was asked of it. The two halves are one question for a descriptor read
    /// and the caller has no use for them apart.
    fn moved(self, wanted: u16) -> bool {
        matches!(self, Self::Done { delivered } if delivered >= wanted)
    }

    /// Whether the transfer completed, for the requests that carry no data
    /// stage and so have no byte count to check.
    fn done(self) -> bool {
        matches!(self, Self::Done { .. })
    }
}

impl core::fmt::Display for Control {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Done { delivered } => write!(f, "{delivered} B delivered"),
            Self::Failed { stage, code } => write!(f, "{stage} stage completion {}", Completion(*code)),
            Self::Silent { stage } => write!(
                f,
                "no answer to the {stage} stage in {} ms",
                USB_TIMEOUT_NS / 1_000_000
            ),
        }
    }
}

/// One endpoint, as [`XhciController::restart_endpoint`] needs to see it.
///
/// A struct because the two callers hold their endpoints in different shapes —
/// a disk's bulk pair live in a mass-storage pool block and a HID's interrupt
/// endpoint in a device block — and the recovery needs both the block the
/// controller writes the *output context* into and the place the ring's memory
/// is. Passing them positionally is six numbers whose order is the whole
/// contract.
struct Restart<'a> {
    slot_id: u8,
    /// The device block whose output context carries this endpoint's state.
    ctx_block: usize,
    dci: u8,
    /// The address the *device* knows this endpoint by, which is what a
    /// CLEAR_FEATURE names.
    ep_addr: u8,
    /// Where in the pool the transfer ring lives, because recovery rebuilds it
    /// rather than resuming a ring the controller has a stale dequeue pointer
    /// into.
    ring_at: usize,
    ring: &'a mut TrbRing,
    ep0_ring: &'a mut TrbRing,
}

/// Spin until `ready`, and say whether that happened inside [`USB_TIMEOUT_NS`].
///
/// The register bits this covers are ones the controller sets in microseconds;
/// one that never sets belongs to a controller or a port this driver cannot
/// drive, and every caller turns `false` into a refusal that names it. Before
/// this existed the five of them were bare `spin_loop`s, which on a machine
/// with no serial port is the same picture as every other way a boot can stop:
/// `Boot: peripherals ready` painted on the panel, forever.
fn settles(ready: impl Fn() -> bool) -> bool {
    let deadline = deadline();
    while !ready() {
        if crate::clock::nanos_since_boot() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}


/// What one endpoint's recovery still owes the **device** once the controller
/// has taken it off the transfer it was running.
///
/// A value of this is what [`XhciController::quiesce_endpoint`] produces and
/// the only thing [`XhciController::clear_endpoint_halt`] accepts, so the two
/// halves of a recovery cannot be run in the other order. That matters to
/// exactly one caller: Bulk-Only Transport's Reset Recovery has a device reset
/// of its own to put between them, and it may not issue that reset while either
/// endpoint still holds a transfer the driver stopped waiting for — see
/// [`Act::ClearHalt`].
#[derive(Clone, Copy)]
pub(in crate::drivers::xhci) enum Owed {
    /// The endpoint runs. Nothing further is owed.
    Nothing,
    /// The device is still holding a halt on the endpoint at this address.
    ClearHalt { ep_addr: u8 },
    /// The endpoint could not be taken off its transfer. Carried rather than
    /// reported by returning early, because the *other* endpoint and the
    /// device's own reset still have to happen — leaving one endpoint stopped
    /// because a step on the other failed is what turns a recoverable device
    /// into a permanently offline one.
    Failed,
}

impl XhciController {
    /// Take one endpoint back to a state that runs TRBs, waiting for each step.
    ///
    /// The route is [`Recovery`]'s and the effects are here. **This driver of
    /// it blocks, and that is correct for its one caller**: a disk's bulk pair
    /// is recovered from `storage_read`/`storage_write`, on the thread that
    /// faulted, which is spending its own time. A HID endpoint is recovered at
    /// the top of a scheduler pass, where it would be spending everybody's, and
    /// [`Self::step_recovery`] is the same route stepped across passes.
    fn restart_endpoint(&mut self, mut ep: Restart<'_>) -> bool {
        let owed = self.quiesce_endpoint(&mut ep);
        self.clear_endpoint_halt(ep.slot_id, ep.ep0_ring, owed)
    }

    /// The half of one endpoint's recovery the **controller** answers: every
    /// command [`Recovery`] owes, up to the point where the sequence would
    /// speak to the device.
    fn quiesce_endpoint(&mut self, ep: &mut Restart<'_>) -> Owed {
        let state = self.endpoint_state(ep.ctx_block, ep.dci);
        log!("xHCI: slot {} endpoint {} is {state}, recovering", ep.slot_id, ep.dci);
        let (mut seq, mut act) = match Recovery::begin(state) {
            Ok(begun) => begun,
            Err(NeedsConfigure(state)) => {
                log_unrecoverable(ep.slot_id, ep.dci, state);
                return Owed::Failed;
            }
        };
        loop {
            let cmd = match act {
                Act::Running => return Owed::Nothing,
                Act::ClearHalt => return Owed::ClearHalt { ep_addr: ep.ep_addr },
                Act::Command(cmd) => cmd,
            };
            let trb = self.recovery_trb(cmd, ep.slot_id, ep.dci, ep.ring, ep.ring_at);
            if !self.run_command(trb, cmd.name()) {
                return Owed::Failed;
            }
            act = seq.completed();
        }
    }

    /// The half the **device** answers, which is the only packet a recovery
    /// puts on the bus.
    fn clear_endpoint_halt(&mut self, slot_id: u8, ep0_ring: &mut TrbRing, owed: Owed) -> bool {
        let ep_addr = match owed {
            Owed::Nothing => return true,
            Owed::Failed => return false,
            Owed::ClearHalt { ep_addr } => ep_addr,
        };
        let cleared =
            self.control_transfer(slot_id, ep0_ring, 0x02, 0x01, 0, ep_addr as u16, None, 0);
        if !cleared.done() {
            log!("xHCI: slot {slot_id} would not clear the halt on endpoint {ep_addr:#04x}: \
                 {cleared}");
            return false;
        }
        true
    }

    /// Run whatever is outstanding to its end, waiting for each answer.
    ///
    /// **Blocking is correct here and only here**: this is the boot scan, so
    /// there is no scheduler yet and the pass this would otherwise give itself
    /// back to does not exist, and `init` has not published the controller for
    /// anything else to poll it. An endpoint holding no TRB raises no further
    /// interrupt, so a device whose *first* transfer failed during the scan
    /// would otherwise stay recorded and silent for the whole boot.
    ///
    /// It is also the boot scan's driver of the enumeration sequence, and the
    /// only difference between it and the hot-plug one: the same acts, run one
    /// after another here and one per scheduler pass there.
    fn settle_outstanding(&mut self) {
        while self.outstanding.busy() || self.devices.iter().any(|d| d.broke_with.is_some()) {
            self.recover_endpoints();
            while self.outstanding.busy() {
                while let Some(event) = self.next_event() {
                    self.dispatch_event(event);
                }
                self.advance_outstanding();
                core::hint::spin_loop();
            }
        }
    }

    /// The completion code and slot id of the command that was enqueued at
    /// `trb`, or `None` if the controller never answered.
    ///
    /// **The address and not the next completion of any command.** A Command
    /// Completion Event names its Command TRB (§6.4.2.2), and a driver that
    /// took the first one it saw handed a command that had run out its deadline
    /// and answered afterwards to whoever asked next. That was latent while
    /// every command was a submit followed by its own wait, and unavoidable now
    /// that a scheduler pass can leave one behind.
    fn wait_command(&mut self, trb: u64) -> Option<(u32, u32)> {
        let deadline = deadline();
        loop {
            let Some(event) = self.next_event() else {
                if crate::clock::nanos_since_boot() >= deadline {
                    return None;
                }
                core::hint::spin_loop();
                continue;
            };
            if (event.control >> 10) & 0x3F == EVENT_CMD_COMPLETE && event.param & !0xF == trb {
                return Some(((event.status >> 24) & 0xFF, (event.control >> 24) & 0xFF));
            }
            self.dispatch_event(event);
        }
    }

    /// Submit `trb` and say whether the controller accepted it, logging
    /// anything it did not. `what` names the command in that line, because a
    /// bare code is unreadable at 3am.
    ///
    /// A `bool` and not the `Option<u32>` it was: the only `Some` that value
    /// ever held was `CC_SUCCESS`, so every caller's `is_none()` was asking a
    /// question the type pretended was open.
    fn run_command(&mut self, trb: Trb, what: &str) -> bool {
        let at = self.submit_command(trb);
        match self.wait_command(at) {
            Some((CC_SUCCESS, _)) => true,
            Some((code, _)) => {
                log!("xHCI: {what} failed: {}", Completion(code));
                false
            }
            None => {
                log!("xHCI: {what} timed out");
                false
            }
        }
    }

    /// The completion of the transfer just queued on (`slot`, `dci`), as a
    /// completion code and the number of bytes the controller did *not* move.
    ///
    /// The event ring is one queue for the whole controller, so anything that
    /// arrives here and is not ours belongs to a bound device delivering a
    /// report — handing it to `dispatch_event` rather than dropping it is what
    /// keeps that device's interrupt ring fed. Matching on the endpoint as
    /// well as the slot matters for mass storage, where one slot carries three
    /// endpoints and a stalled one still completes.
    fn wait_transfer(&mut self, slot: u8, dci: u8) -> Option<(u32, u32)> {
        let deadline = deadline();
        let port = self.port_of_slot(slot);
        loop {
            let Some(event) = self.next_event() else {
                if crate::clock::nanos_since_boot() >= deadline {
                    return None;
                }
                // **A device that has been unplugged is not a device that is
                // slow.** The budget exists for one that might still answer; a
                // port that reads disconnected has nothing behind it, and every
                // nanosecond spent proving that is spent holding `XHCI` with
                // preemption disabled — on the path a filesystem sync, a
                // page-cache fill and every scheduler pass all take. Pulling
                // the stick a machine logs to aims all three at a dead device
                // on the same event.
                if port.is_some_and(|p| !self.read_portsc(p).connected()) {
                    return None;
                }
                core::hint::spin_loop();
                continue;
            };
            let trb_type = (event.control >> 10) & 0x3F;
            let ev_slot = ((event.control >> 24) & 0xFF) as u8;
            let ev_dci = ((event.control >> 16) & 0x1F) as u8;
            if trb_type == EVENT_TRANSFER && ev_slot == slot && ev_dci == dci {
                return Some(((event.status >> 24) & 0xFF, event.status & 0x00FF_FFFF));
            }
            self.dispatch_event(event);
        }
    }

    /// One control transfer on `ring`, which must be the EP0 ring named by
    /// `slot`'s device context.
    fn control_transfer(
        &mut self,
        slot: u8,
        ring: &mut TrbRing,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        data_buf: Option<u64>,
        data_len: u16,
    ) -> Control {
        let has_data = enqueue_control(
            ring, bm_request_type, b_request, w_value, w_index, data_buf, data_len,
        );
        self.ring_doorbell(slot, 1);

        let mut delivered = 0u16;
        if has_data {
            match self.wait_transfer(slot, 1) {
                Some((CC_SUCCESS | CC_SHORT_PACKET, residue)) => {
                    // A residue past the length asked for is a controller
                    // contradicting itself; believing it would report more bytes
                    // delivered than the buffer holds.
                    delivered = data_len.saturating_sub(residue.min(u16::MAX as u32) as u16);
                }
                // The status stage is deliberately not waited for. An errored
                // data stage halts EP0, so the TRB behind it never runs, and
                // waiting would spend the whole transfer budget learning that.
                Some((code, _)) => return Control::Failed { stage: "data", code },
                None => return Control::Silent { stage: "data" },
            }
        }
        match self.wait_transfer(slot, 1) {
            Some((CC_SUCCESS, _)) => Control::Done { delivered },
            Some((code, _)) => Control::Failed { stage: "status", code },
            None => Control::Silent { stage: "status" },
        }
    }

}
