use core::ptr::{write_volatile, write_bytes};

use crate::log;
use super::{Control, Trb, TrbRing, XhciController, PAGE};
use super::{OFF_DCBAA, OFF_INPUT_CTX, OFF_DATA_BUF};
use super::{DEV_INT_RING, DEV_EP0_RING, DEV_OUT_CTX, DEV_REPORT};
use super::{TRB_ENABLE_SLOT, TRB_ADDRESS_DEVICE, TRB_CONFIGURE_EP, TRB_EVALUATE_CONTEXT};
use super::CC_SUCCESS;
use super::{PORTSC_CCS, PORTSC_PED, PORTSC_PR, PORTSC_PRC};
use super::hid::{HidType, HidRole, HidDevice};
use super::msc::MscInterface;

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
enum Function {
    Hid(HidInterfaceInfo),
    Msc(MscInterface),
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

/// EP0's Max Packet Size before the device has been asked, and `None` for a
/// speed this driver has no encoding for.
///
/// Low, High and SuperSpeed each fix it — 8, 64 and 512 — and a device of that
/// speed may report nothing else. **Full Speed does not.** `bMaxPacketSize0` is
/// 8, 16, 32 or 64 there, and it is not known until the first eight bytes of
/// the device descriptor have been read, which is itself a transfer over EP0.
/// 8 is the size every full-speed device can answer at, so it is what Address
/// Device carries and what Evaluate Context replaces once the device has said
/// (xHCI 1.2 §4.3.4; Linux does the same in `xhci_setup_addressable_virt_dev`).
///
/// The old table answered 64 for Full Speed and **8 for everything it did not
/// recognise**, so a SuperSpeedPlus port — speed 5, 6 or 7, which is every
/// Gen 2 and every two-lane link — was addressed with a 64-fold undersized
/// control endpoint and no line said so.
fn initial_ep0_packet(speed: u8) -> Option<u16> {
    match speed {
        1 | 2 => Some(8),
        3 => Some(64),
        4..=7 => Some(512),
        _ => None,
    }
}

/// What `bMaxPacketSize0` means at this speed, or `None` when the device named
/// a size a device of that speed does not have.
///
/// SuperSpeed states the *exponent*: the byte is 9 and the size is 512 (USB 3.2
/// §9.6.1). Everything below it states the size itself, and only Full Speed has
/// a choice to state.
fn ep0_packet_from_descriptor(speed: u8, stated: u8) -> Option<u16> {
    match (speed, stated) {
        (1, 8 | 16 | 32 | 64) => Some(stated as u16),
        (2, 8) => Some(8),
        (3, 64) => Some(64),
        (4..=7, 9) => Some(512),
        _ => None,
    }
}

/// Tell the controller the Max Packet Size of EP0 that only the device knew.
///
/// Evaluate Context rather than Configure Endpoint: §4.6.7 defines it as the
/// command that changes exactly this field on a device that is already
/// addressed, and the A1 flag alone is what says EP0 is the context to look at.
fn evaluate_ep0_packet(ctrl: &mut XhciController, slot_id: u8, max_packet: u16) -> bool {
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
    ctrl.run_command(evaluate, "Evaluate Context (EP0 packet size)")
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
/// ports take 55 ms over this (`specs/metal-hardware-inventory.md`), which is
/// the whole reason the runtime path must not hold a scheduler pass across it.
pub fn begin_reset(ctrl: &mut XhciController, port_idx: u8) {
    let portsc = ctrl.read_portsc(port_idx);
    ctrl.write_portsc(port_idx, super::portsc_neutral(portsc) | PORTSC_PR);
}

/// Whether the port has finished the reset it was asked for.
pub fn reset_done(ctrl: &XhciController, port_idx: u8) -> bool {
    super::PORT_ANSWERS && ctrl.read_portsc(port_idx) & PORTSC_PRC != 0
}

/// Initialize and configure one USB device on a port. `Some(slot)` is a slot
/// the controller enabled and the driver now owns — see [`configure`].
pub fn init_device(ctrl: &mut XhciController, port_idx: u8) -> Option<u8> {
    begin_reset(ctrl, port_idx);

    // A port that asserts CCS and then never asserts PRC — a device pulled
    // between the scan and the reset, a marginal cable, a reset the controller
    // will not run — costs that port and not the boot. This spin had no
    // deadline, on the boot CPU, before the scheduler exists and with nothing
    // logged.
    if !super::settles(|| reset_done(ctrl, port_idx)) {
        log!("xHCI: port {} never finished its reset (PORTSC {:#010x}); skipping it",
            port_idx + 1, ctrl.read_portsc(port_idx));
        return None;
    }
    configure(ctrl, port_idx)
}

/// Everything between a port that has just finished its reset and a device the
/// driver can use.
///
/// `Some(slot)` whenever the controller enabled one, **including every path
/// that then refuses the device**: the slot is the controller's resource from
/// that moment and only Disable Slot gives it back, so the caller has to be
/// told about one it would otherwise have no name for.
pub fn configure(ctrl: &mut XhciController, port_idx: u8) -> Option<u8> {
    let portsc = ctrl.read_portsc(port_idx);
    ctrl.write_portsc(port_idx, super::portsc_neutral(portsc) | PORTSC_PRC);

    let portsc = ctrl.read_portsc(port_idx);
    if portsc & PORTSC_PED == 0 {
        log!("xHCI: port {} reset but not enabled (PORTSC {portsc:#010x}); skipping it",
            port_idx + 1);
        return None;
    }
    let speed = ((portsc >> 10) & 0xF) as u8;
    log!("xHCI: port {} reset, speed={}", port_idx + 1, speed);
    let Some(initial_packet) = initial_ep0_packet(speed) else {
        log!("xHCI: port {} came up at speed {speed}, which is not a speed this driver has a \
             control-endpoint packet size for; skipping it", port_idx + 1);
        return None;
    };

    let mut enable_slot = Trb::ZERO;
    enable_slot.control = TRB_ENABLE_SLOT;
    ctrl.submit_command(enable_slot);
    let slot_id = match ctrl.wait_command() {
        Some((CC_SUCCESS, slot_id)) => slot_id as u8,
        Some((code, _)) => {
            log!("xHCI: Enable Slot failed, code={}", code);
            return None;
        }
        None => {
            log!("xHCI: Enable Slot timed out on port {}", port_idx + 1);
            return None;
        }
    };
    // A slot id is the controller's answer, not the driver's, and CONFIG's
    // MaxSlotsEn is only advisory to a controller that chooses to ignore it —
    // QEMU's does. Nothing of the driver's is written for this device yet, so
    // there is nothing here to unwind; the controller's own Device Slot is
    // already allocated, which is why every `return` below this line carries
    // the slot id back rather than dropping it.
    let Some(block) = ctrl.layout.device(slot_id) else {
        log!("xHCI: slot {} is beyond the pool's {} device blocks, dropping port {}",
            slot_id, ctrl.layout.dev_blocks, port_idx + 1);
        return Some(slot_id);
    };
    log!("xHCI: slot {} enabled (dma +{:#x})", slot_id, block);

    let dma = ctrl.dma();
    let mut ep0_ring = TrbRing::init(dma.subslice(block + DEV_EP0_RING, PAGE));
    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    let input_ctx_phys = input_ctx.phys();
    unsafe { input_ctx.zero(); }

    ctrl.write_ctx32(input_ctx_ptr, 0, 1, 0x3); // Add Slot + EP0
    let slot_dw0 = ((speed as u32) << 20) | (1u32 << 27);
    ctrl.write_ctx32(input_ctx_ptr, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx_ptr, 1, 1, (port_idx as u32 + 1) << 16);

    let ep0_dw1 = (3u32 << 1) | (4u32 << 3) | ((initial_packet as u32) << 16);
    ctrl.write_ctx32(input_ctx_ptr, 2, 1, ep0_dw1);
    let ep0_dequeue = ep0_ring.dequeue();
    ctrl.write_ctx32(input_ctx_ptr, 2, 2, ep0_dequeue as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 3, (ep0_dequeue >> 32) as u32);
    ctrl.write_ctx32(input_ctx_ptr, 2, 4, 8);

    let out_ctx = dma.subslice(block + DEV_OUT_CTX, PAGE / 2);
    unsafe { out_ctx.zero(); }
    unsafe {
        let dcbaa = dma.ptr_at(OFF_DCBAA) as *mut u64;
        write_volatile(dcbaa.add(slot_id as usize), out_ctx.phys());
    }

    let mut addr_dev = Trb::ZERO;
    addr_dev.param = input_ctx_phys;
    addr_dev.control = TRB_ADDRESS_DEVICE | ((slot_id as u32) << 24);
    if !ctrl.run_command(addr_dev, "Address Device") {
        return Some(slot_id);
    }
    log!("xHCI: device addressed");

    // Descriptor scratch, and shared for the same reason the input context is:
    // it is dead the moment this function returns. The report buffer below is
    // the one that is not, so it lives in the device's own block.
    let data_buf = dma.subslice(OFF_DATA_BUF, PAGE);
    let data_buf_ptr = data_buf.base();
    let data_buf_phys = data_buf.phys();

    // Eight bytes, and only eight. `bMaxPacketSize0` is the eighth of them and
    // on a full-speed device it is what decides whether a longer read can be
    // transferred at all — so the prefix is read at a size every device of the
    // speed can answer at and the endpoint context is corrected before the rest
    // of the descriptor is asked for.
    unsafe { write_bytes(data_buf_ptr, 0, MAX_CONFIG_DESC); }
    let got = ctrl.control_transfer(
        slot_id, &mut ep0_ring, 0x80, 0x06, 0x0100, 0, Some(data_buf_phys), 8,
    );
    if !got.moved(8) {
        log!("xHCI: port {} would not give up the first 8 bytes of its device descriptor: {got}",
            port_idx + 1);
        return Some(slot_id);
    }
    let stated = unsafe { *data_buf_ptr.add(7) };
    let Some(ep0_packet) = ep0_packet_from_descriptor(speed, stated) else {
        log!("xHCI: port {} states bMaxPacketSize0={stated}, which is not a control packet size a \
             speed-{speed} device has; skipping it", port_idx + 1);
        return Some(slot_id);
    };
    if ep0_packet != initial_packet {
        log!("xHCI: port {} EP0 packet size {initial_packet} -> {ep0_packet}", port_idx + 1);
        if !evaluate_ep0_packet(ctrl, slot_id, ep0_packet) {
            return Some(slot_id);
        }
    }

    unsafe { write_bytes(data_buf_ptr, 0, MAX_CONFIG_DESC); }
    let got = ctrl.control_transfer(
        slot_id, &mut ep0_ring, 0x80, 0x06, 0x0100, 0, Some(data_buf_phys), 18,
    );
    if !got.moved(18) {
        log!("xHCI: GET_DESCRIPTOR(Device) on port {}: {got}", port_idx + 1);
        return Some(slot_id);
    }
    let descriptor = unsafe { core::slice::from_raw_parts(data_buf_ptr, 18) };
    log!("xHCI: device class={:#x} vendor={:04x} product={:04x}",
        descriptor[4], le16(descriptor, 8), le16(descriptor, 10));

    unsafe { write_bytes(data_buf_ptr, 0, MAX_CONFIG_DESC); }
    let got = ctrl.control_transfer(
        slot_id, &mut ep0_ring, 0x80, 0x06, 0x0200, 0,
        Some(data_buf_phys), MAX_CONFIG_DESC as u16,
    );
    // Nine bytes is a configuration descriptor's own header, which is where
    // `wTotalLength` lives; fewer than that is not one. The parser is then
    // bounded by what *arrived* rather than by what was asked for, so the
    // zeroes behind a short answer are never walked as descriptors.
    let Control::Done { delivered } = got else {
        log!("xHCI: GET_DESCRIPTOR(Config) on port {}: {got}", port_idx + 1);
        return Some(slot_id);
    };
    if delivered < 9 {
        log!("xHCI: port {} answered {delivered} B to GET_DESCRIPTOR(Config); a configuration \
             descriptor is at least 9", port_idx + 1);
        return Some(slot_id);
    }
    let config = unsafe { core::slice::from_raw_parts(data_buf_ptr, delivered as usize) };

    let (config_val, function) = match parse_config(config) {
        Some(f) => f,
        None => {
            log!("xHCI: no HID boot interface found, skipping");
            return Some(slot_id);
        }
    };

    let set = ctrl.control_transfer(
        slot_id, &mut ep0_ring, 0x00, 0x09, config_val as u16, 0, None, 0,
    );
    if !set.done() {
        log!("xHCI: SET_CONFIGURATION on port {}: {set}", port_idx + 1);
        return Some(slot_id);
    }
    log!("xHCI: configuration set");

    let info = match function {
        Function::Msc(msc) => {
            log!("xHCI: mass storage iface={} in={:#x}/{} out={:#x}/{}",
                msc.iface_num, msc.in_ep.addr, msc.in_ep.max_packet,
                msc.out_ep.addr, msc.out_ep.max_packet);
            super::msc::bind(ctrl, ep0_ring, slot_id, block, speed, port_idx, &msc);
            return Some(slot_id);
        }
        Function::Hid(info) => info,
    };

    let kind = match info.protocol {
        HidType::Keyboard => "keyboard",
        HidType::Mouse => "mouse",
        HidType::Tablet => "tablet",
    };
    let int_ep_dci = info.ep.dci();
    log!("xHCI: HID {} iface={} ep={:#x} max_pkt={} interval={} dci={}",
        kind, info.iface_num, info.ep.addr, info.ep.max_packet, info.ep.interval, int_ep_dci);

    // SET_PROTOCOL (boot protocol) — only for boot-interface devices
    if info.protocol != HidType::Tablet {
        let set = ctrl.control_transfer(
            slot_id, &mut ep0_ring, 0x21, 0x0B, 0, info.iface_num as u16, None, 0,
        );
        if !set.done() {
            log!("xHCI: SET_PROTOCOL on port {}: {set}", port_idx + 1);
        }
    }

    let report = dma.subslice(block + DEV_REPORT, 8);
    let report_phys = report.phys();
    let report_ptr = report.base();

    let int_ring = TrbRing::init(dma.subslice(block + DEV_INT_RING, PAGE));

    let input_ctx = dma.subslice(OFF_INPUT_CTX, PAGE);
    let input_ctx_ptr = input_ctx.base();
    let input_ctx_phys = input_ctx.phys();
    unsafe { input_ctx.zero(); }

    ctrl.write_ctx32(input_ctx_ptr, 0, 1, (1u32 << (int_ep_dci as u32)) | 1);

    let slot_dw0 = ((speed as u32) << 20) | ((int_ep_dci as u32) << 27);
    ctrl.write_ctx32(input_ctx_ptr, 1, 0, slot_dw0);
    ctrl.write_ctx32(input_ctx_ptr, 1, 1, (port_idx as u32 + 1) << 16);

    let ep_ctx_index = int_ep_dci as usize + 1;
    let interval_val = if info.ep.interval == 0 { 0u32 } else if speed <= 2 {
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
    ctrl.write_ctx32(input_ctx_ptr, ep_ctx_index, 4, 8);

    let mut config_ep = Trb::ZERO;
    config_ep.param = input_ctx_phys;
    config_ep.control = TRB_CONFIGURE_EP | ((slot_id as u32) << 24);
    if !ctrl.run_command(config_ep, "Configure Endpoint") {
        return Some(slot_id);
    }
    log!("xHCI: endpoint configured");

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
                    slot_id);
                return Some(slot_id);
            }
        },
    };
    let mut dev = HidDevice {
        slot_id,
        port_idx,
        int_ep_dci,
        int_ring,
        report_phys,
        report_ptr,
        report_size,
        role,
        prev_report: [0; 8],
    };

    dev.requeue(&ctrl.db_base);
    // The ring offset is in the line because two devices of one class landing
    // on one ring is invisible from every other angle: both still enumerate,
    // both still bind, and both still deliver until their TRBs interleave.
    log!("xHCI: USB {} ready on slot {}, int_ring +{:#x}", kind, slot_id, block + DEV_INT_RING);
    // The same argument one level up, and the only place the merge is visible:
    // two pointers on two controllers both have a slot 1, so a source derived
    // from the slot id would be one entry and each report would publish the
    // other device's buttons as released.
    if let HidRole::Pointer(source) = dev.role {
        log!("xHCI: pointer on slot {} merges as source {}", slot_id, source.id());
    }
    ctrl.devices.push(dev);
    Some(slot_id)
}

/// Scan all ports on the controller and initialize connected devices.
/// Enumeration is serial by construction, which is what lets the input
/// context, the EP0 ring and the descriptor buffer be one each. Serial does not
/// mean quiet: a device bound on an earlier port is armed and delivering while
/// a later port enumerates, so the event ring carries its completions too and
/// both waits demux by slot id rather than by TRB type alone.
///
/// The root hubs must have settled first — [`super::await_connect_settle`] is
/// what makes `PORTSC.CCS` a question with an answer.
pub fn scan_ports(ctrl: &mut XhciController) {
    for p in 0..ctrl.max_ports {
        // No speed here, deliberately: §4.19.5 says the Port Speed field "shall
        // not be considered valid by software until after the PR bit transitions
        // from a '1' to a '0'". QEMU fills it in from the QOM tree before any
        // reset, so a line printing it here reads as fact on QEMU and as noise
        // on hardware. The `port N reset, speed=` line below is the valid one.
        if ctrl.read_portsc(p) & PORTSC_CCS != 0 {
            log!("xHCI: port {} connected", p + 1);
            let slot = init_device(ctrl, p);
            ctrl.port_bound(p, slot);
        }
    }
    // Every change bit the scan left set, on every port. A change flag is what
    // *raises* a Port Status Change Event, and it raises one only as it goes
    // from 0 to 1 — so a CSC still set from the boot-time attach is a
    // disconnect the controller has no way to report, and the first thing
    // unplugged after boot would go unnoticed.
    ctrl.acknowledge_port_changes();
    #[cfg(feature = "xhci-portsc-rw1c")]
    log!("xHCI: PED as RW1C, {} port(s) disabled by a driver write",
        ctrl.software_disabled_ports());
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
#[cfg(feature = "xhci-descriptor-selftest")]
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
