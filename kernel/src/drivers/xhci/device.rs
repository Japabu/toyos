use core::num::NonZeroU8;
use core::ptr::{write_volatile, write_bytes};

use crate::log;
use toyos_xhci::enumerate::{
    self, ep0_packet_from_descriptor, initial_ep0_packet, Act, Enumeration, Learnt, Next, Request,
};
use toyos_xhci::job::{Await, Outcome, Stages};
use toyos_xhci::port::{self, Reset};
use super::{deadline, Answer, Trb, TrbRing, What, XhciController, PAGE};
use super::{OFF_DCBAA, OFF_INPUT_CTX, OFF_DATA_BUF};
use super::{DEV_INT_RING, DEV_EP0_RING, DEV_OUT_CTX, DEV_REPORT};
use super::{TRB_ENABLE_SLOT, TRB_ADDRESS_DEVICE, TRB_CONFIGURE_EP, TRB_EVALUATE_CONTEXT};
use super::{enqueue_control, CC_SUCCESS};

use super::hid::{HidType, HidRole, HidDevice};
use super::msc::{MscInterface, MscRings};

/// How much of a configuration descriptor the driver reads and parses.
///
/// It is also the size of the GET_DESCRIPTOR request, so a device cannot make
/// the parser walk past it: `wTotalLength` is clamped to what was actually
/// asked for, and the scratch page is four times this.
const MAX_CONFIG_DESC: usize = 256;

/// One endpoint from a configuration descriptor, which this driver has decided
/// it can configure.
///
/// [`Self::new`] is the only way to make one, and what enforces that is the
/// *private field* below rather than the private `fn`: a constructor beside
/// public fields constrains nothing, because a struct literal needs only that
/// the struct and its fields be visible. With `dci` private, no module under
/// `xhci` can build one — so a `dci` that exists is a device context index the
/// driver may write, and "does this endpoint exist" is `Option<Endpoint>`
/// rather than a zero in a field. That is the whole point: the sentinel it
/// replaces was the endpoint *address*, and one direction was guarded by
/// design while the other was guarded by the accident that a zero address and
/// the "not filled in yet" value are the same byte.
///
/// One type for both kinds, with a field each kind ignores, because the
/// alternative is two types with two copies of the constructor — and the
/// constructor is the invariant.
#[derive(Clone, Copy)]
pub(super) struct Endpoint {
    pub(super) addr: u8,
    /// 2..=31 by construction, and private so that stays true. `bind` shifts
    /// `1u32` by this and indexes the input context with it, and `write_ctx32`
    /// bounds neither.
    dci: u8,
    pub(super) max_packet: u16,
    /// The SuperSpeed companion's burst size. Zero is legal and means one
    /// packet per burst, which is what a device that omits the companion means.
    pub(super) max_burst: u8,
    /// bInterval. Only an interrupt endpoint uses it.
    pub(super) interval: u8,
}

impl Endpoint {
    pub(super) fn dci(&self) -> u8 {
        self.dci
    }

    /// `None` for an address naming endpoint 0, which is the check this type
    /// exists to make unforgettable. `0x80` and `0x10` are non-zero bytes and
    /// they resolve to DCI 1, EP0's own endpoint context, and DCI 0, the slot
    /// context: a driver that configures a bulk endpoint at either writes it
    /// over the device's control endpoint or over its speed and root-hub port,
    /// from bytes the device chose, and then relies on the host controller to
    /// reject the command it built.
    fn new(addr: u8, max_packet: u16, interval: u8) -> Option<Self> {
        let num = addr & 0x0F;
        (num != 0).then(|| Self {
            addr,
            dci: num * 2 + u8::from(addr & 0x80 != 0),
            max_packet,
            max_burst: 0,
            interval,
        })
    }
}

/// Result of parsing a USB device's configuration descriptor for HID interfaces.
#[derive(Clone, Copy)]
struct HidInterfaceInfo {
    protocol: HidType,
    iface_num: u8,
    ep: Endpoint,
}

/// What one configuration descriptor offered that this driver can drive.
///
/// Both variants are *complete*: there is no value of this type describing an
/// interface whose endpoints the driver has not resolved, which is why the
/// walk below accumulates into [`Walk`] and converts once.
#[derive(Clone, Copy)]
enum Function {
    Hid(HidInterfaceInfo),
    Msc(MscInterface),
}

impl Function {
    /// The same interface as far as *the order of what is left* depends on it,
    /// which is all [`Enumeration`] is given: whether there is a boot protocol
    /// to select, and nothing about which endpoints or which interface number.
    fn shape(self) -> enumerate::Function {
        match self {
            // A tablet reports in its own format, so there is no boot protocol
            // to ask for and asking is a request it may stall for.
            Self::Hid(info) if info.protocol == HidType::Tablet => enumerate::Function::Hid,
            Self::Hid(_) => enumerate::Function::BootHid,
            Self::Msc(_) => enumerate::Function::Msc,
        }
    }
}

/// The same interface while the walk is still reading its endpoints.
///
/// Separate from [`Function`] so that "one bulk endpoint so far" is a state the
/// walk can be in and the rest of the driver cannot. The conversion in
/// [`Walk::finish`] *is* the completeness test, and it is the only one — two
/// tests on `!= 0` further down deleted themselves when this landed.
enum Walk {
    Hid { protocol: HidType, iface_num: u8, ep: Option<Endpoint> },
    Msc { iface_num: u8, in_ep: Option<Endpoint>, out_ep: Option<Endpoint> },
}

impl Walk {
    /// Move a finished interface into the running answer, if it is one this
    /// driver can bind.
    fn finish(self, hid: &mut Option<HidInterfaceInfo>, msc: &mut Option<MscInterface>) {
        match self {
            Self::Hid { protocol, iface_num, ep: Some(ep) } => {
                if hid.is_none() {
                    *hid = Some(HidInterfaceInfo { protocol, iface_num, ep });
                }
            }
            Self::Hid { .. } => {}
            Self::Msc { iface_num, in_ep: Some(in_ep), out_ep: Some(out_ep) } => {
                if msc.is_none() {
                    *msc = Some(MscInterface { iface_num, in_ep, out_ep });
                }
            }
            Self::Msc { iface_num, .. } => {
                log!("xHCI: mass-storage interface {iface_num} has no pair of bulk endpoints \
                     this driver can configure, skipping");
            }
        }
    }
}

/// Tell the controller the Max Packet Size of EP0 that only the device knew.
///
/// Evaluate Context rather than Configure Endpoint: §4.6.7 defines it as the
/// command that changes exactly this field on a device that is already
/// addressed, and the A1 flag alone is what says EP0 is the context to look at.
fn evaluate_ep0_trb(ctrl: &mut XhciController, slot_id: u8, max_packet: u16) -> Trb {
    let dma = ctrl.dma();
    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    unsafe { input_ctx.zero(); }
    ctrl.write_ctx32(input_ctx_ptr, 0, 1, 1 << 1);
    let ep0_dw1 = (3u32 << 1) | (4u32 << 3) | ((max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx_ptr, 2, 1, ep0_dw1);

    let mut evaluate = Trb::ZERO;
    evaluate.param = input_ctx.phys();
    evaluate.control = TRB_EVALUATE_CONTEXT | ((slot_id as u32) << 24);
    evaluate
}

/// A little-endian 16-bit field at `at`, or 0 past the end. Descriptors are
/// byte-aligned in the wire format and land wherever the previous descriptor's
/// length put them, so the packed-struct reads this replaces were unaligned as
/// well as unbounded.
fn le16(buf: &[u8], at: usize) -> u16 {
    let lo = buf.get(at).copied().unwrap_or(0) as u16;
    let hi = buf.get(at + 1).copied().unwrap_or(0) as u16;
    lo | (hi << 8)
}

/// Walk a configuration descriptor for the first interface this driver can
/// bind, returning it with its configuration value.
///
/// Every field read here is device-supplied, including the lengths that decide
/// where the next descriptor starts — so the walk is bounded by the buffer, a
/// zero length terminates it rather than looping forever, and every field is
/// read through `get`. Mass storage wins a tie with HID because a device
/// offering a disk is a disk; nothing in this tree offers both.
fn parse_config(buf: &[u8]) -> Option<(u8, Function)> {
    let total_len = (le16(buf, 2) as usize).min(buf.len());
    let config_val = *buf.get(5)?;

    let mut hid: Option<HidInterfaceInfo> = None;
    let mut msc: Option<MscInterface> = None;
    // Which interface the endpoint descriptors that follow belong to.
    let mut current: Option<Walk> = None;
    // A SuperSpeed companion describes the endpoint immediately before it.
    let mut last_ep_in: Option<bool> = None;

    let mut offset = 0usize;
    while offset + 2 <= total_len {
        let desc_len = buf[offset] as usize;
        let desc_type = buf[offset + 1];
        if desc_len == 0 {
            break;
        }
        let desc = match buf.get(offset..(offset + desc_len).min(total_len)) {
            Some(d) => d,
            None => break,
        };

        match desc_type {
            // Interface
            4 if desc.len() >= 9 => {
                if let Some(done) = current.take() {
                    done.finish(&mut hid, &mut msc);
                }
                let (class, sub, proto) = (desc[5], desc[6], desc[7]);
                current = if class == 0x08 && sub == 0x06 && proto == 0x50 {
                    Some(Walk::Msc { iface_num: desc[2], in_ep: None, out_ep: None })
                } else if class == 3 {
                    let protocol = match (sub, proto) {
                        (1, 1) => Some(HidType::Keyboard),
                        (1, 2) => Some(HidType::Mouse),
                        (0, _) => Some(HidType::Tablet),
                        _ => None,
                    };
                    protocol.map(|protocol| Walk::Hid { protocol, iface_num: desc[2], ep: None })
                } else {
                    None
                };
            }
            // Endpoint
            5 if desc.len() >= 7 => {
                let transfer = desc[3] & 0x3;
                last_ep_in = None;
                // An address this driver cannot turn into a device context
                // index is not an endpoint as far as anything below here is
                // concerned, and `Endpoint::new` is the only place that is
                // decided. `if let` and not a `let ... else`: the offset
                // advance is at the bottom of this loop.
                if let Some(ep) = Endpoint::new(desc[2], le16(desc, 4), desc[6]) {
                    let is_in = ep.addr & 0x80 != 0;
                    match &mut current {
                        Some(Walk::Hid { ep: slot, .. }) if is_in && slot.is_none() => {
                            *slot = Some(ep);
                        }
                        // Bulk only: a mass-storage interface's interrupt
                        // endpoint belongs to CBI, which this driver does not
                        // speak.
                        Some(Walk::Msc { in_ep, out_ep, .. }) if transfer == 2 => {
                            if is_in && in_ep.is_none() {
                                *in_ep = Some(ep);
                                last_ep_in = Some(true);
                            } else if !is_in && out_ep.is_none() {
                                *out_ep = Some(ep);
                                last_ep_in = Some(false);
                            }
                        }
                        _ => {}
                    }
                }
            }
            // SuperSpeed Endpoint Companion, which is where a SuperSpeed
            // device states the burst size of the endpoint just above it.
            0x30 if desc.len() >= 3 => {
                if let (Some(Walk::Msc { in_ep, out_ep, .. }), Some(is_in)) =
                    (&mut current, last_ep_in)
                {
                    if let Some(ep) = if is_in { in_ep } else { out_ep } {
                        ep.max_burst = desc[2];
                    }
                }
            }
            _ => {}
        }
        offset += desc_len;
    }
    if let Some(done) = current {
        done.finish(&mut hid, &mut msc);
    }

    // Mass storage wins a tie with HID because a device offering a disk is a
    // disk. No completeness test here: an interface that reached `hid` or `msc`
    // has one, because `Walk::finish` could not have built it otherwise.
    if let Some(m) = msc {
        return Some((config_val, Function::Msc(m)));
    }
    Some((config_val, Function::Hid(hid?)))
}

/// Ask a port to reset.
///
/// Separate from the wait because the two ends of it have different callers.
/// The controller answers by setting PRC, which is a Port Status Change Event
/// as much as it is a register bit — so the boot path spins on the register
/// ([`init_device`]) while the runtime path comes back when the event arrives,
/// and neither is a second implementation of the other. The T14's own root
/// ports take 55 ms over this (`specs/reference/metal-hardware-inventory.md`), which is
/// the whole reason the runtime path must not hold a scheduler pass across it.
pub fn reset_port(ctrl: &mut XhciController, port_idx: u8, kind: Reset) {
    let portsc = ctrl.read_portsc(port_idx);
    ctrl.write_portsc(port_idx, port::reset_write(kind, portsc));
}

/// Whether the port has finished the reset it was asked for.
pub fn reset_done(ctrl: &XhciController, port_idx: u8) -> bool {
    super::port_answers() && ctrl.read_portsc(port_idx).reset_changed()
}

/// One device's enumeration, as everything the acts still owed will need.
///
/// The state travels with the wait because the pass that submitted an act gave
/// itself back: by the time the answer arrives, the local variables the
/// straight-line version kept this in are long gone. `issued` travels with it
/// for the same reason [`Enumeration`] does — what an answer *means* is a
/// function of what was asked, and nothing else holds that.
pub(super) struct Enumerating {
    port_idx: u8,
    speed: u8,
    /// The slot the controller enabled. Every refusal from here on carries it
    /// back to the port, because only Disable Slot gives one back.
    slot_id: u8,
    block: usize,
    ep0_ring: TrbRing,
    /// EP0's Max Packet Size as the controller currently believes it.
    packet: u16,
    seq: Enumeration,
    issued: Act,
    /// The configuration value and the function the descriptor named.
    parsed: Option<(u8, Function)>,
    /// What Configure Endpoint named, which the bind behind it owns. Carried
    /// rather than rebuilt: a second `TrbRing::init` would zero memory the
    /// controller is by then reading.
    rings: Option<Rings>,
}

/// The transfer rings one device's Configure Endpoint put into the Running
/// state, and where they came from.
#[derive(Clone, Copy)]
enum Rings {
    Hid(TrbRing),
    Msc(MscRings),
}


/// Acknowledge the reset, read what the port came up as, and ask for a slot.
///
/// The one act that needs no device state, which is why it is here and not in
/// [`perform`]: until the controller answers there is no slot id, no pool block
/// and no EP0 ring.
pub(super) fn begin(ctrl: &mut XhciController, port_idx: u8) {
    let portsc = ctrl.read_portsc(port_idx);
    ctrl.write_portsc(port_idx, portsc.neutral().acknowledging_reset());

    let portsc = ctrl.read_portsc(port_idx);
    if !portsc.enabled() {
        log!("xHCI: port {} reset but not enabled (PORTSC {:#010x}); skipping it",
            port_idx + 1, portsc.raw());
        return finish(ctrl, port_idx, None);
    }
    let speed = portsc.speed();
    log!("xHCI: port {} enabled, speed={}", port_idx + 1, speed);
    let Some(packet) = initial_ep0_packet(speed) else {
        log!("xHCI: port {} came up at speed {speed}, which is not a speed this driver has a \
             control-endpoint packet size for; skipping it", port_idx + 1);
        return finish(ctrl, port_idx, None);
    };

    let (seq, _) = Enumeration::begin();
    let mut enable_slot = Trb::ZERO;
    enable_slot.control = TRB_ENABLE_SLOT;
    let on = Await::Command { trb: ctrl.submit_command(enable_slot) };
    let what = What::SlotWanted { port_idx, speed, packet, seq };
    ctrl.outstanding.submit(what, on, Stages::One, deadline());
}

/// The controller answered Enable Slot. Everything after this point has a
/// device to write for.
pub(super) fn slot_answered(
    ctrl: &mut XhciController,
    port_idx: u8,
    speed: u8,
    packet: u16,
    seq: Enumeration,
    outcome: Outcome,
) {
    let Outcome::Command { code: CC_SUCCESS, slot: slot_id } = outcome else {
        log!("xHCI: Enable Slot on port {}: {}", port_idx + 1, Answer(outcome));
        return finish(ctrl, port_idx, None);
    };
    // A slot id is the controller's answer, not the driver's, and CONFIG's
    // MaxSlotsEn is only advisory to a controller that chooses to ignore it —
    // QEMU's does. Nothing of the driver's is written for this device yet, so
    // there is nothing here to unwind; the controller's own Device Slot is
    // already allocated, which is why every refusal below carries the slot id
    // back rather than dropping it.
    let Some(block) = ctrl.layout.device(slot_id) else {
        log!("xHCI: slot {} is beyond the pool's {} device blocks, dropping port {}",
            slot_id, ctrl.layout.dev_blocks, port_idx + 1);
        return finish(ctrl, port_idx, Some(slot_id));
    };
    log!("xHCI: slot {} enabled (dma +{:#x})", slot_id, block);

    let ep0_ring = TrbRing::init(ctrl.dma().subslice(block + DEV_EP0_RING, PAGE));
    let state = Enumerating {
        port_idx,
        speed,
        slot_id,
        block,
        ep0_ring,
        packet,
        seq,
        issued: Act::Command(enumerate::Command::EnableSlot),
        parsed: None,
        rings: None,
    };
    advance(ctrl, state, Learnt::Nothing);
}

/// The controller answered the act that was outstanding. Read what it says,
/// and either carry on or leave the port with whatever it has spent.
pub(super) fn stepped(ctrl: &mut XhciController, mut state: Enumerating, outcome: Outcome) {
    let port = state.port_idx + 1;
    let learnt = match state.issued {
        Act::Command(cmd) => {
            if !outcome.succeeded() {
                log!("xHCI: {} on port {port}: {}", command_name(cmd), Answer(outcome));
                return finish(ctrl, state.port_idx, Some(state.slot_id));
            }
            match cmd {
                enumerate::Command::AddressDevice => log!("xHCI: device addressed"),
                enumerate::Command::ConfigureEndpoint => log!("xHCI: endpoint configured"),
                enumerate::Command::EnableSlot | enumerate::Command::EvaluateEp0 => {}
            }
            Learnt::Nothing
        }
        // SET_PROTOCOL is the one request a device may refuse without being
        // refused for it: the interface said it has a boot protocol, and a
        // device that will not select it still reports in the format its
        // descriptors promised often enough to be worth binding.
        Act::Request(Request::SetProtocol) => {
            if !outcome.succeeded() {
                log!("xHCI: SET_PROTOCOL on port {port}: {}", Answer(outcome));
            }
            Learnt::Nothing
        }
        Act::Request(request) => {
            let want = match request {
                Request::DeviceDescriptor { want } => want,
                Request::ConfigDescriptor => MAX_CONFIG_DESC as u16,
                Request::SetConfiguration | Request::SetProtocol => 0,
            };
            let Some(delivered) = delivered(outcome, want) else {
                log!("xHCI: {} on port {port}: {}", request_name(request), Answer(outcome));
                return finish(ctrl, state.port_idx, Some(state.slot_id));
            };
            match read_back(ctrl, &mut state, request, delivered) {
                Ok(learnt) => learnt,
                Err(()) => return finish(ctrl, state.port_idx, Some(state.slot_id)),
            }
        }
    };
    advance(ctrl, state, learnt);
}

/// Ask the sequence what is owed next and do it.
fn advance(ctrl: &mut XhciController, state: Enumerating, learnt: Learnt) {
    match state.seq.completed(learnt) {
        Next::Act(seq, act) => perform(ctrl, Enumerating { seq, ..state }, act),
        Next::Bind => bind(ctrl, state),
        Next::Refuse => {
            log!("xHCI: no HID boot interface found on port {}, skipping it",
                state.port_idx + 1);
            finish(ctrl, state.port_idx, Some(state.slot_id))
        }
    }
}

/// Submit one act and record that the controller owes an answer for it.
fn perform(ctrl: &mut XhciController, mut state: Enumerating, act: Act) {
    state.issued = act;
    let submitted = match act {
        Act::Command(cmd) => command(ctrl, &mut state, cmd)
            .map(|trb| (Await::Command { trb: ctrl.submit_command(trb) }, Stages::One)),
        Act::Request(request) => Some(control(ctrl, &mut state, request)),
    };
    let Some((on, stages)) = submitted else {
        return finish(ctrl, state.port_idx, Some(state.slot_id));
    };
    ctrl.outstanding.submit(What::Enumerating(state), on, stages, deadline());
}

/// The TRB for one command act, or `None` where the driver has no room to
/// perform it and has said so.
fn command(
    ctrl: &mut XhciController,
    state: &mut Enumerating,
    cmd: enumerate::Command,
) -> Option<Trb> {
    match cmd {
        // Answered in `begin`, which is where the sequence starts; it is never
        // asked for again.
        enumerate::Command::EnableSlot => None,
        enumerate::Command::AddressDevice => Some(address_device_trb(ctrl, state)),
        enumerate::Command::EvaluateEp0 => {
            Some(evaluate_ep0_trb(ctrl, state.slot_id, state.packet))
        }
        enumerate::Command::ConfigureEndpoint => configure_endpoint_trb(ctrl, state),
    }
}

/// Put one control request on the device's EP0 ring and say what ends it.
///
/// **A request with a data stage is two completions**, because the data stage
/// carries IOC so the driver can learn how many bytes arrived — and
/// [`enqueue_control`] already answers exactly that question, so the two cannot
/// disagree.
fn control(
    ctrl: &mut XhciController,
    state: &mut Enumerating,
    request: Request,
) -> (Await, Stages) {
    let dma = ctrl.dma();
    let scratch = dma.phys() + OFF_DATA_BUF as u64;
    let (bm_request_type, b_request, w_value, w_index, data, len) = match request {
        Request::DeviceDescriptor { want } => (0x80, 0x06, 0x0100, 0, Some(scratch), want),
        Request::ConfigDescriptor => {
            (0x80, 0x06, 0x0200, 0, Some(scratch), MAX_CONFIG_DESC as u16)
        }
        Request::SetConfiguration => {
            let (config_val, _) = state.parsed.expect("a configuration named a function");
            (0x00, 0x09, config_val as u16, 0, None, 0)
        }
        Request::SetProtocol => {
            let iface = match state.parsed {
                Some((_, Function::Hid(info))) => info.iface_num,
                _ => unreachable!("only a HID interface has a boot protocol to select"),
            };
            (0x21, 0x0B, 0, iface as u16, None, 0)
        }
    };
    // The scratch is one page shared by every enumeration, and enumeration is
    // serial: the slot holds one operation and a port inside an effect is not
    // decided about. Zeroed before each read so a short answer leaves zeroes
    // behind it rather than the last device's descriptor.
    if data.is_some() {
        unsafe { write_bytes(dma.ptr_at(OFF_DATA_BUF), 0, MAX_CONFIG_DESC); }
    }
    let has_data = enqueue_control(
        &mut state.ep0_ring, bm_request_type, b_request, w_value, w_index, data, len,
    );
    ctrl.ring_doorbell(state.slot_id, 1);
    let stages = if has_data { Stages::DataThenStatus } else { Stages::One };
    (Await::Transfer { slot: state.slot_id, dci: 1 }, stages)
}

/// What one control request that completed left in the scratch page, and what
/// the order of the rest of the sequence takes from it. `Err` is a device this
/// driver will not bind, already said so.
fn read_back(
    ctrl: &mut XhciController,
    state: &mut Enumerating,
    request: Request,
    delivered: u16,
) -> Result<Learnt, ()> {
    let port = state.port_idx + 1;
    let buf = ctrl.dma().ptr_at(OFF_DATA_BUF);
    match request {
        Request::DeviceDescriptor { want: 8 } => {
            if delivered < 8 {
                log!("xHCI: port {port} would not give up the first 8 bytes of its device \
                     descriptor: {delivered} B delivered");
                return Err(());
            }
            let stated = unsafe { *buf.add(7) };
            let Some(ep0_packet) = ep0_packet_from_descriptor(state.speed, stated) else {
                log!("xHCI: port {port} states bMaxPacketSize0={stated}, which is not a control \
                     packet size a speed-{} device has; skipping it", state.speed);
                return Err(());
            };
            if ep0_packet == state.packet {
                return Ok(Learnt::Nothing);
            }
            log!("xHCI: port {port} EP0 packet size {} -> {ep0_packet}", state.packet);
            state.packet = ep0_packet;
            Ok(Learnt::Ep0PacketWrong)
        }
        Request::DeviceDescriptor { .. } => {
            if delivered < 18 {
                log!("xHCI: GET_DESCRIPTOR(Device) on port {port}: {delivered} B delivered");
                return Err(());
            }
            let descriptor = unsafe { core::slice::from_raw_parts(buf, 18) };
            log!("xHCI: device class={:#x} vendor={:04x} product={:04x}",
                descriptor[4], le16(descriptor, 8), le16(descriptor, 10));
            Ok(Learnt::Nothing)
        }
        // Nine bytes is a configuration descriptor's own header, which is where
        // `wTotalLength` lives; fewer than that is not one. The parser is then
        // bounded by what *arrived* rather than by what was asked for, so the
        // zeroes behind a short answer are never walked as descriptors.
        Request::ConfigDescriptor => {
            if delivered < 9 {
                log!("xHCI: port {port} answered {delivered} B to GET_DESCRIPTOR(Config); a \
                     configuration descriptor is at least 9", );
                return Err(());
            }
            let config = unsafe { core::slice::from_raw_parts(buf, delivered as usize) };
            let Some((config_val, function)) = parse_config(config) else {
                return Ok(Learnt::Nothing);
            };
            match function {
                Function::Msc(msc) => log!("xHCI: mass storage iface={} in={:#x}/{} out={:#x}/{}",
                    msc.iface_num, msc.in_ep.addr, msc.in_ep.max_packet,
                    msc.out_ep.addr, msc.out_ep.max_packet),
                Function::Hid(info) => log!("xHCI: HID {} iface={} ep={:#x} max_pkt={} \
                     interval={} dci={}", hid_kind(info.protocol), info.iface_num, info.ep.addr,
                    info.ep.max_packet, info.ep.interval, info.ep.dci()),
            }
            state.parsed = Some((config_val, function));
            Ok(Learnt::Function(function.shape()))
        }
        Request::SetConfiguration => {
            log!("xHCI: configuration set");
            Ok(Learnt::Nothing)
        }
        Request::SetProtocol => Ok(Learnt::Nothing),
    }
}

/// The bytes a control request moved, and `None` where it did not complete.
///
/// The completion code cannot say on its own: the status stage reports Success
/// whether the data stage filled the buffer or left it untouched, which is how
/// a T14 port that answered no descriptor at all was logged as `class=0x0
/// vendor=0000 product=0000`.
fn delivered(outcome: Outcome, want: u16) -> Option<u16> {
    let Outcome::Transfer { code: CC_SUCCESS, residue } = outcome else { return None };
    // A residue past the length asked for is a controller contradicting itself;
    // believing it would report more bytes delivered than the buffer holds.
    Some(want.saturating_sub(residue.min(u16::MAX as u32) as u16))
}

fn command_name(cmd: enumerate::Command) -> &'static str {
    match cmd {
        enumerate::Command::EnableSlot => "Enable Slot",
        enumerate::Command::AddressDevice => "Address Device",
        enumerate::Command::EvaluateEp0 => "Evaluate Context (EP0 packet size)",
        enumerate::Command::ConfigureEndpoint => "Configure Endpoint",
    }
}

fn request_name(request: Request) -> &'static str {
    match request {
        Request::DeviceDescriptor { want: 8 } => "GET_DESCRIPTOR(Device, 8)",
        Request::DeviceDescriptor { .. } => "GET_DESCRIPTOR(Device)",
        Request::ConfigDescriptor => "GET_DESCRIPTOR(Config)",
        Request::SetConfiguration => "SET_CONFIGURATION",
        Request::SetProtocol => "SET_PROTOCOL",
    }
}

fn hid_kind(protocol: HidType) -> &'static str {
    match protocol {
        HidType::Keyboard => "keyboard",
        HidType::Mouse => "mouse",
        HidType::Tablet => "tablet",
    }
}

/// The Address Device command, with the slot and control endpoint this device
/// is about to be known by.
fn address_device_trb(ctrl: &mut XhciController, state: &Enumerating) -> Trb {
    let dma = ctrl.dma();
    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    unsafe { input_ctx.zero(); }

    ctrl.write_ctx32(input_ctx_ptr, 0, 1, 0x3); // Add Slot + EP0
    let slot_dw0 = ((state.speed as u32) << 20) | (1u32 << 27);
    ctrl.write_ctx32(input_ctx_ptr, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx_ptr, 1, 1, (state.port_idx as u32 + 1) << 16);

    let ep0_dw1 = (3u32 << 1) | (4u32 << 3) | ((state.packet as u32) << 16);
    ctrl.write_ctx32(input_ctx_ptr, 2, 1, ep0_dw1);
    let ep0_dequeue = state.ep0_ring.dequeue();
    ctrl.write_ctx32(input_ctx_ptr, 2, 2, ep0_dequeue as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 3, (ep0_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 4, 8);

    let out_ctx = dma.subslice(state.block + DEV_OUT_CTX, PAGE / 2);
    unsafe { out_ctx.zero(); }
    unsafe {
        let dcbaa = dma.ptr_at(OFF_DCBAA) as *mut u64;
        write_volatile(dcbaa.add(state.slot_id as usize), out_ctx.phys());
    }

    let mut addr_dev = Trb::ZERO;
    addr_dev.param = input_ctx.phys();
    addr_dev.control = TRB_ADDRESS_DEVICE | ((state.slot_id as u32) << 24);
    addr_dev
}

/// The Configure Endpoint command for whichever function this device is, with
/// the transfer rings it will run recorded on the way through.
///
/// `None` where the driver has no room for the device — a disk past the pool's
/// mass-storage blocks — which is a refusal and not a failure of the command.
fn configure_endpoint_trb(ctrl: &mut XhciController, state: &mut Enumerating) -> Option<Trb> {
    let (_, function) = state.parsed.expect("a configuration named a function");
    let rings = match function {
        Function::Hid(info) => Rings::Hid(hid_input_context(ctrl, state, &info)),
        Function::Msc(info) => Rings::Msc(super::msc::prepare(
            ctrl, state.slot_id, state.speed, state.port_idx, &info,
        )?),
    };
    state.rings = Some(rings);

    let mut configure = Trb::ZERO;
    configure.param = ctrl.dma().subslice(OFF_INPUT_CTX, PAGE).phys();
    configure.control = TRB_CONFIGURE_EP | ((state.slot_id as u32) << 24);
    Some(configure)
}

/// The input context for one HID interrupt endpoint, and the ring it runs on.
fn hid_input_context(
    ctrl: &mut XhciController,
    state: &Enumerating,
    info: &HidInterfaceInfo,
) -> TrbRing {
    let dma = ctrl.dma();
    let int_ep_dci = info.ep.dci();
    let int_ring = TrbRing::init(dma.subslice(state.block + DEV_INT_RING, PAGE));

    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    unsafe { input_ctx.zero(); }

    ctrl.write_ctx32(input_ctx_ptr, 0, 1, (1u32 << (int_ep_dci as u32)) | 1);

    let slot_dw0 = ((state.speed as u32) << 20) | ((int_ep_dci as u32) << 27);
    ctrl.write_ctx32(input_ctx_ptr, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx_ptr, 1, 1, (state.port_idx as u32 + 1) << 16);

    let ep_ctx_index = int_ep_dci as usize + 1;
    let interval_val = if info.ep.interval == 0 { 0u32 } else if state.speed <= 2 {
        let frames = (info.ep.interval as u32) * 8;
        let mut exp = 0u32;
        let mut v = frames;
        while v > 1 { v >>= 1; exp += 1; }
        exp
    } else {
        (info.ep.interval - 1) as u32
    };
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 0, interval_val << 16);

    let ep_dw1 = (3u32 << 1) | (7u32 << 3) | ((info.ep.max_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 1, ep_dw1);

    let int_dequeue = int_ring.dequeue();
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 2, int_dequeue as u32);
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 3, (int_dequeue >> 32) as u32);
    // Max ESIT Payload in the high half, Average TRB Length in the low. The
    // first is what an xHC allocates periodic bandwidth from — xHCI 1.2 §6.2.3.8
    // defines it as the bytes this endpoint moves per service interval, and
    // §4.14.2 makes it the term the scheduler uses — and this dword used to be a
    // flat 8 copied from EP0's, where a control endpoint has no Max ESIT Payload
    // and 8 is the Average TRB Length of a setup stage. So a periodic endpoint
    // was declaring that it moves nothing. For a low- or full-speed interrupt
    // endpoint there is one burst of one packet, so both halves are the max
    // packet size, which is what Linux's `xhci_endpoint_init` writes.
    let esit = info.ep.max_packet as u32;
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 4, (esit << 16) | esit);
    int_ring
}

/// Everything class-specific, which is where this sequence stops and the
/// device's own driver starts.
fn bind(ctrl: &mut XhciController, state: Enumerating) {
    let (_, function) = state.parsed.expect("a configuration named a function");
    let rings = state.rings.expect("Configure Endpoint named this device's rings");
    match (function, rings) {
        (Function::Msc(info), Rings::Msc(msc)) => {
            super::msc::bind(ctrl, state.ep0_ring, state.slot_id, state.block, msc, &info);
        }
        (Function::Hid(info), Rings::Hid(int_ring)) => {
            bind_hid(ctrl, &state, &info, int_ring);
        }
        // The rings are built from the function two acts earlier and nothing
        // between the two can change it, so a mismatch is a driver that lost
        // track of which device it is enumerating.
        _ => unreachable!("the rings were built for another function"),
    }
    finish(ctrl, state.port_idx, Some(state.slot_id));
}

fn bind_hid(
    ctrl: &mut XhciController,
    state: &Enumerating,
    info: &HidInterfaceInfo,
    int_ring: TrbRing,
) {
    let report = ctrl.dma().subslice(state.block + DEV_REPORT, 8);
    let report_size = match info.protocol {
        HidType::Keyboard => 8,
        HidType::Mouse => 4,
        HidType::Tablet => 6,
    };
    let role = match info.protocol {
        HidType::Keyboard => HidRole::Keyboard,
        // A pointer with no entry in the button table cannot be bound: it
        // would have to share another device's, and then each report of one
        // publishes the other's buttons as released.
        HidType::Mouse | HidType::Tablet => match crate::mouse::PointerSource::claim() {
            Some(source) => HidRole::Pointer(source),
            None => {
                log!("xHCI: slot {} is past the pointers this machine can number, dropping it",
                    state.slot_id);
                return;
            }
        },
    };
    let mut dev = HidDevice {
        slot_id: state.slot_id,
        port_idx: state.port_idx,
        block: state.block,
        int_ep_dci: info.ep.dci(),
        ep_addr: info.ep.addr,
        int_ring,
        ep0_ring: state.ep0_ring,
        report_phys: report.phys(),
        report_ptr: report.base(),
        report_size,
        role,
        prev_report: [0; 8],
        broke_with: None,
        failures: 0,
        completions: 0,
    };

    dev.requeue(&ctrl.db_base);
    // The ring offset is in the line because two devices of one class landing
    // on one ring is invisible from every other angle: both still enumerate,
    // both still bind, and both still deliver until their TRBs interleave.
    log!("xHCI: USB {} ready on slot {}, int_ring +{:#x}",
        hid_kind(info.protocol), state.slot_id, state.block + DEV_INT_RING);
    // The same argument one level up, and the only place the merge is visible:
    // two pointers on two controllers both have a slot 1, so a source derived
    // from the slot id would be one entry and each report would publish the
    // other device's buttons as released.
    if let HidRole::Pointer(source) = dev.role {
        log!("xHCI: pointer on slot {} merges as source {}", state.slot_id, source.id());
    }
    ctrl.devices.push(dev);
}

/// The enumeration is over, however it went.
///
/// `slot` is recorded **including on every path that then refused the device**:
/// the slot is the controller's resource from the moment Enable Slot answers
/// and only Disable Slot gives it back, so a port that forgot one would leak it
/// for the life of the boot.
///
/// The acknowledge is the port's own change flags — not PRC, which `begin`
/// cleared, but any flag left set on a port that is now quiet: the next thing
/// to happen here is the device being pulled, and a CSC that is already '1' is
/// a disconnect the controller cannot report.
pub(super) fn finish(ctrl: &mut XhciController, port_idx: u8, slot: Option<u8>) {
    ctrl.ports[port_idx as usize].enumerated(slot.and_then(NonZeroU8::new));
    ctrl.acknowledge_port_read(port_idx);
}

/// Drop an enumeration outstanding for a port whose device has gone, for the
/// reason a recovery is dropped: the device it is talking to will not answer,
/// and the teardown behind it would spend a deadline per remaining act finding
/// that out. The slot goes to the port, which is what makes the teardown
/// disable it.
///
/// The Enable Slot that has not answered yet is deliberately left alone: its
/// answer *is* the slot id, and a driver that stopped listening for it would
/// leak a Device Slot the controller has already allocated.
pub(super) fn cancel_on(ctrl: &mut XhciController, port_idx: u8) {
    let Some(What::Enumerating(state)) = ctrl.outstanding.what() else { return };
    if state.port_idx != port_idx {
        return;
    }
    let slot_id = state.slot_id;
    ctrl.outstanding.cancel();
    log!("xHCI: the enumeration on port {} is abandoned; its device has gone", port_idx + 1);
    // The slot and nothing else. [`finish`]'s acknowledge would clear the very
    // change flag that brought the caller here, and the teardown behind this
    // decides what a port means from exactly that word.
    ctrl.ports[port_idx as usize].enumerated(NonZeroU8::new(slot_id));
}

/// Configuration descriptors no device in reach will hand us.
///
/// Same reason `legacy::selftest` exists, and the same shape: `parse_config` is
/// a pure function over bytes a *device* chose, and every device QEMU can
/// attach describes itself correctly — so the refusals below have no boot to
/// point at. Each case's expected value is what the parser must decide, and the
/// two that matter are the endpoint addresses naming endpoint 0: they are
/// non-zero bytes, which is all the acceptance tests used to be, and they
/// resolve to the slot context and to EP0's.
#[cfg(feature = "boot-actuators")]
pub fn selftest() {
    /// (kind, config value, first DCI, second DCI); kind 1 is HID, 2 is mass
    /// storage. A tuple rather than the enum, because what is under test is the
    /// numbers the parser resolved and `Function` has no equality.
    type Verdict = Option<(u8, u8, u8, u8)>;

    fn summarise(got: Option<(u8, Function)>) -> Verdict {
        match got? {
            (cfg, Function::Hid(h)) => Some((1, cfg, h.ep.dci(), 0)),
            (cfg, Function::Msc(m)) => Some((2, cfg, m.in_ep.dci(), m.out_ep.dci())),
        }
    }

    /// A config descriptor whose `wTotalLength` is `total` and whose body is
    /// one interface followed by `eps`, each `(address, transfer type)`.
    fn build(buf: &mut [u8; 64], class: (u8, u8, u8), eps: &[(u8, u8)], total: u16) -> usize {
        buf.fill(0);
        buf[..9].copy_from_slice(&[9, 2, total as u8, (total >> 8) as u8, 1, 0x42, 0, 0, 0]);
        buf[9..18].copy_from_slice(&[9, 4, 0, 0, eps.len() as u8, class.0, class.1, class.2, 0]);
        let mut at = 18;
        for &(addr, transfer) in eps {
            buf[at..at + 7].copy_from_slice(&[7, 5, addr, transfer, 64, 0, 8]);
            at += 7;
        }
        at
    }

    const MSC: (u8, u8, u8) = (0x08, 0x06, 0x50);
    const KBD: (u8, u8, u8) = (3, 1, 1);
    const CASES: usize = 9;

    let mut passed = 0usize;
    let mut buf = [0u8; 64];
    let mut check = |name: &str, desc: &[u8], want: Verdict| {
        let got = summarise(parse_config(desc));
        if got == want {
            passed += 1;
        } else {
            log!("xHCI: descriptor selftest FAILED on {name}: got {got:?}, want {want:?}");
        }
    };

    // Bulk IN 0x81 is DCI 3, bulk OUT 0x02 is DCI 4.
    let len = build(&mut buf, MSC, &[(0x81, 2), (0x02, 2)], 32);
    check("an ordinary disk", &buf[..len], Some((2, 0x42, 3, 4)));

    // 0x80 is endpoint 0 IN, whose DCI is 1 — the control endpoint a Configure
    // Endpoint command must not add, and the ring `clear_stall` drives.
    let len = build(&mut buf, MSC, &[(0x80, 2), (0x02, 2)], 32);
    check("a bulk IN endpoint naming endpoint 0", &buf[..len], None);

    // 0x10 is endpoint 0 OUT, whose DCI is 0 — the slot context, holding the
    // speed and the root hub port this device was addressed on.
    let len = build(&mut buf, MSC, &[(0x81, 2), (0x10, 2)], 32);
    check("a bulk OUT endpoint naming endpoint 0", &buf[..len], None);

    // Interrupt rather than bulk: CBI, which this driver does not speak.
    let len = build(&mut buf, MSC, &[(0x81, 3), (0x02, 3)], 32);
    check("a mass-storage interface with no bulk pair", &buf[..len], None);

    let len = build(&mut buf, KBD, &[(0x81, 3)], 25);
    check("an ordinary keyboard", &buf[..len], Some((1, 0x42, 3, 0)));

    let len = build(&mut buf, KBD, &[(0x80, 3)], 25);
    check("a keyboard whose interrupt endpoint is endpoint 0", &buf[..len], None);

    // A zero length is the walk's one non-advancing step, and the reason it
    // terminates rather than reading the same descriptor forever.
    let len = build(&mut buf, KBD, &[(0x81, 3)], 25);
    buf[9] = 0;
    check("a descriptor claiming zero length", &buf[..len], None);

    // wTotalLength is the device's, so it is clamped to what was actually
    // requested; the interface before the lie is still found.
    let len = build(&mut buf, KBD, &[(0x81, 3)], u16::MAX);
    check("wTotalLength past the buffer", &buf[..len], Some((1, 0x42, 3, 0)));

    // The last descriptor runs off the end of what the device sent.
    let len = build(&mut buf, KBD, &[(0x81, 3)], 25);
    check("a truncated final descriptor", &buf[..len - 3], None);

    log!("xHCI: descriptor selftest {passed}/{CASES} configurations parsed as required");
}
