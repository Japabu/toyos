//! Stage H0 of `specs/hda-driver-plan.md`: the boot that decides the track.
//!
//! Four questions, one boot, no driver. `00:1f.3` on the T14 has been present
//! and undriven on every boot this project has ever taken, so nothing here
//! knows what is behind it — and two of the four answers end or redirect the
//! whole audio plan. What the plan calls for is a *question*, so nothing below
//! configures a path, moves a sample, or allocates a descriptor.
//!
//! **Why this is a kernel feature and nothing else can reach it**
//! (`specs/device-test-strategy.md`'s requirement of an actuator): there is no
//! way for a userland process to touch a codec at all. `SYS_DEVICE_CLAIM` hands
//! out a claim on a device the kernel already bound, not a PCI function; the
//! capability that would let a process map a BAR and drive one is
//! `specs/userspace-drivers-spec.md` stage 4, and it is unbuilt. The questions
//! here are precisely the ones that
//! decide whether that capability will ever be given this device, so the thing
//! that answers them cannot be built on top of it.
//!
//! Every wait is bounded and every device-supplied number is checked before it
//! indexes anything. The machine this runs on has no serial port: a probe that
//! hangs leaves its owner a black screen and no information, which is the
//! outcome the exercise exists to avoid.
//!
//! **The output is a fixture.** `specs/hda-driver-plan.md` §5.2 makes the T14's
//! own widget graph the fixture `toyos-hda`'s output-path traversal is
//! host-tested against, so every line below that describes the codec is
//! `key=value` after a `hda:` prefix, carries the device's raw word beside any
//! decoded name, and never folds two nodes onto one line. A decoded name that
//! disagrees with the raw word beside it is a bug in this file and not in the
//! fixture.

use alloc::vec::Vec;

use toyos_hda::caps::{ConfigDefault, DefaultDevice, WidgetCaps, WidgetKind};
use toyos_hda::graph::{
    decode_connections, ConnectionListLen, FunctionKind, MAX_CONNECTIONS, MAX_FUNCTION_GROUPS,
    MAX_WIDGETS,
};
use toyos_hda::verb::{self, Address, NoSubordinates, Node, Response, Subordinates, Verb};

use crate::drivers::pci::PciDevice;
use crate::mm::paging::CachePolicy;
use crate::mm::Mmio;
use crate::log;

/// Class `0403`. Every function that answers it is probed, and `prog_if` is
/// not part of the selection: the T14's reports `0x80`, QEMU's reports `0x00`,
/// and a probe that matched on it would silently skip the machine it exists
/// for.
const CLASS_MULTIMEDIA: u8 = 0x04;
const SUBCLASS_HDA: u8 = 0x03;

const HEADER_COMMAND: u64 = 0x04;
const HEADER_BAR0: u64 = 0x10;
const COMMAND_MEMORY_SPACE: u16 = 1 << 1;

const CAP_POWER_MANAGEMENT: u8 = 0x01;

/// `PMCSR.PowerState`, and the recovery the PCI power-management specification
/// requires before the first register access after leaving D3hot.
const PM_CONTROL_STATUS: u64 = 0x04;
const PM_STATE_MASK: u16 = 0x3;
const PM_D3HOT_RECOVERY_NS: u64 = 10_000_000;

/// The controller's register window, from the Intel High Definition Audio
/// specification's register chapter. Only the reset, capability and immediate-
/// command registers are here: this probe never touches a stream descriptor, a
/// ring pointer or an interrupt enable, because none of them is needed to ask
/// what is on the link.
const GCAP: u64 = 0x00;
const VMIN: u64 = 0x02;
const VMAJ: u64 = 0x03;
const GCTL: u64 = 0x08;
const STATESTS: u64 = 0x0E;
const IMMEDIATE_COMMAND: u64 = 0x60;
const IMMEDIATE_RESPONSE: u64 = 0x64;
const IMMEDIATE_STATUS: u64 = 0x68;

const GCTL_CRST: u32 = 1 << 0;
const IMMEDIATE_BUSY: u16 = 1 << 0;
const IMMEDIATE_RESULT_VALID: u16 = 1 << 1;

/// The smallest window the controller's registers fit in. A BAR smaller than
/// this is a function that is not the controller this probe understands, and
/// is refused by name rather than mapped and read past.
const MIN_BAR_BYTES: u64 = 0x1000;

/// How long a register bit is given to settle.
///
/// Policy, not physics: it is the bound past which this probe stops waiting
/// for hardware that is not answering and says so. The specification's own
/// numbers are far below it — a controller reset is microseconds and an
/// immediate command completes in one codec frame — so an expiry here is a
/// device that has stopped, not one that is slow.
const SETTLE_NS: u64 = 100_000_000;

/// The delay the specification requires between releasing `CRST` and believing
/// `STATESTS`: 25 frames at 48 kHz. Rounded up to a millisecond, and read
/// twice a millisecond apart, because a codec that appears late reads as no
/// codec at all — and "no codec" is the answer that ends this plan.
const CODEC_DETECT_NS: u64 = 1_000_000;

pub fn run(rsdp_addr: u64, devices: &[PciDevice]) {
    log!("hda: === H0 probe: specs/hda-driver-plan.md §6 ===");

    // Every function of the class, never the first: the display audio on an
    // Iris Xe is a codec on the same controller, but a machine with two
    // *controllers* is the shape `pci.rs` already records this kernel getting
    // wrong once.
    let controllers: Vec<&PciDevice> = devices
        .iter()
        .filter(|d| d.matches_class(CLASS_MULTIMEDIA, SUBCLASS_HDA, None))
        .collect();

    if controllers.is_empty() {
        log!("hda: no class 0403 function on this machine — nothing to probe");
        log!("hda: === H0 probe done ===");
        return;
    }

    for controller in &controllers {
        log!(
            "hda: controller {:02x}:{:02x}.{} {:04x}:{:04x}",
            controller.bus,
            controller.dev,
            controller.func,
            controller.vendor_id(),
            controller.device_id()
        );
        // Sized once and handed to both halves: sizing takes the function's
        // memory decode down, and doing that twice around a controller that is
        // about to be taken out of reset is two windows where it is dark for
        // no reason.
        let bar = size_bar0(controller);
        handoff(rsdp_addr, devices, controller, bar.as_ref());
        match &bar {
            Some(bar) => codecs(controller, bar),
            None => log!("hda: (b) bar0 is not a memory BAR — the controller cannot be reached"),
        }
    }

    if controllers.len() > 1 {
        log!(
            "hda: {} class 0403 functions on this machine — every one is probed above",
            controllers.len()
        );
    }
    log!("hda: === H0 probe done ===");
}

// --- (a) Is this function handoff-able to userspace at all? ---

/// `specs/iommu-spec.md` §7.3 and §7.4, `specs/userspace-drivers-spec.md` §4.3
/// and §4.5, asked of one function.
///
/// Not one of these is audio's alone. The T14's HDA is function 3 of a
/// five-function device whose function 6 is the I219-V, so an isolation scope
/// that refuses this device refuses gate N's metal target with it.
fn handoff(rsdp_addr: u64, devices: &[PciDevice], hda: &PciDevice, bar: Option<&Bar>) {
    scope(devices, hda);
    reserved_regions(rsdp_addr, hda);
    interrupts(hda);
    match bar {
        Some(bar) => bar0(devices, hda, bar),
        None => log!("hda: (a) bar0 is not a memory BAR — this function decodes nothing to map"),
    }
}

/// Who else is inside this device's isolation scope.
///
/// Two of §7.3's three inputs are answerable from the bus alone and the third
/// is not: the sibling functions are `pci::enumerate`'s own result, the bridge
/// above is the topology it walked, and whether a real root complex enforces
/// what its ACS capability advertises is a hardware property no boot can
/// verify from inside. So this reports what the machine *presents* and leaves
/// the rule to §7.3.
fn scope(devices: &[PciDevice], hda: &PciDevice) {
    let mut siblings = 0usize;
    for other in devices {
        if other.bus == hda.bus && other.dev == hda.dev && other.func != hda.func {
            siblings += 1;
            log!(
                "hda: scope sibling {:02x}:{:02x}.{} {:04x}:{:04x} class={:02x}{:02x}",
                other.bus,
                other.dev,
                other.func,
                other.vendor_id(),
                other.device_id(),
                other.class(),
                other.subclass()
            );
        }
    }

    match upstream_bridge(devices, hda) {
        Some(bridge) => log!(
            "hda: scope upstream-bridge {:02x}:{:02x}.{} {:04x}:{:04x} acs={}",
            bridge.bus,
            bridge.dev,
            bridge.func,
            bridge.vendor_id(),
            bridge.device_id(),
            match bridge.extended_capability(EXT_CAP_ACS) {
                Some(cap) => acs_control(cap),
                None => AcsControl::Absent,
            }
        ),
        // Not "no bridge found": a function on bus 0 of a machine whose host
        // bridge is also on bus 0 is root-complex-integrated, and §7.3's
        // peer-to-peer-behind-a-switch case cannot arise for it. Which is a
        // *weaker* statement than isolation, because what two functions of one
        // root-complex-integrated device can do to each other is a property of
        // the root complex that no capability register on the bus reports.
        None => log!(
            "hda: scope upstream-bridge none — {:02x}:{:02x}.{} is root-complex-integrated",
            hda.bus, hda.dev, hda.func
        ),
    }

    log!(
        "hda: (a) scope members={} — {}",
        siblings + 1,
        if siblings == 0 {
            "a singleton; iommu-spec §7.3 permits handoff on this count"
        } else {
            "not a singleton; iommu-spec §7.3 refuses handoff, and refuses every sibling with it"
        }
    );
}

/// PCIe extended capability id for Access Control Services.
const EXT_CAP_ACS: u16 = 0x000D;
/// `ACS Control`, two bytes past the capability header's four and the two of
/// `ACS Capability`.
const ACS_CONTROL: u64 = 0x06;
/// Source Validation, Translation Blocking, P2P Request Redirect, P2P
/// Completion Redirect, Upstream Forwarding. The five whose being *set*
/// together is what makes a downstream port route peer traffic up to the unit
/// instead of across.
const ACS_REDIRECT_ALL: u16 = 0x1F;

enum AcsControl {
    Absent,
    Enabled,
    /// Present, and not routing everything upstream. The bits are printed
    /// because "partial" is not a state anyone can act on without them.
    Partial(u16),
}

impl core::fmt::Display for AcsControl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Absent => write!(f, "absent"),
            Self::Enabled => write!(f, "on"),
            Self::Partial(ctl) => write!(f, "partial({ctl:#06x})"),
        }
    }
}

fn acs_control(cap: crate::drivers::pci::ExtendedCapability<'_>) -> AcsControl {
    let ctl = cap.read_u16(ACS_CONTROL);
    if ctl & ACS_REDIRECT_ALL == ACS_REDIRECT_ALL {
        AcsControl::Enabled
    } else {
        AcsControl::Partial(ctl)
    }
}

const CLASS_BRIDGE: u8 = 0x06;
const SUBCLASS_PCI_BRIDGE: u8 = 0x04;

/// The bridge whose secondary bus range contains `hda`'s bus.
///
/// `None` for a function on the same bus as the host bridge, which is what a
/// root-complex-integrated endpoint is.
fn upstream_bridge<'a>(devices: &'a [PciDevice], hda: &PciDevice) -> Option<&'a PciDevice> {
    devices.iter().find(|b| {
        b.matches_class(CLASS_BRIDGE, SUBCLASS_PCI_BRIDGE, None)
            && (b.secondary_bus()..=b.subordinate_bus()).contains(&hda.bus)
    })
}

/// §7.4: a device carrying a reserved region is refused for userspace handoff,
/// because identity-mapping firmware's range into an untrusted driver's domain
/// hands that driver a window into memory it was never given.
///
/// QEMU publishes no RMRR at all, so this is one of the two halves of H0 whose
/// interesting answer only a real machine can give.
fn reserved_regions(rsdp_addr: u64, hda: &PciDevice) {
    let facts = crate::iommu::describe_device(rsdp_addr, hda.bus, hda.dev, hda.func);

    match facts.unit {
        Some(unit) => log!(
            "hda: (a) unit={} scoped-by={} ecap.sc={}",
            unit.index,
            if unit.explicit { "device-scope" } else { "include-pci-all" },
            yn(unit.snoop_control)
        ),
        // Not the same as "there is no IOMMU": the boot's own `iommu:` lines
        // say which it was, and this line is about the device.
        None => log!(
            "hda: (a) unit=none — no remapping unit on this machine claims {:02x}:{:02x}.{}",
            hda.bus, hda.dev, hda.func
        ),
    }

    if facts.reserved.is_empty() {
        log!("hda: (a) rmrr none — no reserved region names this device");
    }
    for region in &facts.reserved {
        log!(
            "hda: (a) rmrr {:#018x}..{:#018x} — iommu-spec §7.4 refuses handoff",
            region.base,
            region.limit
        );
    }
}

/// `specs/userspace-drivers-spec.md` §4.5: a function offering neither MSI-X
/// nor MSI is ineligible for userspace, because there is no way to deliver its
/// interrupt to a driver process. §4.2 of `specs/hda-driver-plan.md` argues
/// plain MSI is the *better* of the two here — an MSI-X table lives inside a
/// BAR and cannot be carved out at 2 MiB granularity, and a capability in
/// config space simply has no such hole.
fn interrupts(hda: &PciDevice) {
    // An MSI-X this kernel would decline to arm is one a handoff cannot rest
    // on either, so it is named and then treated as absent — leaving the MSI
    // answer, which is the one §4.2 wants anyway.
    let msix = hda.msix().and_then(|decoded| match decoded {
        Ok(msix) => Some(msix),
        Err(why) => {
            log!("hda: (a) msix unusable — {why}");
            None
        }
    });

    match (hda.msi(), msix) {
        (_, Some(msix)) => log!(
            "hda: (a) msix vectors={} — eligible, and its table is inside BAR {} \
             (userspace-drivers-spec §4.3)",
            msix.entries(),
            msix.bir()
        ),
        (Some(msi), None) => log!(
            "hda: (a) msi vectors={} addr64={} msix=none — eligible, capability in config \
             space and no table in a BAR",
            msi.vectors(),
            yn(msi.wide())
        ),
        (None, None) => log!(
            "hda: (a) msi=none msix=none — userspace-drivers-spec §4.5 makes this function \
             ineligible for handoff"
        ),
    }
}

/// BAR0's size, width, prefetchability, whether its address bits are writable,
/// and who else is inside the 2 MiB page it currently sits in.
///
/// The last is the sharpest of the four. `specs/userspace-drivers-spec.md` §4.3
/// maps a BAR into a driver process at 2 MiB granularity, so a BAR sharing its
/// page with another function's registers is a BAR that cannot be handed over
/// without handing that function's registers over too.
///
/// **The neighbours are found by base and never by sizing them.** Sizing a BAR
/// means taking its function's memory decode down and leaving `0xFFFFFFFF` in
/// the register across two config writes, and every other function on this
/// machine includes the NVMe controller and the xHC this kernel is running
/// from. Base alone is enough and is not an approximation: firmware does not
/// overlap BARs, so a window whose base is outside our 2 MiB page cannot reach
/// into it.
fn bar0(devices: &[PciDevice], hda: &PciDevice, bar: &Bar) {
    let page = crate::mm::PAGE_2M;
    let mut neighbours = 0usize;
    for other in devices {
        if other.bus == hda.bus && other.dev == hda.dev && other.func == hda.func {
            continue;
        }
        let mut index = 0u8;
        while index < 6 {
            let raw = other.read_config_u32(HEADER_BAR0 + index as u64 * 4);
            // An I/O BAR decodes a different space entirely and never shares a
            // page with anything.
            if raw & 1 != 0 {
                index += 1;
                continue;
            }
            let wide = (raw >> 1) & 0x3 == 2;
            let base = if wide {
                let high = other.read_config_u32(HEADER_BAR0 + (index as u64 + 1) * 4);
                ((high as u64) << 32) | (raw as u64 & !0xF)
            } else {
                raw as u64 & !0xF
            };
            // The upper half of a 64-bit BAR is not a BAR, and reading it as
            // one would name an address no function decodes.
            index += if wide { 2 } else { 1 };
            if base == 0 || base / page != bar.base / page {
                continue;
            }
            neighbours += 1;
            log!(
                "hda: bar0 shares its 2 MiB page with {:02x}:{:02x}.{} bar{} at {base:#x}",
                other.bus,
                other.dev,
                other.func,
                index - if wide { 2 } else { 1 },
            );
        }
    }

    log!(
        "hda: (a) bar0 base={:#x} size={:#x} 64bit={} prefetch={} movable={} \
         2m-page-neighbours={neighbours}",
        bar.base,
        bar.size,
        yn(bar.wide),
        yn(bar.prefetchable),
        yn(bar.movable),
    );
}

struct Bar {
    base: u64,
    size: u64,
    wide: bool,
    prefetchable: bool,
    /// The address bits came back writable, which is what a BAR firmware
    /// assigned looks like and what a hardwired window does not.
    movable: bool,
}

/// BAR0, sized by the mechanism the PCI specification defines: all ones written
/// back, the low bits the function leaves clear naming its span.
///
/// Memory decode is off across the write, because a BAR briefly holding
/// `0xFFFFFFFF` is a BAR briefly claiming an address range that belongs to
/// something else. **Only ever called on a class-0403 function**, which no
/// driver in this kernel binds, so nothing can be mid-transaction while it is
/// dark.
fn size_bar0(pci: &PciDevice) -> Option<Bar> {
    let low = pci.read_config_u32(HEADER_BAR0);
    if low & 1 != 0 {
        return None;
    }
    let wide = (low >> 1) & 0x3 == 2;
    let high = if wide { pci.read_config_u32(HEADER_BAR0 + 4) } else { 0 };
    let base = ((high as u64) << 32) | (low as u64 & !0xF);

    let command = pci.read_config_u16(HEADER_COMMAND);
    pci.write_config_u16(HEADER_COMMAND, command & !COMMAND_MEMORY_SPACE);
    pci.write_config_u32(HEADER_BAR0, u32::MAX);
    let probed_low = pci.read_config_u32(HEADER_BAR0);
    let probed_high = if wide {
        pci.write_config_u32(HEADER_BAR0 + 4, u32::MAX);
        let probed = pci.read_config_u32(HEADER_BAR0 + 4);
        pci.write_config_u32(HEADER_BAR0 + 4, high);
        probed
    } else {
        u32::MAX
    };
    pci.write_config_u32(HEADER_BAR0, low);
    pci.write_config_u16(HEADER_COMMAND, command);

    let mask = (((probed_high as u64) << 32) | (probed_low as u64 & !0xF)) & !0xF;
    Some(Bar {
        base,
        size: if mask == 0 { 0 } else { (!mask).wrapping_add(1) },
        wide,
        prefetchable: low & (1 << 3) != 0,
        movable: mask != 0,
    })
}

// --- (b), (c), (d): what is on the link ---

/// Take the controller out of reset, read `STATESTS`, and dump every codec
/// that answers.
///
/// This is the cheapest question in the plan and the one most likely to end
/// it. Tiger Lake designs ship in which the codec hangs off SoundWire and the
/// legacy HDA link enumerates nothing at all — that machine reads zero here,
/// and no amount of driver work changes it.
fn codecs(hda: &PciDevice, bar: &Bar) {
    if bar.base == 0 {
        log!("hda: (b) bar0 is unassigned — firmware gave this function no register window");
        return;
    }
    if bar.size < MIN_BAR_BYTES {
        log!(
            "hda: (b) bar0 is {:#x} bytes, too small to be an HDA register window — refused",
            bar.size
        );
        return;
    }

    power_up(hda);

    // Memory decode, because firmware may have left this function dark, and
    // nothing else in this kernel has ever turned it on. Bus mastering stays
    // *off*: everything below is one write and one read of a register, and a
    // probe that enabled DMA it does not use would be a probe that could
    // corrupt memory if it were wrong.
    let command = hda.read_config_u16(HEADER_COMMAND);
    hda.write_config_u16(HEADER_COMMAND, command | COMMAND_MEMORY_SPACE);

    let regs = crate::mm::paging::map_mmio(
        bar.base,
        bar.size.min(MIN_BAR_BYTES * 4),
        CachePolicy::DeferToMtrr,
    );

    let gcap = regs.read_u16(GCAP);
    if gcap == u16::MAX {
        log!(
            "hda: (b) GCAP reads all ones — the register window at {:#x} answers nothing",
            bar.base
        );
        return;
    }
    log!(
        "hda: gcap={gcap:#06x} vmaj={} vmin={} oss={} iss={} bss={} nsdo={} addr64={}",
        regs.read_u8(VMAJ),
        regs.read_u8(VMIN),
        (gcap >> 12) & 0xF,
        (gcap >> 8) & 0xF,
        (gcap >> 3) & 0x1F,
        (gcap >> 1) & 0x3,
        yn(gcap & 1 != 0)
    );

    if !reset(regs) {
        return;
    }

    let first = regs.read_u16(STATESTS);
    spin_until_ns(CODEC_DETECT_NS);
    let statests = regs.read_u16(STATESTS);
    log!("hda: statests={statests:#06x} (first read {first:#06x})");

    if statests == 0 {
        log!(
            "hda: (b) statests=0x0000 — NO CODEC ON THE LEGACY LINK. On silicon of this \
             generation that means the audio path is behind the vendor DSP (Intel Smart Sound) \
             or on SoundWire, and hda-driver-plan.md is dead on this machine."
        );
        log!("hda: (c) no widget graph: no codec answered");
        log!("hda: (d) codecs=0");
        return;
    }

    let mut found = 0usize;
    let mut speakers = 0usize;
    for address in verb::present(statests) {
        found += 1;
        speakers += dump_codec(regs, address);
    }

    log!(
        "hda: (b) statests={statests:#06x} — a codec answers on the legacy link; the plan is \
         alive on this machine"
    );
    log!(
        "hda: (d) codecs={found} — every one is dumped above. {}",
        if found > 1 {
            "More than one: a driver taking the first match can bind display audio and produce \
             silence from the speakers (hda-driver-plan.md §2.3)."
        } else {
            "One, so §2.3's first-match trap cannot arise on this machine — which says nothing \
             about the next one."
        }
    );
    log!(
        "hda: (c) pins reporting a speaker default device: {speakers}. {}",
        if speakers == 0 {
            "None — §2.3's traversal has no speaker pin to choose and would refuse this machine."
        } else {
            "The traversal has a candidate."
        }
    );
}

/// Put the function in D0 if firmware left it lower.
///
/// A function in D3hot answers every register read with all ones, which is
/// indistinguishable from a controller that is not there — and firmware
/// leaving an undriven audio controller powered down is the ordinary case, not
/// the exotic one.
fn power_up(pci: &PciDevice) {
    let Some(cap) = pci.capabilities().find(|c| c.id() == CAP_POWER_MANAGEMENT) else {
        log!("hda: no power-management capability — the function has no state to leave");
        return;
    };
    let pmcsr = cap.read_u16(PM_CONTROL_STATUS);
    let state = pmcsr & PM_STATE_MASK;
    if state == 0 {
        log!("hda: power state D0");
        return;
    }
    cap.write_u16(PM_CONTROL_STATUS, pmcsr & !PM_STATE_MASK);
    spin_until_ns(PM_D3HOT_RECOVERY_NS);
    log!("hda: power state D{state} -> D{}", cap.read_u16(PM_CONTROL_STATUS) & PM_STATE_MASK);
}

/// `GCTL.CRST`: hold the controller in reset, then release it.
///
/// Both edges are waited for, because the specification makes the bit read
/// back only once the controller has acted on it — so a write that is not read
/// back is a controller that is not there, and reading `STATESTS` off one
/// would report a codec that does not exist.
fn reset(regs: Mmio) -> bool {
    regs.write_u32(GCTL, 0);
    if !settles(|| regs.read_u32(GCTL) & GCTL_CRST == 0) {
        log!("hda: controller never entered reset (GCTL={:#010x}) — refused", regs.read_u32(GCTL));
        return false;
    }
    regs.write_u32(GCTL, GCTL_CRST);
    if !settles(|| regs.read_u32(GCTL) & GCTL_CRST != 0) {
        log!("hda: controller never left reset (GCTL={:#010x}) — refused", regs.read_u32(GCTL));
        return false;
    }
    spin_until_ns(CODEC_DETECT_NS);
    true
}

/// What one immediate command came back as.
///
/// The controller not answering is a different fact from a codec answering all
/// ones, and H0 is the boot that has to tell them apart: a controller with no
/// immediate-command implementation and a codec that has stopped look identical
/// to anything that folds them together.
#[derive(Clone, Copy)]
enum Answer {
    Codec(Response),
    /// All ones — a link with nothing on it, and a codec that has stopped
    /// answering, both read as this, and neither is a value to decode.
    AllOnes,
    /// The controller never completed the command.
    Silent,
}

impl Answer {
    /// The word a dump line carries: what the codec said, or the all ones the
    /// link reads as when nothing did. Re-encoded here because a fixture line
    /// records what the wire carried; nothing in this file decides on it.
    fn word(self) -> u32 {
        match self {
            Self::Codec(response) => response.raw(),
            Self::AllOnes | Self::Silent => u32::MAX,
        }
    }

    fn codec(self) -> Option<Response> {
        match self {
            Self::Codec(response) => Some(response),
            Self::AllOnes | Self::Silent => None,
        }
    }
}

/// One verb over the immediate-command registers: one write, one poll, one
/// read, and no DMA anywhere.
fn get(regs: Mmio, codec: Address, node: Node, verb: u16, payload: u8) -> Answer {
    let command = Verb::short(codec, node, verb, payload);

    if !settles(|| regs.read_u16(IMMEDIATE_STATUS) & IMMEDIATE_BUSY == 0) {
        return Answer::Silent;
    }
    regs.write_u16(IMMEDIATE_STATUS, IMMEDIATE_RESULT_VALID);
    regs.write_u32(IMMEDIATE_COMMAND, command.raw());
    regs.write_u16(IMMEDIATE_STATUS, IMMEDIATE_BUSY);

    let done = settles(|| {
        let status = regs.read_u16(IMMEDIATE_STATUS);
        status & IMMEDIATE_BUSY == 0 && status & IMMEDIATE_RESULT_VALID != 0
    });
    if !done {
        return Answer::Silent;
    }
    let response = regs.read_u32(IMMEDIATE_RESPONSE);
    regs.write_u16(IMMEDIATE_STATUS, IMMEDIATE_RESULT_VALID);
    Response::new(response).map_or(Answer::AllOnes, Answer::Codec)
}

fn param(regs: Mmio, codec: Address, node: Node, which: u8) -> Answer {
    get(regs, codec, node, verb::GET_PARAMETER, which)
}

/// The subordinate node range a node declares, clamped to what this probe walks.
fn subordinates(
    regs: Mmio,
    codec: Address,
    node: Node,
    limit: usize,
    what: &str,
) -> Option<Subordinates> {
    let response = match param(regs, codec, node, verb::PARAM_SUB_NODE_COUNT) {
        Answer::Codec(response) => response,
        Answer::Silent => return None,
        Answer::AllOnes => {
            log!(
                "hda: codec{codec} node={node:#04x} answered all ones to its {what} count — \
                 stopping"
            );
            return None;
        }
    };
    let range = match Subordinates::decode(response) {
        Ok(range) => range,
        Err(NoSubordinates::Leaf) => return None,
        Err(NoSubordinates::PastNodeSpace { first, count }) => {
            log!(
                "hda: codec{codec} node={node:#04x} declares {count} {what}s from {first:#04x}, \
                 past the node id space — refused"
            );
            return None;
        }
    };
    let walked = (range.count as usize).min(limit);
    if walked < range.count as usize {
        log!(
            "hda: codec{codec} node={node:#04x} declares {} {what}s, more than this probe walks \
             ({limit}) — the rest are not dumped",
            range.count
        );
    }
    Some(Subordinates { first: range.first, count: walked as u8 })
}

/// Returns how many pins on this codec name a speaker as their default device.
fn dump_codec(regs: Mmio, codec: Address) -> usize {
    let vendor = match param(regs, codec, Node::ROOT, verb::PARAM_VENDOR_ID) {
        Answer::Codec(vendor) => vendor.raw(),
        Answer::Silent => {
            log!(
                "hda: codec{codec} the controller did not answer an immediate command — no verb \
                 interface on this controller, so nothing below could be read"
            );
            return 0;
        }
        Answer::AllOnes => {
            log!(
                "hda: codec{codec} answered all ones to its vendor id — STATESTS claims a codec \
                 the link does not carry"
            );
            return 0;
        }
    };
    log!(
        "hda: codec{codec} vendor={:04x} device={:04x} revision={:#010x}",
        vendor >> 16,
        vendor & 0xFFFF,
        param(regs, codec, Node::ROOT, verb::PARAM_REVISION_ID).word()
    );

    let Some(groups) = subordinates(regs, codec, Node::ROOT, MAX_FUNCTION_GROUPS, "function group")
    else {
        log!("hda: codec{codec} declares no function group");
        return 0;
    };

    let mut speakers = 0usize;
    for group in groups.nodes() {
        speakers += dump_function_group(regs, codec, group);
    }
    speakers
}

fn dump_function_group(regs: Mmio, codec: Address, group: Node) -> usize {
    let kind = param(regs, codec, group, verb::PARAM_FUNCTION_TYPE)
        .codec()
        .map_or(FunctionKind::Other(u8::MAX), FunctionKind::decode);
    log!(
        "hda: codec{codec} fg={group:#04x} type={:#04x} ({}) caps={:#010x} power={:#010x} \
         gpio={:#010x}",
        kind.code(),
        kind.name(),
        param(regs, codec, group, verb::PARAM_FUNCTION_CAPS).word(),
        param(regs, codec, group, verb::PARAM_POWER_STATES).word(),
        param(regs, codec, group, verb::PARAM_GPIO_COUNT).word(),
    );
    // A modem function group is walked no further, and said so rather than
    // dropped: §2.3 step 1 keeps the audio groups and logs the rest.
    if kind != FunctionKind::Audio {
        log!("hda: codec{codec} fg={group:#04x} is not an audio function group — not walked");
        return 0;
    }

    let Some(widgets) = subordinates(regs, codec, group, MAX_WIDGETS, "widget") else {
        log!("hda: codec{codec} fg={group:#04x} declares no widget");
        return 0;
    };
    log!(
        "hda: codec{codec} fg={group:#04x} widgets={:#04x}..{:#04x}",
        widgets.first,
        widgets.last()
    );

    let mut speakers = 0usize;
    for node in widgets.nodes() {
        speakers += dump_widget(regs, codec, node);
    }
    speakers
}

/// One widget, in as many lines as it has facts. Returns 1 if this is a pin
/// complex whose configuration default names a speaker.
fn dump_widget(regs: Mmio, codec: Address, node: Node) -> usize {
    let Some(response) = param(regs, codec, node, verb::PARAM_WIDGET_CAPS).codec() else {
        log!("hda: codec{codec} node={node:#04x} answered all ones to its capabilities — skipped");
        return 0;
    };
    let caps = WidgetCaps::decode(response);
    log!(
        "hda: codec{codec} node={node:#04x} type={:#x} ({}) caps={:#010x} channels={} \
         amp-in={} amp-out={} power={} digital={} conn-list={}",
        caps.kind.code(),
        caps.kind.name(),
        response.raw(),
        caps.channels,
        yn(caps.input_amp),
        yn(caps.output_amp),
        yn(caps.power_control),
        yn(caps.digital),
        yn(caps.connection_list),
    );

    if caps.input_amp {
        log!(
            "hda: codec{codec} node={node:#04x} amp-in-caps={:#010x}",
            param(regs, codec, node, verb::PARAM_AMP_IN_CAPS).word()
        );
    }
    if caps.output_amp {
        log!(
            "hda: codec{codec} node={node:#04x} amp-out-caps={:#010x}",
            param(regs, codec, node, verb::PARAM_AMP_OUT_CAPS).word()
        );
    }
    if matches!(caps.kind, WidgetKind::AudioOutput | WidgetKind::AudioInput) {
        log!(
            "hda: codec{codec} node={node:#04x} pcm={:#010x} formats={:#010x}",
            param(regs, codec, node, verb::PARAM_PCM).word(),
            param(regs, codec, node, verb::PARAM_STREAM_FORMATS).word()
        );
    }
    if caps.power_control {
        log!(
            "hda: codec{codec} node={node:#04x} power-states={:#010x} power-state={:#010x}",
            param(regs, codec, node, verb::PARAM_POWER_STATES).word(),
            get(regs, codec, node, verb::GET_POWER_STATE, 0).word()
        );
    }
    if caps.kind == WidgetKind::VolumeKnob {
        log!(
            "hda: codec{codec} node={node:#04x} volume-knob-caps={:#010x}",
            param(regs, codec, node, verb::PARAM_VOLUME_KNOB_CAPS).word()
        );
    }
    if caps.proc_widget {
        log!(
            "hda: codec{codec} node={node:#04x} processing-caps={:#010x}",
            param(regs, codec, node, verb::PARAM_PROCESSING_CAPS).word()
        );
    }

    connections(regs, codec, node);

    if caps.kind == WidgetKind::PinComplex {
        return pin(regs, codec, node);
    }
    0
}

/// The widget's connection list, decoded to node ids.
///
/// §2.3 step 4 walks this list backwards to find a converter, so it is the part
/// of the dump the traversal is tested against and the part a codec is most
/// able to lie about. The decode is `toyos-hda`'s, which refuses a list whose
/// range entries run backwards or off the end of the bound rather than guessing
/// at one: this dump is what H1's fixtures are made of, and a list the driver
/// would refuse must not reach one looking like a list it accepted. The words
/// the codec sent are on the refusal line, so nothing it said is lost.
fn connections(regs: Mmio, codec: Address, node: Node) {
    let Some(length) = param(regs, codec, node, verb::PARAM_CONNECTION_LENGTH).codec() else {
        return;
    };
    let declared = ConnectionListLen::decode(length);
    if declared.count == 0 {
        return;
    }
    let len = ConnectionListLen {
        count: (declared.count as usize).min(MAX_CONNECTIONS) as u8,
        long: declared.long,
    };
    if len.count < declared.count {
        log!(
            "hda: codec{codec} node={node:#04x} declares {} connections, more than this probe \
             walks ({MAX_CONNECTIONS}) — the rest are not dumped",
            declared.count
        );
    }

    let mut responses: Vec<Response> = Vec::with_capacity(len.responses());
    for index in 0..len.responses() {
        let entry = (index * len.per_response()) as u8;
        let Some(response) = get(regs, codec, node, verb::GET_CONNECTION_LIST, entry).codec() else {
            return;
        };
        responses.push(response);
    }

    match decode_connections(len, &responses) {
        Some(list) => {
            let list: Vec<u8> = list.iter().map(|named| named.0).collect();
            log!(
                "hda: codec{codec} node={node:#04x} conn-len={:#010x} long={} count={} list={:?}",
                length.raw(),
                yn(len.long),
                list.len(),
                list
            );
        }
        None => {
            let words: Vec<u32> = responses.iter().map(|response| response.raw()).collect();
            log!(
                "hda: codec{codec} node={node:#04x} conn-len={:#010x} long={} entries={} \
                 responses={:08x?} — the entries do not name a node list and are not dumped as one",
                length.raw(),
                yn(len.long),
                len.count,
                words
            );
        }
    }
}

fn pin(regs: Mmio, codec: Address, node: Node) -> usize {
    let config = get(regs, codec, node, verb::GET_CONFIG_DEFAULT, 0);
    log!(
        "hda: codec{codec} node={node:#04x} pin-caps={:#010x} pin-ctl={:#010x} eapd={:#010x}",
        param(regs, codec, node, verb::PARAM_PIN_CAPS).word(),
        get(regs, codec, node, verb::GET_PIN_CONTROL, 0).word(),
        get(regs, codec, node, verb::GET_EAPD, 0).word(),
    );
    // All ones decodes to a perfectly plausible pin — "jack+fixed", device
    // "other" — which is a name for a read that did not happen. §2.3 chooses a
    // pin off exactly these fields, so a fixture carrying one of these would
    // put a speaker in a graph the codec never described.
    let Some(config) = config.codec() else {
        log!(
            "hda: codec{codec} node={node:#04x} cfgdef=0xffffffff — no answer, not decoded and \
             not a candidate"
        );
        return 0;
    };
    let default = ConfigDefault::decode(config);
    log!(
        "hda: codec{codec} node={node:#04x} cfgdef={:#010x} conn={} device={} location={:#04x} \
         type={:#x} colour={:#x} assoc={} sequence={}",
        config.raw(),
        default.connectivity.name(),
        default.device.name(),
        default.location,
        default.connection_type,
        default.colour,
        default.association,
        default.sequence,
    );
    // §2.3 step 3 discards a pin whose port connectivity says no physical
    // connection, whatever its default device claims — a header nobody
    // soldered still names a speaker.
    usize::from(default.device == DefaultDevice::Speaker && default.is_physical())
}

// --- bounded waiting ---

/// Poll `ready` until it holds or [`SETTLE_NS`] passes.
fn settles(ready: impl Fn() -> bool) -> bool {
    let deadline = crate::clock::nanos_since_boot() + SETTLE_NS;
    while !ready() {
        if crate::clock::nanos_since_boot() >= deadline {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn spin_until_ns(duration: u64) {
    let deadline = crate::clock::nanos_since_boot() + duration;
    while crate::clock::nanos_since_boot() < deadline {
        core::hint::spin_loop();
    }
}

fn yn(v: bool) -> char {
    if v {
        'y'
    } else {
        'n'
    }
}
