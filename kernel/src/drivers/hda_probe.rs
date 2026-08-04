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
//! way for a userland process to touch a codec at all. `SYS_OPEN_DEVICE` hands
//! out a `DEVICE_AUDIO` claim, not a PCI function; the capability that would
//! let a process map a BAR and drive one is `specs/userspace-drivers-spec.md`
//! stage 4, and it is unbuilt. The questions here are precisely the ones that
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
const CAP_MSI: u8 = 0x05;
const CAP_MSIX: u8 = 0x11;

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

/// `STATESTS` has one bit per SDI link, and the specification gives it fifteen.
const MAX_CODECS: u8 = 15;

/// Bounds on what a codec says about itself. All four are policy: a codec's own
/// numbers are untrusted input, a walk over them needs a ceiling that is not
/// the codec's, and what a device past one loses is the part past it — which it
/// is told, rather than left to infer from a short dump.
const MAX_FUNCTION_GROUPS: u8 = 8;
const MAX_WIDGETS: u8 = 128;
const MAX_CONNECTIONS: u8 = 64;

/// A response of all ones is what a link with nothing on it, and a codec that
/// has stopped answering, both read as. Neither is a value to decode.
const NO_RESPONSE: u32 = u32::MAX;

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
        handoff(rsdp_addr, devices, controller);
        codecs(controller);
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
fn handoff(rsdp_addr: u64, devices: &[PciDevice], hda: &PciDevice) {
    scope(devices, hda);
    reserved_regions(rsdp_addr, hda);
    interrupts(hda);
    bar0(devices, hda);
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
    let msi = hda.capabilities().find(|c| c.id() == CAP_MSI).map(|cap| {
        let ctrl = cap.read_u16(2);
        // `Multiple Message Capable`, an exponent: three bits saying how many
        // consecutive vectors the function can raise.
        (1u16 << ((ctrl >> 1) & 0x7), ctrl & (1 << 7) != 0)
    });
    let msix = hda.capabilities().find(|c| c.id() == CAP_MSIX).map(|cap| {
        // `Table Size`, encoded one less than it is.
        (cap.read_u16(2) & 0x7FF) + 1
    });

    match (msi, msix) {
        (_, Some(vectors)) => log!(
            "hda: (a) msix vectors={vectors} — eligible, and its table is inside a BAR \
             (userspace-drivers-spec §4.3)"
        ),
        (Some((vectors, wide)), None) => log!(
            "hda: (a) msi vectors={vectors} addr64={} msix=none — eligible, capability in config \
             space and no table in a BAR",
            yn(wide)
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
/// Sizing writes all ones and puts the original back, with memory decode off
/// across the pair: a BAR briefly holding `0xFFFFFFFF` is a BAR briefly
/// claiming an address range that belongs to something else. Nothing in this
/// kernel drives this function, so nothing can be mid-transaction while it is
/// dark.
fn bar0(devices: &[PciDevice], hda: &PciDevice) {
    let Some(bar) = size_bar0(hda) else {
        log!("hda: (a) bar0 is not a memory BAR — this function decodes nothing to map");
        return;
    };

    let page = crate::mm::PAGE_2M;
    let mut neighbours = 0usize;
    for other in devices {
        if other.bus == hda.bus && other.dev == hda.dev && other.func == hda.func {
            continue;
        }
        for index in 0..6u8 {
            let Some(theirs) = size_bar(other, index) else { continue };
            if theirs.base == 0 || theirs.size == 0 {
                continue;
            }
            // Their BAR's own extent against the 2 MiB page ours sits in.
            if theirs.base / page == bar.base / page
                || (theirs.base + theirs.size - 1) / page == bar.base / page
            {
                neighbours += 1;
                log!(
                    "hda: bar0 shares its 2 MiB page with {:02x}:{:02x}.{} bar{index} \
                     {:#x}+{:#x}",
                    other.bus,
                    other.dev,
                    other.func,
                    theirs.base,
                    theirs.size
                );
            }
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

fn size_bar0(pci: &PciDevice) -> Option<Bar> {
    size_bar(pci, 0)
}

/// One BAR, by the mechanism the PCI specification defines: all ones written
/// back, the low bits the function leaves clear naming its span.
fn size_bar(pci: &PciDevice, index: u8) -> Option<Bar> {
    let offset = HEADER_BAR0 + index as u64 * 4;
    let low = pci.read_config_u32(offset);
    if low & 1 != 0 {
        return None;
    }
    let wide = (low >> 1) & 0x3 == 2;
    let high = if wide { pci.read_config_u32(offset + 4) } else { 0 };
    let base = ((high as u64) << 32) | (low as u64 & !0xF);

    let command = pci.read_config_u16(HEADER_COMMAND);
    pci.write_config_u16(HEADER_COMMAND, command & !COMMAND_MEMORY_SPACE);
    pci.write_config_u32(offset, u32::MAX);
    let probed_low = pci.read_config_u32(offset);
    let probed_high = if wide {
        pci.write_config_u32(offset + 4, u32::MAX);
        let probed = pci.read_config_u32(offset + 4);
        pci.write_config_u32(offset + 4, high);
        probed
    } else {
        u32::MAX
    };
    pci.write_config_u32(offset, low);
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
fn codecs(hda: &PciDevice) {
    let Some(bar) = size_bar0(hda) else {
        log!("hda: (b) bar0 is not a memory BAR — the controller cannot be reached");
        return;
    };
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

    let regs = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(
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
    for address in 0..MAX_CODECS {
        if statests & (1 << address) == 0 {
            continue;
        }
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

/// One verb over the immediate-command registers: one write, one poll, one
/// read, and no DMA anywhere.
///
/// `None` is the controller not answering, which is a different fact from a
/// codec answering all ones — the caller separates them, because a controller
/// with no immediate-command implementation and a codec that has stopped look
/// identical to anything that folds them together.
fn verb(regs: Mmio, codec: u8, node: u8, verb: u32, payload: u32) -> Option<u32> {
    let command = ((codec as u32) << 28) | ((node as u32) << 20) | (verb << 8) | payload;

    if !settles(|| regs.read_u16(IMMEDIATE_STATUS) & IMMEDIATE_BUSY == 0) {
        return None;
    }
    regs.write_u16(IMMEDIATE_STATUS, IMMEDIATE_RESULT_VALID);
    regs.write_u32(IMMEDIATE_COMMAND, command);
    regs.write_u16(IMMEDIATE_STATUS, IMMEDIATE_BUSY);

    let done = settles(|| {
        let status = regs.read_u16(IMMEDIATE_STATUS);
        status & IMMEDIATE_BUSY == 0 && status & IMMEDIATE_RESULT_VALID != 0
    });
    if !done {
        return None;
    }
    let response = regs.read_u32(IMMEDIATE_RESPONSE);
    regs.write_u16(IMMEDIATE_STATUS, IMMEDIATE_RESULT_VALID);
    Some(response)
}

/// A 12-bit verb with an 8-bit payload, which is every verb this probe sends.
fn get(regs: Mmio, codec: u8, node: u8, cmd: u32, payload: u32) -> Option<u32> {
    verb(regs, codec, node, cmd, payload)
}

const VERB_GET_PARAMETER: u32 = 0xF00;
const VERB_GET_CONNECTION_LIST: u32 = 0xF02;
const VERB_GET_POWER_STATE: u32 = 0xF05;
const VERB_GET_PIN_CONTROL: u32 = 0xF07;
const VERB_GET_EAPD: u32 = 0xF0C;
const VERB_GET_CONFIG_DEFAULT: u32 = 0xF1C;

const PARAM_VENDOR_ID: u32 = 0x00;
const PARAM_REVISION_ID: u32 = 0x02;
const PARAM_SUB_NODE_COUNT: u32 = 0x04;
const PARAM_FUNCTION_TYPE: u32 = 0x05;
const PARAM_FUNCTION_CAPS: u32 = 0x08;
const PARAM_WIDGET_CAPS: u32 = 0x09;
const PARAM_PCM_RATES: u32 = 0x0A;
const PARAM_STREAM_FORMATS: u32 = 0x0B;
const PARAM_PIN_CAPS: u32 = 0x0C;
const PARAM_AMP_IN_CAPS: u32 = 0x0D;
const PARAM_CONNECTION_LENGTH: u32 = 0x0E;
const PARAM_POWER_STATES: u32 = 0x0F;
const PARAM_PROCESSING_CAPS: u32 = 0x10;
const PARAM_GPIO_COUNT: u32 = 0x11;
const PARAM_AMP_OUT_CAPS: u32 = 0x12;
const PARAM_VOLUME_KNOB_CAPS: u32 = 0x13;

fn param(regs: Mmio, codec: u8, node: u8, which: u32) -> Option<u32> {
    get(regs, codec, node, VERB_GET_PARAMETER, which)
}

/// A subordinate node count, as the start id and the count the codec declared.
///
/// The two are checked together and against the node id space, because a codec
/// claiming a range that runs past 255 is a codec whose walk would wrap — and
/// a wrapped walk re-reads node 0 and calls it a widget.
fn subordinates(regs: Mmio, codec: u8, node: u8, limit: u8, what: &str) -> Option<(u8, u8)> {
    let raw = param(regs, codec, node, PARAM_SUB_NODE_COUNT)?;
    if raw == NO_RESPONSE {
        log!("hda: codec{codec} node={node:#04x} answered all ones to its {what} count — stopping");
        return None;
    }
    let start = ((raw >> 16) & 0xFF) as u8;
    let count = (raw & 0xFF) as u8;
    if count == 0 {
        return None;
    }
    if start as u16 + count as u16 > 256 {
        log!(
            "hda: codec{codec} node={node:#04x} declares {count} {what}s from {start:#04x}, past \
             the node id space — refused"
        );
        return None;
    }
    if count > limit {
        log!(
            "hda: codec{codec} node={node:#04x} declares {count} {what}s, more than this probe \
             walks ({limit}) — the rest are not dumped"
        );
        return Some((start, limit));
    }
    Some((start, count))
}

/// Returns how many pins on this codec name a speaker as their default device.
fn dump_codec(regs: Mmio, codec: u8) -> usize {
    let Some(vendor) = param(regs, codec, 0, PARAM_VENDOR_ID) else {
        log!(
            "hda: codec{codec} the controller did not answer an immediate command — no verb \
             interface on this controller, so nothing below could be read"
        );
        return 0;
    };
    if vendor == NO_RESPONSE {
        log!(
            "hda: codec{codec} answered all ones to its vendor id — STATESTS claims a codec the \
             link does not carry"
        );
        return 0;
    }
    log!(
        "hda: codec{codec} vendor={:04x} device={:04x} revision={:#010x}",
        vendor >> 16,
        vendor & 0xFFFF,
        param(regs, codec, 0, PARAM_REVISION_ID).unwrap_or(NO_RESPONSE)
    );

    let Some((first, groups)) = subordinates(regs, codec, 0, MAX_FUNCTION_GROUPS, "function group")
    else {
        log!("hda: codec{codec} declares no function group");
        return 0;
    };

    let mut speakers = 0usize;
    for group in first..first.saturating_add(groups) {
        speakers += dump_function_group(regs, codec, group);
    }
    speakers
}

fn dump_function_group(regs: Mmio, codec: u8, group: u8) -> usize {
    let kind = param(regs, codec, group, PARAM_FUNCTION_TYPE).unwrap_or(NO_RESPONSE) & 0xFF;
    log!(
        "hda: codec{codec} fg={group:#04x} type={kind:#04x} ({}) caps={:#010x} power={:#010x} \
         gpio={:#010x}",
        function_type_name(kind),
        param(regs, codec, group, PARAM_FUNCTION_CAPS).unwrap_or(NO_RESPONSE),
        param(regs, codec, group, PARAM_POWER_STATES).unwrap_or(NO_RESPONSE),
        param(regs, codec, group, PARAM_GPIO_COUNT).unwrap_or(NO_RESPONSE),
    );
    // A modem function group is walked no further, and said so rather than
    // dropped: §2.3 step 1 keeps the audio groups and logs the rest.
    if kind != FUNCTION_TYPE_AUDIO {
        log!("hda: codec{codec} fg={group:#04x} is not an audio function group — not walked");
        return 0;
    }

    let Some((first, widgets)) = subordinates(regs, codec, group, MAX_WIDGETS, "widget") else {
        log!("hda: codec{codec} fg={group:#04x} declares no widget");
        return 0;
    };
    log!("hda: codec{codec} fg={group:#04x} widgets={first:#04x}..{:#04x}", first + widgets - 1);

    let mut speakers = 0usize;
    for node in first..first.saturating_add(widgets) {
        speakers += dump_widget(regs, codec, node);
    }
    speakers
}

const FUNCTION_TYPE_AUDIO: u32 = 0x01;

fn function_type_name(kind: u32) -> &'static str {
    match kind {
        0x00 => "reserved",
        FUNCTION_TYPE_AUDIO => "audio",
        0x02 => "modem",
        _ => "vendor/unknown",
    }
}

const WIDGET_PIN_COMPLEX: u32 = 4;

fn widget_type_name(kind: u32) -> &'static str {
    match kind {
        0 => "audio-out",
        1 => "audio-in",
        2 => "mixer",
        3 => "selector",
        WIDGET_PIN_COMPLEX => "pin",
        5 => "power",
        6 => "volume-knob",
        7 => "beep",
        0xF => "vendor-defined",
        _ => "reserved",
    }
}

/// One widget, in as many lines as it has facts. Returns 1 if this is a pin
/// complex whose configuration default names a speaker.
fn dump_widget(regs: Mmio, codec: u8, node: u8) -> usize {
    let caps = param(regs, codec, node, PARAM_WIDGET_CAPS).unwrap_or(NO_RESPONSE);
    if caps == NO_RESPONSE {
        log!("hda: codec{codec} node={node:#04x} answered all ones to its capabilities — skipped");
        return 0;
    }
    let kind = (caps >> 20) & 0xF;
    // `Chan Count Ext` in bits 15:13 and `Stereo` in bit 0 together, both
    // encoded one pair short of the count.
    let channels = (((caps >> 13) & 0x7) << 1 | (caps & 1)) + 1;
    log!(
        "hda: codec{codec} node={node:#04x} type={kind:#x} ({}) caps={caps:#010x} channels={channels} \
         amp-in={} amp-out={} power={} digital={} conn-override={}",
        widget_type_name(kind),
        yn(caps & (1 << 1) != 0),
        yn(caps & (1 << 2) != 0),
        yn(caps & (1 << 10) != 0),
        yn(caps & (1 << 9) != 0),
        yn(caps & (1 << 8) != 0),
    );

    if caps & (1 << 1) != 0 {
        log!(
            "hda: codec{codec} node={node:#04x} amp-in-caps={:#010x}",
            param(regs, codec, node, PARAM_AMP_IN_CAPS).unwrap_or(NO_RESPONSE)
        );
    }
    if caps & (1 << 2) != 0 {
        log!(
            "hda: codec{codec} node={node:#04x} amp-out-caps={:#010x}",
            param(regs, codec, node, PARAM_AMP_OUT_CAPS).unwrap_or(NO_RESPONSE)
        );
    }
    if kind == 0 || kind == 1 {
        log!(
            "hda: codec{codec} node={node:#04x} pcm={:#010x} formats={:#010x}",
            param(regs, codec, node, PARAM_PCM_RATES).unwrap_or(NO_RESPONSE),
            param(regs, codec, node, PARAM_STREAM_FORMATS).unwrap_or(NO_RESPONSE)
        );
    }
    if caps & (1 << 10) != 0 {
        log!(
            "hda: codec{codec} node={node:#04x} power-states={:#010x} power-state={:#010x}",
            param(regs, codec, node, PARAM_POWER_STATES).unwrap_or(NO_RESPONSE),
            get(regs, codec, node, VERB_GET_POWER_STATE, 0).unwrap_or(NO_RESPONSE)
        );
    }
    if kind == 6 {
        log!(
            "hda: codec{codec} node={node:#04x} volume-knob-caps={:#010x}",
            param(regs, codec, node, PARAM_VOLUME_KNOB_CAPS).unwrap_or(NO_RESPONSE)
        );
    }
    if caps & (1 << 6) != 0 {
        log!(
            "hda: codec{codec} node={node:#04x} processing-caps={:#010x}",
            param(regs, codec, node, PARAM_PROCESSING_CAPS).unwrap_or(NO_RESPONSE)
        );
    }

    connections(regs, codec, node);

    if kind == WIDGET_PIN_COMPLEX {
        return pin(regs, codec, node);
    }
    0
}

/// The widget's connection list, decoded to node ids.
///
/// §2.3 step 4 walks this list backwards to find a converter, so it is the part
/// of the dump the traversal is tested against and the part a codec is most
/// able to lie about. Range entries are expanded here — the short form marks a
/// range end in bit 7 of an entry and the long form in bit 15 — and an expansion
/// that would run backwards is printed as the raw pair rather than guessed at.
fn connections(regs: Mmio, codec: u8, node: u8) {
    let Some(raw) = param(regs, codec, node, PARAM_CONNECTION_LENGTH) else { return };
    if raw == NO_RESPONSE {
        return;
    }
    let long = raw & (1 << 7) != 0;
    let declared = (raw & 0x7F) as u8;
    if declared == 0 {
        return;
    }
    let count = declared.min(MAX_CONNECTIONS);
    if count < declared {
        log!(
            "hda: codec{codec} node={node:#04x} declares {declared} connections, more than this \
             probe walks ({MAX_CONNECTIONS}) — the rest are not dumped"
        );
    }

    let per_response = if long { 2 } else { 4 };
    let mut entries: Vec<u16> = Vec::new();
    let mut index = 0u32;
    while (entries.len() as u8) < count {
        let Some(response) = get(regs, codec, node, VERB_GET_CONNECTION_LIST, index) else {
            return;
        };
        for slot in 0..per_response {
            if (entries.len() as u8) >= count {
                break;
            }
            entries.push(if long {
                ((response >> (slot * 16)) & 0xFFFF) as u16
            } else {
                ((response >> (slot * 8)) & 0xFF) as u16
            });
        }
        index += per_response as u32;
    }

    let range_bit: u16 = if long { 1 << 15 } else { 1 << 7 };
    let mut line: Vec<u16> = Vec::new();
    let mut previous: Option<u16> = None;
    for entry in &entries {
        let id = entry & !range_bit;
        match (entry & range_bit != 0, previous) {
            (true, Some(from)) if id > from => line.extend((from + 1)..=id),
            // A range whose end is not above its start is not a range. Printed
            // as the entry it is, because a probe that guessed which way the
            // codec meant it would put a node id in a fixture that the codec
            // never named.
            (true, _) => line.push(id),
            (false, _) => line.push(id),
        }
        previous = Some(id);
    }

    log!(
        "hda: codec{codec} node={node:#04x} conn-len={raw:#010x} long={} count={} list={:?}",
        yn(long),
        line.len(),
        line
    );
}

const PIN_DEVICE_SPEAKER: u32 = 0x1;
const PIN_DEVICE_HEADPHONE: u32 = 0x2;

fn pin_device_name(device: u32) -> &'static str {
    match device {
        0x0 => "line-out",
        PIN_DEVICE_SPEAKER => "speaker",
        PIN_DEVICE_HEADPHONE => "hp-out",
        0x3 => "cd",
        0x4 => "spdif-out",
        0x5 => "digital-other-out",
        0x6 => "modem-line",
        0x7 => "modem-handset",
        0x8 => "line-in",
        0x9 => "aux",
        0xA => "mic-in",
        0xB => "telephony",
        0xC => "spdif-in",
        0xD => "digital-other-in",
        0xF => "other",
        _ => "reserved",
    }
}

fn pin_connectivity_name(connectivity: u32) -> &'static str {
    match connectivity {
        0 => "jack",
        1 => "none",
        2 => "fixed",
        3 => "jack+fixed",
        _ => unreachable!("two bits"),
    }
}

fn pin(regs: Mmio, codec: u8, node: u8) -> usize {
    let config = get(regs, codec, node, VERB_GET_CONFIG_DEFAULT, 0).unwrap_or(NO_RESPONSE);
    let connectivity = (config >> 30) & 0x3;
    let device = (config >> 20) & 0xF;
    log!(
        "hda: codec{codec} node={node:#04x} pin-caps={:#010x} pin-ctl={:#010x} eapd={:#010x}",
        param(regs, codec, node, PARAM_PIN_CAPS).unwrap_or(NO_RESPONSE),
        get(regs, codec, node, VERB_GET_PIN_CONTROL, 0).unwrap_or(NO_RESPONSE),
        get(regs, codec, node, VERB_GET_EAPD, 0).unwrap_or(NO_RESPONSE),
    );
    log!(
        "hda: codec{codec} node={node:#04x} cfgdef={config:#010x} conn={} device={} location={:#04x} \
         type={:#x} colour={:#x} assoc={} sequence={}",
        pin_connectivity_name(connectivity),
        pin_device_name(device),
        (config >> 24) & 0x3F,
        (config >> 16) & 0xF,
        (config >> 12) & 0xF,
        (config >> 4) & 0xF,
        config & 0xF,
    );
    // §2.3 step 3 discards a pin whose port connectivity says no physical
    // connection, whatever its default device claims — a header nobody
    // soldered still names a speaker.
    usize::from(device == PIN_DEVICE_SPEAKER && connectivity != 1)
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
