//! Bringing every controller on the machine up, and the one scan that happens
//! before there is a scheduler.
//!
//! Below [`super`] because every register settle and every enumeration act here
//! is waited for in place — which is correct exactly once, and this is it: the
//! pass a submit-and-return would give itself back to does not exist yet.

use alloc::vec::Vec;
use core::ptr::write_volatile;
use core::sync::atomic::Ordering;

use crate::log;
use crate::mm::paging::CachePolicy;
use crate::mm::Mmio;
use crate::drivers::pci::PciDevice;
use crate::drivers::DmaPool;
use super::super::{device, legacy};
use super::super::{storage_count, DEV_STRIDE, MSC_BLOCKS, PortState, Outstanding, Trb};
use device::{begin, reset_done, reset_port};
use super::super::{ErstEntry, Layout, MscBlock, PortMask, Portsc, Protocols, TrbRing};
use super::super::{XhciController, PAGE, RING_SIZE, USB_TIMEOUT_NS};
use super::super::{CAP_CAPLENGTH, CAP_DBOFF, CAP_HCCPARAMS1, CAP_HCSPARAMS1, CAP_HCSPARAMS2};
use super::super::{CAP_RTSOFF, HCC_PPC, XHCI_VECTOR};
use super::super::{IR0_ERDP, IR0_ERSTBA, IR0_ERSTSZ, IR0_IMAN, IR0_IMOD};
use super::super::{OFF_CMD_RING, OFF_DCBAA, OFF_ERST, OFF_EVT_RING};
use super::super::{OP_CONFIG, OP_CRCR, OP_DCBAAP, OP_PAGESIZE, OP_PORT_BASE, OP_USBCMD, OP_USBSTS};
use super::super::{PORTSC_PP, PORT_REG_SIZE, PORT_WORK_AT, XHCI};
use super::super::{CONTROLLER_ANSWERS, PORT_DEBOUNCE_NS};
use super::settles;
use toyos_xhci::port::{self, Reset};
use toyos_xhci::Protocol;

/// How long a machine on which *nothing at all* has connected keeps looking.
///
/// The debounce above cannot answer this on its own, and that is not a detail:
/// an empty port set has been "stable" since the instant power was applied, so
/// a settle written only as "wait for the set to hold still" returns
/// immediately on exactly the machine this code exists for. A device that is
/// slow to appear and a bus with nothing on it are the same reading until one
/// of them changes, so the only way to tell them apart without hotplug is to
/// keep looking.
///
/// The asymmetry is deliberate: this is paid **only** by a machine that would
/// otherwise report an empty bus, which is the outcome that cost the T14 its
/// `/boot`. Any machine with one USB device anywhere settles on the debounce.
///
/// One second is policy, not physics. It covers the longest detection path a
/// spec puts a number on — a SuperSpeed link that fails to train spends
/// `tPollingLFPSTimeout` (360 ms, USB 3.2 §7.5.4.3) before it falls back, and
/// the USB2 connect and debounce behind that add ~100 ms — and it sits under
/// Linux's `HUB_DEBOUNCE_TIMEOUT`, which is 2000 ms in `drivers/usb/core/hub.c`.
const EMPTY_BUS_NS: u64 = 1_000_000_000;

/// When the driver stops waiting for a root hub that keeps changing its mind.
///
/// Policy, and under Linux's `HUB_DEBOUNCE_TIMEOUT`, which is 2000 ms in
/// `drivers/usb/core/hub.c`. What the caller sees when it is hit is a line
/// naming the machine's port state and a scan of whatever is connected at that
/// moment — a flapping port costs the boot a bounded second and a half, never
/// the machine.
pub const PORT_SETTLE_CEILING_NS: u64 = 1_500_000_000;

/// How often the settle re-reads the port registers. Each pass is one MMIO read
/// per port, so on the widest controller in reach this is 16 reads per
/// millisecond of the debounce.
pub const PORT_POLL_NS: u64 = 1_000_000;
/// Wait for every root hub on the machine to stop changing its mind.
///
/// **`PORTSC.CCS` is not a question that can be asked at an instant.** HCRST
/// returns every port to the state it has with nothing attached (spec §4.19.1.1
/// for USB2, §4.19.1.2 for USB3), so a device firmware had already enumerated
/// has to be detected all over again — and detection is a physical process:
/// port power settling, a USB2 pull-up being debounced, a USB3 link running
/// receiver detection and training. A scan issued in the same microsecond as
/// `USBCMD.R/S` reports an empty bus on any machine whose ports are real. That
/// is what the T14 did on both of its controllers while booting off a stick
/// plugged into one of them: `controller started` and `no HID devices` share a
/// millisecond in its log and no `port N connected` line sits between them.
///
/// QEMU's controller has no port state machine and no timer. `xhci_reset()`
/// calls `xhci_port_update()` for every port, which assigns PORTSC from the QOM
/// tree — CCS, CSC, PP, the speed, and PED for a SuperSpeed device — so the
/// register is in its terminal state before the guest's first MMIO access. That
/// is the whole reason a driver that never waited here passed every test in
/// this suite.
///
/// Machine-wide rather than per controller because the wait is wall-clock: a
/// laptop with two xHCs would otherwise pay for an interval both of them were
/// already inside. On the T14 that is the difference between one debounce and
/// two.
fn await_connect_settle(controllers: &[XhciController]) {
    let Some(powered_at) = controllers.iter().map(|c| c.powered_at).max() else { return };
    let mut seen: Vec<(PortMask, u64)> = controllers
        .iter()
        .map(|c| (c.connected_ports(), c.powered_at))
        .collect();

    loop {
        let now = crate::clock::nanos_since_boot();
        let empty = seen.iter().all(|(mask, _)| *mask == [0u64; 4]);
        let debounced = seen
            .iter()
            .all(|(_, at)| now.saturating_sub(*at) >= PORT_DEBOUNCE_NS);
        let looked_long_enough = !empty || now.saturating_sub(powered_at) >= EMPTY_BUS_NS;
        if debounced && looked_long_enough {
            return;
        }
        if now.saturating_sub(powered_at) >= PORT_SETTLE_CEILING_NS {
            log!("xHCI: no root hub on this machine held one connect state for {} ms within \
                 {} ms; enumerating whatever is connected now",
                PORT_DEBOUNCE_NS / 1_000_000, PORT_SETTLE_CEILING_NS / 1_000_000);
            return;
        }

        let next = now + PORT_POLL_NS;
        while crate::clock::nanos_since_boot() < next {
            core::hint::spin_loop();
        }
        for (ctrl, (mask, changed_at)) in controllers.iter().zip(seen.iter_mut()) {
            let now_mask = ctrl.connected_ports();
            if now_mask != *mask {
                *mask = now_mask;
                *changed_at = crate::clock::nanos_since_boot();
            }
        }
    }
}

/// Point this controller's interrupts at [`XHCI_VECTOR`] and name the
/// mechanism that took them, or `None` when the function offers neither.
///
/// `None` has to be a refusal and not a degradation, and that is the whole
/// shape of this function. Every read of an event ring in this driver is
/// `poll_if_pending`, which runs only behind an `irq_ring` record that
/// nothing but vector 0x21's ISR publishes — so a controller whose messages
/// cannot reach a CPU is one whose ring is never read again. This used to log
/// "no MSI-X capability, using polled mode" and carry on: there is no polled
/// mode, and every device on such a controller enumerated, logged itself
/// ready, and delivered nothing for the life of the boot.
fn arm_interrupt(pci_dev: &PciDevice) -> Option<&'static str> {
    if pci_dev.enable_msix(XHCI_VECTOR) {
        return Some("MSI-X");
    }
    pci_dev.enable_msi(XHCI_VECTOR).then_some("MSI")
}

/// Bring up every xHCI controller on the machine.
///
/// Every one, not the first: a Tiger Lake laptop has two — the Thunderbolt
/// block's at 00:0d.0 and the PCH's at 00:14.0, identical in class, subclass
/// and prog_if — and its keyboard and USB-A ports are on the second. Taking
/// the first match reported that the T14 had no USB HID at all, which was true
/// of that controller and false of the machine.
pub fn init(devices: &[PciDevice]) {
    // Once for the machine, not once per controller: it reads no register and
    // touches no device, so a second run would say the same thing twice.
    #[cfg(feature = "xhci-descriptor-selftest")]
    device::selftest();

    // Every controller is brought up and its ports powered before any of them
    // is scanned, because the scan cannot start until the root hub has settled
    // and that wait is wall-clock. Interleaving bring-up with enumeration would
    // make a machine with two controllers pay `PORT_DEBOUNCE_NS` twice for a
    // interval both of them were already inside.
    let mut controllers = Vec::new();
    let mut present = 0;
    for pci_dev in devices.iter().filter(|d| d.matches_class(0x0C, 0x03, Some(0x30))) {
        present += 1;
        if let Some(ctrl) = init_one(pci_dev) {
            controllers.push(ctrl);
        }
    }

    await_connect_settle(&controllers);

    for ctrl in controllers.iter_mut() {
        scan_ports(ctrl);
        if ctrl.devices.is_empty() {
            log!("xHCI: no HID devices on the controller at {:02x}:{:02x}.{}",
                ctrl.pci.bus, ctrl.pci.dev, ctrl.pci.func);
        }
    }

    if controllers.is_empty() {
        // A machine with no xHC and a machine whose xHCs this driver refused
        // are different machines, and the second used to print the first's
        // line. The per-controller refusal above says why; this says that
        // nothing was left.
        match present {
            0 => log!("xHCI: no controller on this machine, USB input unavailable"),
            n => log!("xHCI: {n} controller(s) present, none of them usable, USB unavailable"),
        }
        return;
    }
    // Nothing is outstanding out of a boot scan — every port it looked at it
    // acted on — so a machine that is never plugged into pays nothing for
    // hotplug beyond one atomic load per pass.
    PORT_WORK_AT.store(0, Ordering::Relaxed);
    let hid: usize = controllers.iter().map(|c| c.devices.len()).sum();
    log!("xHCI: {} controller(s), {} HID device(s)", controllers.len(), hid);
    log!("usb-storage: {} device(s)", storage_count());
    *XHCI.lock() = controllers;
}

/// What each of this controller's port registers speaks, out of its own
/// Supported Protocol capabilities (§7.2).
///
/// **A controller that says nothing leaves every port unknown**, and unknown is
/// driven the USB2 way — which is what every port got before this was read at
/// all, so a controller this cannot describe is no worse off than it was.
fn read_protocols(
    bar: &Mmio,
    bar_size: u64,
    hccparams1: u32,
    max_ports: u8,
    pci_dev: &PciDevice,
) -> Protocols {
    let read = |offset: u64| -> Option<u32> {
        (offset.checked_add(4)? <= bar_size).then(|| bar.read_u32(offset))
    };
    let mut protocols = Protocols::UNKNOWN;
    let mut refused = 0;
    let walked = legacy::for_each(
        &read,
        hccparams1 >> 16,
        legacy::CAP_ID_PROTOCOL,
        &mut |at| {
            let dwords = (read(at), read(at + 4), read(at + 8));
            let (Some(dw0), Some(dw1), Some(dw2)) = dwords else {
                refused += 1;
                return;
            };
            match toyos_xhci::protocol::SupportedProtocol::decode(dw0, dw1, dw2, max_ports) {
                Ok(found) => {
                    log!("xHCI: USB {}.{:x} on ports {}..={}", found.major, found.minor >> 4,
                        found.first_port + 1, found.first_port + found.port_count);
                    protocols.record(&found);
                }
                Err(why) => {
                    refused += 1;
                    log!("xHCI: a Supported Protocol capability at {at:#x} is unusable: {why:?}");
                }
            }
        },
    );
    if let Err(why) = walked {
        log!("xHCI: the capability list at PCI {:02x}:{:02x}.{} does not walk: {why:?}",
            pci_dev.bus, pci_dev.dev, pci_dev.func);
    }
    let (usb2, usb3) = protocols.counts(max_ports);
    // The line that says whether this machine's SuperSpeed ports are known to
    // be SuperSpeed. A zero here on a controller that has them is the T14's
    // failure waiting to happen, and it used to be invisible.
    log!("xHCI: {usb2} USB2 and {usb3} USB3 port register(s) of {max_ports} named, \
         {refused} capability(ies) refused");
    protocols
}

fn init_one(pci_dev: &PciDevice) -> Option<XhciController> {
    log!("xHCI: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    let bar_addr = pci_dev.read_bar_64(0);
    pci_dev.enable_bus_master();
    log!("xHCI: BAR0={:#x}", bar_addr);

    // Ahead of the reset and ahead of the port scan, because a controller
    // whose interrupts cannot be delivered must not reach either: the reset
    // is what makes it ours, and the port scan is what prints
    // `USB keyboard ready`. Refusing here leaves the controller exactly as
    // firmware left it, with nothing enumerated on it to claim otherwise.
    let Some(irq) = arm_interrupt(pci_dev) else {
        log!(
            "xHCI: NOT INITIALISED at PCI {:02x}:{:02x}.{} — the controller offers neither \
             MSI-X nor MSI, and this driver has no other way to be told it has anything to \
             say. No USB device on it can be used.",
            pci_dev.bus, pci_dev.dev, pci_dev.func
        );
        return None;
    };
    log!("xHCI: {irq} enabled (vector {XHCI_VECTOR:#x})");

    let bar = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(bar_addr, 0x10000, CachePolicy::DeferToMtrr);

    let cap_length = bar.read_u8(CAP_CAPLENGTH) as u64;
    let hcsparams1 = bar.read_u32(CAP_HCSPARAMS1);
    let hcsparams2 = bar.read_u32(CAP_HCSPARAMS2);
    let hccparams1 = bar.read_u32(CAP_HCCPARAMS1);
    let db_offset = (bar.read_u32(CAP_DBOFF) & !0x3) as u64;
    let rts_offset = (bar.read_u32(CAP_RTSOFF) & !0x1F) as u64;

    let max_slots = (hcsparams1 & 0xFF) as u8;
    let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;
    let csz = ((hccparams1 >> 2) & 1) != 0;
    let context_size: usize = if csz { 64 } else { 32 };

    // Everything below refuses this controller by name rather than taking the
    // machine with it. Two controllers is the target laptop's shape, and a
    // property of the empty Thunderbolt one is no reason the PCH's ports should
    // not come up.
    let refuse = |why: core::fmt::Arguments| {
        log!("xHCI: NOT INITIALISED at PCI {:02x}:{:02x}.{} — {why}. No USB device on it can \
             be used.", pci_dev.bus, pci_dev.dev, pci_dev.func);
    };

    // The BAR is mapped at a fixed 64 KiB and both offsets are the controller's
    // own 32-bit numbers, so this is where a controller that puts its doorbells
    // or its runtime registers outside the window has to be refused: the
    // subtraction below it underflows, and with overflow checks off it wraps
    // back to exactly `bar_size`, which `Mmio::subregion`'s own assertion then
    // accepts — an `Mmio` based outside the mapping, faulting on the first
    // doorbell write.
    let bar_size = 0x10000u64;
    let (Some(db_len), Some(rt_len)) =
        (bar_size.checked_sub(db_offset), bar_size.checked_sub(rts_offset))
    else {
        refuse(format_args!(
            "DBOFF={db_offset:#x} RTSOFF={rts_offset:#x} put its registers outside the \
             {bar_size:#x} window this driver maps"
        ));
        return None;
    };
    let op_base = bar.subregion(cap_length, bar_size - cap_length);
    let db_base = bar.subregion(db_offset, db_len);
    let rt_base = bar.subregion(rts_offset, rt_len);

    let pagesize = op_base.read_u32(OP_PAGESIZE) & 0xFFFF;
    log!("xHCI: max_slots={} max_ports={} ctx_size={} pagesize={:#x}",
        max_slots, max_ports, context_size, pagesize);
    // Bit 0 is 4 KiB, and it is the only bit this driver can use — the register
    // is a mask of the page sizes the controller supports, so the test is that
    // the bit is set and not that it is alone. The scratchpad is the whole
    // exposure: its entries are one PAGE apart, so a controller placing them at
    // 8 KiB writes each buffer over the next and the last one past `dev_base`
    // into block 0's interrupt ring — memory corruption with no diagnostic.
    // Every other consequence runs the safe way, since a larger page size only
    // relaxes the rule that the DCBAA and the contexts must not cross one.
    if pagesize & 1 == 0 {
        refuse(format_args!(
            "PAGESIZE={pagesize:#x} does not include 4 KiB, and every ring, context and \
             scratchpad buffer here is placed at 4 KiB"
        ));
        return None;
    }

    let max_sp_hi = ((hcsparams2 >> 21) & 0x1F) as usize;
    let max_sp_lo = ((hcsparams2 >> 27) & 0x1F) as usize;
    let layout = Layout::new((max_sp_hi << 5) | max_sp_lo, max_slots);
    log!("xHCI: dma {} KiB: scratchpad={} device blocks={} of {} B (max_slots={})",
        layout.pool_size / 1024, layout.scratch_count, layout.dev_blocks, DEV_STRIDE, max_slots);

    // Before the controller is touched at all: on a PC the firmware may still
    // own it for legacy keyboard emulation, and resetting a controller SMM is
    // driving is a fight with no diagnostic.
    legacy::take_ownership(&bar, bar_size, hccparams1);
    let protocols = read_protocols(&bar, bar_size, hccparams1, max_ports, pci_dev);

    let usbcmd = op_base.read_u32(OP_USBCMD);
    if usbcmd & 1 != 0 {
        op_base.write_u32(OP_USBCMD, usbcmd & !1);
    }
    let deadline_ms = USB_TIMEOUT_NS / 1_000_000;
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBSTS) & 1 != 0) {
        refuse(format_args!("it never halted, within {deadline_ms} ms of being asked to"));
        return None;
    }

    op_base.write_u32(OP_USBCMD, 1 << 1);
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBCMD) & (1 << 1) == 0) {
        refuse(format_args!("it held HCRST for {deadline_ms} ms"));
        return None;
    }
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBSTS) & (1 << 11) == 0) {
        refuse(format_args!("it stayed Controller Not Ready for {deadline_ms} ms after its reset"));
        return None;
    }
    log!("xHCI: controller reset");

    // After the reset, so a controller refused above costs no physical memory
    // at all — and the pool is freed with the `DmaPool` on every refusal below,
    // since `PhysPage` gives its page back when dropped.
    let pool = DmaPool::alloc(layout.pool_size);

    // MaxSlotsEn is what the driver can track, not what the controller can
    // offer: a conformant xHC then refuses Enable Slot past it rather than
    // handing back an id with nowhere to put its context.
    op_base.write_u32(OP_CONFIG, layout.dev_blocks as u32);

    let dma = pool.slice();
    unsafe { dma.zero(); }

    if layout.scratch_count > 0 {
        let array = dma.ptr_at(layout.scratch_array) as *mut u64;
        for i in 0..layout.scratch_count {
            let buf = dma.phys() + (layout.scratch_buffers + i * PAGE) as u64;
            unsafe { write_volatile(array.add(i), buf); }
        }
        unsafe {
            write_volatile(
                dma.ptr_at(OFF_DCBAA) as *mut u64,
                dma.phys() + layout.scratch_array as u64,
            );
        }
        log!("xHCI: {} scratchpad buffers configured", layout.scratch_count);
    }

    op_base.write_u64(OP_DCBAAP, dma.phys() + OFF_DCBAA as u64);

    let cmd_ring = TrbRing::init(dma.subslice(OFF_CMD_RING, PAGE));
    op_base.write_u64(OP_CRCR, dma.phys() + OFF_CMD_RING as u64 | 1);

    let evt_ring_buf = dma.subslice(OFF_EVT_RING, PAGE);
    let erst = dma.ptr_at(OFF_ERST) as *mut ErstEntry;
    unsafe {
        write_volatile(erst, ErstEntry {
            ring_base: evt_ring_buf.phys(),
            ring_size: RING_SIZE as u32,
            _reserved: 0,
        });
    }
    rt_base.write_u32(IR0_ERSTSZ, 1);
    rt_base.write_u64(IR0_ERDP, evt_ring_buf.phys());
    rt_base.write_u64(IR0_ERSTBA, dma.phys() + OFF_ERST as u64);

    // Enable interrupter 0
    rt_base.write_u32(IR0_IMOD, 0);
    rt_base.write_u32(IR0_IMAN, 3);

    // Start controller (R/S + INTE for interrupt delivery)
    op_base.write_u32(OP_USBCMD, 1 | (1 << 2));
    if !settles(|| CONTROLLER_ANSWERS && op_base.read_u32(OP_USBSTS) & 1 == 0) {
        refuse(format_args!("it stayed halted for {deadline_ms} ms after R/S"));
        return None;
    }
    log!("xHCI: controller started");

    // HCRST returns every root-hub port to the state it has with nothing
    // attached, and on a controller with Port Power Control that state is
    // unpowered — a port with no power reports no device, for the life of the
    // boot. PP is RW there and reads back set on a controller without PPC, so
    // the write is unconditional and the count is what says which happened.
    let mut powered = 0;
    for p in 0..max_ports {
        let off = OP_PORT_BASE + p as u64 * PORT_REG_SIZE;
        let portsc = op_base.read_u32(off);
        if portsc & PORTSC_PP == 0 {
            op_base.write_u32(off, Portsc::from_raw(portsc).neutral().powered().raw());
        }
        if op_base.read_u32(off) & PORTSC_PP != 0 {
            powered += 1;
        }
    }
    let powered_at = crate::clock::nanos_since_boot();
    log!("xHCI: {powered}/{max_ports} root-hub ports powered (PPC={})",
        u8::from(hccparams1 & HCC_PPC != 0));

    // A controller with no HID on it is still a controller, and keeping it is
    // not a formality: it has been reset, started and armed, so dropping it
    // leaves a live interrupter with nothing draining its event ring. It is
    // also the ordinary state of the target laptop, whose keyboard is PS/2 and
    // whose touchpad is I2C-HID — under metal-sim a `None` here reached
    // `kernel_main`'s `.expect` and panicked the boot.
    Some(XhciController {
        pci: *pci_dev,
        op_base,
        db_base,
        rt_base,
        max_ports,
        powered_at,
        context_size,
        layout,
        pool,
        protocols,
        cmd_ring,
        event_ring: evt_ring_buf.base() as *const Trb,
        event_head: 0,
        event_phase: true,
        devices: Vec::new(),
        msc: [MscBlock::FREE; MSC_BLOCKS],
        ports: (0..max_ports)
            .map(|p| {
                let mut port = PortState::EMPTY;
                port.speaks(protocols.of(p));
                port
            })
            .collect(),
        ports_dirty: false,
        outstanding: Outstanding::EMPTY,
        #[cfg(feature = "xhci-portsc-rw1c")]
        software_disabled: [0u64; 4],
    })
}
/// Initialize and configure one USB device on a port, waiting for each step.
///
/// **Which reset this port needs is [`port::reset_needed`]'s answer and never a
/// second opinion.** A machine that boots off a USB stick has that stick in the
/// port before the scheduler exists, so a SuperSpeed fix reaching only the
/// hot-plug path would not reach the machine it was written for.
///
/// The port records what came of it, here as on the hot-plug path: `finish` is
/// the one place a slot the controller enabled becomes the port's.
pub fn init_device(ctrl: &mut XhciController, port_idx: u8, protocol: Option<Protocol>) {
    let Some(kind) = port::reset_needed(protocol, ctrl.read_portsc(port_idx)) else {
        log!("xHCI: port {} link already trained, no reset needed", port_idx + 1);
        return configure(ctrl, port_idx);
    };
    reset_port(ctrl, port_idx, kind);

    // A port that asserts CCS and then never finishes — a device pulled between
    // the scan and the reset, a marginal cable, a reset the controller will not
    // run — costs that port and not the boot. This spin had no deadline, on the
    // boot CPU, before the scheduler exists and with nothing logged.
    if super::settles(|| reset_done(ctrl, port_idx)) {
        return configure(ctrl, port_idx);
    }

    // A hot reset a SuperSpeed link could not take leaves it Inactive, and
    // §4.19.1.2.4 has exactly one way out of that. Without this the port is
    // lost for the boot, which on the T14 is a USB-A socket that mounts
    // nothing, two boots out of two.
    if kind == Reset::Hot && protocol == Some(Protocol::Usb3) {
        log!("xHCI: port {} did not take a hot reset (link {:?}); warm resetting it",
            port_idx + 1, ctrl.read_portsc(port_idx).link_state());
        reset_port(ctrl, port_idx, Reset::Warm);
        if super::settles(|| reset_done(ctrl, port_idx)) {
            return configure(ctrl, port_idx);
        }
    }
    log!("xHCI: port {} never finished its reset (PORTSC {:#010x}); skipping it",
        port_idx + 1, ctrl.read_portsc(port_idx).raw());
    ctrl.port_bound(port_idx, None);
}

/// Everything between a port that has just finished its reset and a device the
/// driver can use, run to the end in place.
///
/// The boot scan's driver of [`Enumeration`], and the only difference between
/// it and the hot-plug one is where the waiting happens: here there is no
/// scheduler to give a pass back to, so the acts run one after another.
pub fn configure(ctrl: &mut XhciController, port_idx: u8) {
    begin(ctrl, port_idx);
    ctrl.settle_outstanding();
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
        // on hardware. The `port N enabled, speed=` line below is the valid one, and it says
        // enabled rather than reset because a SuperSpeed port reaches this
        // without one.
        if ctrl.read_portsc(p).connected() {
            log!("xHCI: port {} connected", p + 1);
            init_device(ctrl, p, ctrl.protocols.of(p));
        }
    }
    // Every change bit the scan left set, on every port. A change flag is what
    // *raises* a Port Status Change Event, and it raises one only as it goes
    // from 0 to 1 — so a CSC still set from the boot-time attach is a
    // disconnect the controller has no way to report, and the first thing
    // unplugged after boot would go unnoticed.
    ctrl.acknowledge_port_changes();
    // A device bound on an earlier port is armed and delivering while a later
    // one enumerates, so its completions arrive inside the drain a later port's
    // own acts run — and a broken one is recorded there rather than acted on.
    // Nothing else would come back for it: an endpoint holding no TRB raises no
    // further interrupt, so without this a device whose *first* transfer failed
    // during the boot scan would stay recorded and silent for the whole boot.
    ctrl.settle_outstanding();
    #[cfg(feature = "xhci-portsc-rw1c")]
    log!("xHCI: PED as RW1C, {} port(s) disabled by a driver write",
        ctrl.software_disabled_ports());
}

