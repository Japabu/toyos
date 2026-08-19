mod device;
mod hid;
mod legacy;
mod wait;

use wait::msc;

/// The driver's whole surface to the rest of the kernel. Everything that waits
/// lives under [`wait`] — see its own documentation for why that is a module
/// boundary and not a type.
pub use wait::boot::{init, PORT_POLL_NS, PORT_SETTLE_CEILING_NS};
pub use wait::msc::{storage_flush, storage_read, storage_write};

use alloc::vec::Vec;
use core::num::NonZeroU8;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use crate::mm::Mmio;
use crate::mm::KernelSlice;
use crate::log;
use super::pci::PciDevice;
use super::DmaPool;
use crate::sync::Lock;
use toyos_xhci::job::{Await, Outcome, Outstanding, Stages};
use toyos_xhci::port::{self as portmachine, GaveUp, Gone, PortState, Reset, Step};
use toyos_xhci::recovery::{self, Act, EndpointState, NeedsConfigure, Recovery};
use toyos_xhci::Protocols;
use toyos_xhci::Portsc;

use hid::HidDevice;

// xHCI Capability Register offsets (from BAR0)
const CAP_CAPLENGTH:  u64 = 0x00; // u8
const CAP_HCSPARAMS1: u64 = 0x04; // u32
const CAP_HCSPARAMS2: u64 = 0x08; // u32
const CAP_HCCPARAMS1: u64 = 0x10; // u32
const CAP_DBOFF:      u64 = 0x14; // u32
const CAP_RTSOFF:     u64 = 0x18; // u32

// xHCI Operational Register offsets (from op_base = BAR0 + cap_length)
const OP_USBCMD:   u64 = 0x00;
const OP_USBSTS:   u64 = 0x04;
const OP_PAGESIZE: u64 = 0x08;
const OP_CRCR:     u64 = 0x18; // 64-bit
const OP_DCBAAP:   u64 = 0x30; // 64-bit
const OP_CONFIG:   u64 = 0x38;
const OP_PORT_BASE: u64 = 0x400;
const PORT_REG_SIZE: u64 = 0x10;

// The raw bits, for the two paths that work on a word rather than on a decoded
// register: the feature-gated injections in `read_portsc`, which hide bits a
// controller reported, and `init_one`'s port power, which runs before this
// controller exists. Every decision goes through `Portsc`.
const PORTSC_CCS: u32 = 1 << 0;
const PORTSC_PED: u32 = 1 << 1;
const PORTSC_PR:  u32 = 1 << 4;
const PORTSC_PP:  u32 = 1 << 9;
const PORTSC_SPEED: u32 = 0xF << 10;

/// HCCPARAMS1 bit 3: the controller has Port Power Control, which is also what
/// decides whether PORTSC's PP comes out of a reset clear or set.
const HCC_PPC: u32 = 1 << 3;

// Runtime Register offsets (from rt_base = BAR0 + rts_offset)
// Interrupter 0 starts at offset 0x20
const IR0_IMAN:   u64 = 0x20; // Interrupt Management (IP + IE)
const IR0_IMOD:   u64 = 0x24; // Interrupt Moderation
const IR0_ERSTSZ: u64 = 0x28;
const IR0_ERSTBA: u64 = 0x30; // 64-bit
const IR0_ERDP:   u64 = 0x38; // 64-bit

// xHCI interrupt vector
const XHCI_VECTOR: u8 = 0x21;

// TRB (Transfer Request Block) — 16 bytes
#[repr(C)]
#[derive(Clone, Copy)]
struct Trb {
    param: u64,
    status: u32,
    control: u32,
}

impl Trb {
    const ZERO: Self = Self { param: 0, status: 0, control: 0 };
}

const TRB_CYCLE: u32 = 1;

// TRB type field is bits [15:10]
const fn trb_type(t: u32) -> u32 { t << 10 }

const TRB_NORMAL:       u32 = trb_type(1);
const TRB_SETUP_STAGE:  u32 = trb_type(2);
const TRB_DATA_STAGE:   u32 = trb_type(3);
const TRB_STATUS_STAGE: u32 = trb_type(4);
const TRB_LINK:         u32 = trb_type(6);

const TRB_ENABLE_SLOT:    u32 = trb_type(9);
const TRB_DISABLE_SLOT:   u32 = trb_type(10);
const TRB_ADDRESS_DEVICE: u32 = trb_type(11);
const TRB_CONFIGURE_EP:   u32 = trb_type(12);
const TRB_EVALUATE_CONTEXT: u32 = trb_type(13);
const TRB_RESET_ENDPOINT: u32 = trb_type(14);
const TRB_STOP_ENDPOINT:  u32 = trb_type(15);
const TRB_SET_TR_DEQUEUE: u32 = trb_type(16);

// Event TRB types (read from event ring, encoded in bits [15:10])
const EVENT_TRANSFER:     u32 = 32;
const EVENT_CMD_COMPLETE: u32 = 33;
const EVENT_PORT_STATUS_CHANGE: u32 = 34;

// Completion codes worth naming. A transfer that moved less than it asked for
// reports Short Packet, which is a success with a residue and not an error —
// reading it as one is the classic mass-storage bug, since every SCSI command
// that under-delivers takes that path.
const CC_SUCCESS: u32 = 1;
const CC_STALL: u32 = 6;
const CC_SHORT_PACKET: u32 = 13;

/// A completion code, named where xHCI 1.2 Table 6-90 names it.
///
/// The bare number is what every line in this driver used to print, and the
/// device those lines are about is one that has stopped working on a machine
/// with no debugger and often no serial port: `code 6` and `code 6 (Stall
/// Error)` are the difference between reaching for the specification and
/// reading the answer. The number is always there, because a controller can
/// report one the table does not define and that is the case worth carrying
/// verbatim.
#[derive(Clone, Copy)]
struct Completion(u32);

impl core::fmt::Display for Completion {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let named = match self.0 {
            1 => "Success",
            2 => "Data Buffer Error",
            3 => "Babble Detected",
            4 => "USB Transaction Error",
            5 => "TRB Error",
            6 => "Stall Error",
            7 => "Resource Error",
            8 => "Bandwidth Error",
            9 => "No Slot Available",
            10 => "Invalid Stream Type",
            11 => "Slot Not Enabled",
            12 => "Endpoint Not Enabled",
            13 => "Short Packet",
            14 => "Ring Underrun",
            15 => "Ring Overrun",
            16 => "VF Event Ring Full",
            17 => "Parameter Error",
            18 => "Bandwidth Overrun",
            19 => "Context State Error",
            20 => "No Ping Response",
            21 => "Event Ring Full",
            22 => "Incompatible Device",
            23 => "Missed Service Error",
            24 => "Command Ring Stopped",
            25 => "Command Aborted",
            26 => "Stopped",
            27 => "Stopped - Length Invalid",
            28 => "Stopped - Short Packet",
            29 => "Max Exit Latency Too Large",
            31 => "Isoch Buffer Overrun",
            32 => "Event Lost",
            33 => "Undefined Error",
            34 => "Invalid Stream ID",
            35 => "Secondary Bandwidth Error",
            36 => "Split Transaction Error",
            _ => return write!(f, "code {}", self.0),
        };
        write!(f, "code {} ({named})", self.0)
    }
}

/// What the controller's answer to the one outstanding operation is *for*.
///
/// Every variant is work the driver used to do by spinning inside a scheduler
/// pass, which is what pulling the boot stick out of a T14 runs.
enum What {
    /// Disable Slot, and what stops being reachable once it has completed.
    SlotGone { slot: u8, then: AfterSlot },
    /// One step of a HID interrupt endpoint's way back to Running. The
    /// sequence travels with the wait because the step after this one is a
    /// function of where it started, and nothing else holds that; `issued`
    /// travels with it because a failure has to name the command, and by the
    /// time one is read the pass that sent it has long returned.
    Recovering { slot_id: u8, seq: Recovery, issued: &'static str },
    /// Enable Slot for a port that has finished its reset. Its own variant
    /// because until the controller answers there is no device: no slot id, no
    /// pool block and no EP0 ring, and every act after this one carries all
    /// three.
    SlotWanted { port_idx: u8, speed: u8, packet: u16, seq: toyos_xhci::enumerate::Enumeration },
    /// One act of a device's enumeration.
    Enumerating(device::Enumerating),
}

/// What the controller said about one outstanding operation, for the line a
/// refusal produces. Every failure in this driver reads the same way whether
/// the controller answered badly or not at all, and the difference is what a
/// person reaching for the specification needs first.
struct Answer(Outcome);

impl core::fmt::Display for Answer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Outcome::Command { code, .. } | Outcome::Transfer { code, .. } => {
                write!(f, "{}", Completion(code))
            }
            Outcome::Silent => {
                write!(f, "no answer in {} ms", USB_TIMEOUT_NS / 1_000_000)
            }
        }
    }
}

/// Which device a line is about: the controller, and the slot on it.
///
/// **A slot id is the controller's own numbering, and a machine has more than
/// one controller.** `Profile::MetalHotplug` boots the disk as slot 1 on
/// `00:02.0` and hot-plugs the mouse as slot 1 on `00:03.0`, so a line saying
/// `slot 1` names two different devices and nothing reading the log can tell
/// which. That is not hypothetical: a harness assertion counting endpoint
/// recoveries counted the boot disk's as a mouse's on three CI runs
/// (`issues/hardware/xhci-hid-break-counts-any-endpoint-3.md`), and the
/// same shape counted the boot stick's transport recovery as the disk under
/// test's (`…/usb-transport-break-counts-the-boot-sticks-recovery.md`).
///
/// So every line the recovery path writes carries this, and `pci` on
/// [`XhciController`] is the field that makes it available.
#[derive(Clone, Copy)]
struct Slot {
    bus: u8,
    dev: u8,
    func: u8,
    id: u8,
}

impl core::fmt::Display for Slot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{} slot {}", self.bus, self.dev, self.func, self.id)
    }
}

/// Why a slot was given back, which is what decides what goes with it.
enum AfterSlot {
    /// A port's device has left the bus. Its pool blocks belong to the next
    /// device the moment the slot does, and the port becomes one the machine
    /// may enumerate again.
    Teardown(u8),
    /// A device this driver gave up on while it is still in its port, so the
    /// port stays marked attached — see [`XhciController::let_go`].
    LetGo,
}

/// The earlier of two instants something wants to be looked at again.
fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (at, None) | (None, at) => at,
    }
}

/// Put one control transfer's TRBs on an EP0 ring, and say whether it carries
/// a data stage — which is what decides how many completions it produces and
/// therefore what a caller has to wait for.
///
/// Separate from the wait because the two ends have different callers: an
/// endpoint recovery stepped across scheduler passes submits here and comes
/// back for the completion, and every other control transfer in this driver
/// waits for it in place.
fn enqueue_control(
    ring: &mut TrbRing,
    bm_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    data_buf: Option<u64>,
    data_len: u16,
) -> bool {
    let is_in = (bm_request_type & 0x80) != 0;
    let has_data = data_len > 0 && data_buf.is_some();
    let trt = if !has_data { 0u32 } else if is_in { 3 } else { 2 };

    let mut setup = Trb::ZERO;
    setup.param = setup_packet(bm_request_type, b_request, w_value, w_index, data_len);
    setup.status = 8;
    setup.control = TRB_SETUP_STAGE | (1 << 6) | (trt << 16);
    ring.enqueue(setup);

    if let Some(buf) = data_buf.filter(|_| has_data) {
        let mut data = Trb::ZERO;
        data.param = buf;
        data.status = data_len as u32;
        let dir = if is_in { 1u32 << 16 } else { 0 };
        // ISP and IOC, which this TRB carried neither of. Without IOC the data
        // stage produces no event at all and the only thing the driver ever
        // sees is the status stage's Success; without ISP a device that answers
        // short is not required to say so. Between them the two are the whole
        // of "how many bytes are actually in that buffer", and a descriptor
        // read has no other way to ask.
        data.control = TRB_DATA_STAGE | dir | (1 << 2) | (1 << 5);
        ring.enqueue(data);
    }

    let mut status = Trb::ZERO;
    let status_dir = if has_data && is_in { 0 } else { 1u32 << 16 };
    status.control = TRB_STATUS_STAGE | (1 << 5) | status_dir;
    ring.enqueue(status);
    has_data
}

/// The line for an endpoint no sequence of commands takes back to Running.
/// Two callers — the recovery that waits and the one that is stepped — and the
/// same endpoint whichever asked.
fn log_unrecoverable(slot: Slot, dci: u8, state: EndpointState) {
    log!("xHCI: {slot} endpoint {dci} is {state}; nothing short of Configure Endpoint \
         takes an endpoint out of that, and this driver does not re-configure a bound device");
}

/// How many transfers one HID interrupt endpoint may fail in a row before the
/// device it belongs to is let go.
///
/// Policy, not physics. The count is *consecutive* and a delivered report
/// clears it, so a device that glitches once is never let go for it; and a
/// device that fails every transfer is let go on its own service interval
/// rather than costing a recovery per poll for the life of the boot. That cost
/// is not abstract: each recovery is two commands and a spin on the event ring,
/// taken inside `poll_if_pending` at the top of a scheduler pass, which is the
/// path the audio pipeline runs on.
///
/// What the caller sees when it is hit is [`XhciController::let_go`]: the
/// device is named, its keys or its button-table entry are given back, its slot
/// is disabled, and the line says to unplug it — because a port left marked
/// attached is the one thing that stops the driver enumerating the same
/// endpoint again every debounce.
const MAX_HID_FAILURES: u8 = 8;

/// How long the driver waits on any one command or transfer.
///
/// A device that never answers must cost that device and not the CPU that
/// asked it — which is the whole reason this exists, because every wait in
/// this driver used to be an unbounded `spin_loop`. The bound is generous on
/// purpose: the transfers it covers complete in microseconds even under TCG,
/// so nothing but a dead device can reach it.
const USB_TIMEOUT_NS: u64 = 2_000_000_000;

/// When a wait started now would give up. Before `clock::init` this is 0 plus
/// the timeout and `nanos_since_boot` stays 0, so the wait is unbounded — the
/// behaviour this driver had everywhere, and reachable only by a caller that
/// runs before phase 2.
fn deadline() -> u64 {
    crate::clock::nanos_since_boot() + USB_TIMEOUT_NS
}

/// Let a test starve one of those waits on a controller that is otherwise
/// answering perfectly.
///
/// Kernel features because nothing on the host side can stage them: QEMU's xHC
/// halts, resets, clears CNR and starts in microseconds, and its ports finish a
/// reset synchronously — there is no device or machine property that makes a
/// register bit not settle, and unplugging between the scan and the reset is
/// not expressible either. The rest of bring-up runs unchanged, so what these
/// certify is the deadline and the refusal, which is exactly the code that has
/// no other way to execute. Same reason `xhci-one-slot` and `i8042-fault`
/// exist.
fn controller_answers() -> bool {
    !crate::actuator::xhci_deaf_controller()
}

fn port_answers() -> bool {
    !crate::actuator::xhci_deaf_port()
}

/// How long a mass-storage bulk transfer's completion is held back before the
/// driver may see it.
///
/// **A kernel feature because nothing on the host side can stage it, and the
/// one QEMU property this suite most needs.** `usb-storage` answers a CBW, a
/// data phase and a CSW in microseconds, and no device, drive or machine
/// property makes one answer late: `rerror`/`werror` fail a transfer rather
/// than delaying it, and QEMU's block layer throttling is per *drive* I/O and
/// does not reach the USB transport's completion at all. A USB flash stick's
/// 4 KiB write, on the other hand, is tens of milliseconds — the erase block is
/// the reason and every stick has one — and that is the whole of what the
/// T14's audio pops are made of (`issues/audio/disk-wait-pins-a-cpu.md`).
///
/// What is replaced is *when the controller publishes the event*, not the
/// event. The TRB really ran, the completion code is the controller's own and
/// the bytes really moved; the driver simply does not get to see the Transfer
/// Event until it is this old, which is the state a slow device leaves behind
/// and the only state in which the cost of waiting for one is visible. Same
/// reason `usb-transport-break` and `xhci-slow-connect` exist.
///
/// Two milliseconds: a Bulk-Only round trip is three transfers, and one of
/// `/bin/logd`'s flushes is a page write, a FAT entry, a directory entry and a
/// SYNCHRONIZE CACHE — so about ten round trips, which puts a flush at the ~50
/// ms the T14 measured. It is deliberately *not* one stick's number: what the
/// gate asserts is that the machine stays responsive while a device is slow,
/// and any value large against a 2.902 ms audio period asks that question.
const SLOW_TRANSFER_NS: u64 = 2_000_000;

/// The boot-time connect settle measures the same interval the per-port machine
/// does, so it reads it from the same place.
use portmachine::DEBOUNCE_NS as PORT_DEBOUNCE_NS;


/// Report an empty root hub for the first [`SLOW_CONNECT_NS`] of the boot.
///
/// A kernel feature because nothing on the host side can stage it. QEMU fills
/// PORTSC in from the QOM tree — an attached device reads CCS the instant the
/// register is touched, before and after HCRST alike — so "connected in 300 ms,
/// not now", which is what every physical root hub does after a controller
/// reset, is not expressible as a device or a machine property. `device_add`
/// cannot reach it either: the port scan runs in the peripheral phase, tens of
/// milliseconds into a boot, and QMP cannot be aimed at that window.
///
/// What is replaced is the *register*, not a verdict. During the window the
/// port reads exactly as an unpopulated one does — no CCS, no PED, speed zero —
/// so a driver that believes it gets nothing to enumerate, and the device that
/// appears afterwards is enumerated by the ordinary path with the ordinary
/// bytes behind it. Same reason `xhci-one-slot` and `xhci-deaf-port` exist.
///
/// [`xhci-slow-storage-connect`](SLOW_STORAGE_PORT) hides one port instead of
/// all of them, which is a different machine and not a weaker version of this
/// one — and it closes its window on the boot scan rather than on this clock,
/// because what it stages is an ordering and what this stages is a duration the
/// settle has to keep looking through.
const SLOW_CONNECT_NS: u64 = 300_000_000;

/// Report *one* root-hub port empty for the first [`SLOW_CONNECT_NS`], while
/// every other port on the machine reads normally.
///
/// The machine [`xhci-slow-connect`](SLOW_CONNECT_NS) cannot stage, and the one
/// the T14 is. [`await_connect_settle`] stops looking as soon as the connect set
/// has held still for [`PORT_DEBOUNCE_NS`] **and is non-empty**, so a bus whose
/// other devices have settled settles on them — and the laptop has four internal
/// USB devices (camera, Bluetooth, card reader, fingerprint reader) that come up
/// beside the stick it boots from. Hiding the whole bus exercises the
/// keep-looking path and can never reach this one, because the condition that
/// ends the wait early is precisely the presence of the devices it hides.
///
/// Port index 0 because that is where the boot stick lands: it is the only
/// SuperSpeed device the profiles attach, so it takes the SuperSpeed view of the
/// first port register while every HID takes the USB2 view of a later one.
///
/// **The window is the boot scan itself and not a span of nanoseconds**, which
/// [`SLOW_CONNECT_NS`] is and which this used to share. What has to be staged is
/// "the disk arrives after `scan_ports`", and 300 ms is a claim about how far
/// into a boot that runs — true at 253 ms on the dev host and false at 407 ms on
/// the runner that red `late_storage_connect` on CI run `31286199802`, where the
/// scan bound the disk and the gate correctly reported that it was measuring an
/// ordinary boot. The scan is an event, so it is the event that closes this.
const SLOW_STORAGE_PORT: u8 = 0;

/// Whether the boot port scan has run. Until it has, [`SLOW_STORAGE_PORT`]
/// reads unpopulated.
pub(super) static BOOT_SCAN_DONE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// One bit per root-hub port. HCSPARAMS1's MaxPorts is a byte, so four words
/// cover every controller that can exist, and "did the connect state change"
/// is one comparison rather than a per-port history.
type PortMask = [u64; 4];

fn port_bit(mask: &PortMask, port_idx: u8) -> bool {
    mask[port_idx as usize / 64] & (1 << (port_idx % 64)) != 0
}

/// When some controller's port state machine must be stepped again, or 0 when
/// no controller has a port outstanding.
///
/// Two readers, and neither may take [`XHCI`]: `poll_if_pending` runs on every
/// CPU at the top of every scheduler pass, and the idle loop's final recheck
/// runs with interrupts off. 0 as "nothing" is the same encoding `irq_ring`
/// uses for the same reason — every value written here is
/// `nanos_since_boot() + PORT_DEBOUNCE_NS` or larger, so the boot instant is
/// not a deadline this can hold.
///
/// **A CPU with nothing else to run must not sleep while this is set**, and
/// that is what it is for. Nothing else would wake it: the connect edge that
/// started the debounce was the last interrupt the controller had to give, and
/// the scheduler arms its one-shot timer for parked *tasks*, of which a
/// driver's deferred work is not one. The cost is one idle CPU declining to
/// halt for at most the debounce, or for the reset deadline behind it —
/// bounded, self-clearing, and paid only by a machine that has just been
/// plugged into.
static PORT_WORK_AT: AtomicU64 = AtomicU64::new(0);

/// Whether a CPU with nothing to run must stay awake for [`PORT_WORK_AT`].
pub fn port_work_pending() -> bool {
    PORT_WORK_AT.load(Ordering::Relaxed) != 0
}

/// Read every root-hub port again now, whatever the driver last recorded, and
/// step whatever has changed since.
///
/// **The boot scan is not a census, and nothing here ever claimed it was.**
/// [`await_connect_settle`] returns as soon as the connect set has held still
/// for [`PORT_DEBOUNCE_NS`] and is non-empty, so a machine whose other devices
/// are up settles on *them* and [`device::scan_ports`] runs without whatever is
/// still coming. The T14 has four internal USB devices beside the stick it boots
/// from, which is how that machine reached a working desktop with no `/boot` and
/// no `/log` on one boot and mounted both on the next.
///
/// [`poll_if_pending`] cannot be used for this and the difference is the whole
/// reason this exists: it returns without looking unless an interrupt was
/// recorded or [`PORT_WORK_AT`] is due, and the end of a boot scan stores zero
/// there precisely because nothing was left outstanding. This is for a caller
/// that has a reason of its own to keep looking —
/// `fat32_adapter::probe_boot_disks`, which knows firmware named a partition
/// that nothing on this machine carries yet.
pub fn recheck_ports() {
    let mut wake_at: Option<u64> = None;
    for ctrl in XHCI.lock().iter_mut() {
        ctrl.ports_dirty = true;
        if let Some(at) = ctrl.poll() {
            wake_at = Some(wake_at.map_or(at, |w: u64| w.min(at)));
        }
    }
    PORT_WORK_AT.store(wake_at.unwrap_or(0), Ordering::Relaxed);
}

const RING_SIZE: usize = 256; // TRBs per ring (one page = 256 * 16)

/// Event Ring Segment Table entry (16 bytes).
#[repr(C)]
struct ErstEntry {
    ring_base: u64,
    ring_size: u32,
    _reserved: u32,
}

#[derive(Clone, Copy)]
struct TrbRing {
    base: *mut Trb,
    base_phys: u64,
    tail: u16,
    cycle: bool,
}

impl TrbRing {
    /// A ring the controller has never seen: zeroed, with the wrap link TRB
    /// already at the last slot and the enqueue pointer at the first.
    ///
    /// Also the recovery primitive. After a stall the controller's dequeue
    /// pointer is somewhere in the middle of a ring holding TRBs it will never
    /// run, so recovery is this plus a Set TR Dequeue Pointer naming
    /// [`Self::dequeue`] — the two have to agree or the endpoint resumes on
    /// stale TRBs.
    fn init(buf: KernelSlice) -> Self {
        assert!(buf.size() >= RING_SIZE * core::mem::size_of::<Trb>());
        unsafe { buf.zero(); }
        let mut link = Trb::ZERO;
        link.param = buf.phys();
        link.control = TRB_LINK | (1 << 1); // TC (Toggle Cycle)
        unsafe { write_volatile((buf.base() as *mut Trb).add(RING_SIZE - 1), link); }
        Self { base: buf.base() as *mut Trb, base_phys: buf.phys(), tail: 0, cycle: true }
    }

    /// Where the controller should resume, with the cycle state it must expect.
    ///
    /// A TRB is 16 bytes, so the address is 16-byte aligned and bit 0 is free
    /// for the cycle state. Parenthesised because `+` and `*` both bind tighter
    /// than `|`, and this should not need that table to read.
    fn dequeue(&self) -> u64 {
        (self.base_phys + (self.tail as u64) * 16) | (self.cycle as u64)
    }

    /// Put `trb` on the ring and answer with **where it landed**, which for the
    /// command ring is the only name a Command Completion Event gives it
    /// (xHCI 1.2 §6.4.2.2). A caller matching on anything coarser than that —
    /// "the next completion of any command" — takes the answer belonging to a
    /// command that ran out its deadline and replied afterwards.
    fn enqueue(&mut self, mut trb: Trb) -> u64 {
        if self.cycle {
            trb.control |= TRB_CYCLE;
        } else {
            trb.control &= !TRB_CYCLE;
        }
        let at = self.base_phys + (self.tail as u64) * 16;
        unsafe { write_volatile(self.base.add(self.tail as usize), trb); }
        self.tail += 1;

        if self.tail as usize >= RING_SIZE - 1 {
            let mut link = Trb::ZERO;
            link.param = self.base_phys;
            link.control = TRB_LINK | (1 << 1); // TC (Toggle Cycle)
            if self.cycle { link.control |= TRB_CYCLE; }
            unsafe { write_volatile(self.base.add(self.tail as usize), link); }
            self.tail = 0;
            self.cycle = !self.cycle;
        }
        at
    }
}

/// The granularity every structure below is placed at. It is the xHCI
/// PAGESIZE the controller reports, and no shipping xHC reports anything else;
/// `init` asserts the register rather than trusting the coincidence.
const PAGE: usize = 0x1000;

// The pool's fixed head. Everything here is either the controller's own state
// or enumeration scratch, and there is exactly one of each because enumeration
// is serial — see `device::init_device`.
// The whole table is `N * PAGE` and reads as one column of page numbers; the
// two that reduce are not written differently from the four that do not.
#[allow(clippy::erasing_op)]
const OFF_DCBAA: usize     = 0 * PAGE; // (max_slots + 1) * 8, 2 KiB at most
#[allow(clippy::identity_op)]
const OFF_CMD_RING: usize  = 1 * PAGE;
const OFF_ERST: usize      = 2 * PAGE;
const OFF_EVT_RING: usize  = 3 * PAGE;
const OFF_INPUT_CTX: usize = 4 * PAGE; // 33 contexts, so 2112 B at ctx_size 64
const OFF_DATA_BUF: usize  = 5 * PAGE;
const SHARED_SIZE: usize   = 6 * PAGE;

// One of these per device the controller gives us a slot for. All four
// outlive enumeration: the controller writes the output context and the report
// buffer for as long as the device is attached, and the interrupt ring carries
// that device's transfers. Sharing any of them between two devices is a
// silent data race, which is what keying the interrupt ring by HID class did.
//
// The EP0 ring is here for the same reason and used to be one shared page.
// That was sound only while every control transfer happened during that
// device's own enumeration: the ring is rewound for each device, so a device
// enumerated earlier has an EP0 dequeue pointer into a ring whose contents and
// cycle state have since moved under it. Mass storage is the first thing that
// needs to talk to a device *after* boot — Clear-Feature(HALT) and Bulk-Only
// Reset are control transfers on the recovery path — so the ring has to belong
// to the device rather than to the enumeration.
const DEV_INT_RING: usize = 0;                 // 256 TRBs, exactly one page
const DEV_EP0_RING: usize = PAGE;              // likewise
const DEV_OUT_CTX: usize  = 2 * PAGE;          // 32 contexts, 2 KiB at ctx_size 64
const DEV_REPORT: usize   = 2 * PAGE + 0x800;  // 8 B, the largest boot report
const DEV_STRIDE: usize   = 3 * PAGE;

// One of these per mass-storage device, and separate from the device block
// above because the two are three orders of magnitude apart in appetite: a
// keyboard needs 8 bytes of report buffer and a disk needs a transfer buffer.
// Folding the larger into `DEV_STRIDE` would hand every keyboard, hub and
// camera on the bus a 64 KiB block it never touches, and would divide the
// number of devices the pool can track by eight.
const MSC_IN_RING: usize   = 0;
const MSC_OUT_RING: usize  = PAGE;
const MSC_CBW: usize       = 2 * PAGE;         // 31 B
const MSC_CSW: usize       = 2 * PAGE + 0x40;  // 13 B
const MSC_SCRATCH: usize   = 2 * PAGE + 0x80;  // INQUIRY, READ CAPACITY, sense
const MSC_SCRATCH_LEN: usize = 64;
/// The bulk data buffer, placed so it cannot cross a 64 KiB boundary — which
/// is the one placement rule an xHCI Normal TRB's buffer has. `msc_base` is
/// aligned to `MSC_STRIDE` and the pool's physical base is 2 MiB aligned, so
/// every block starts on a 64 KiB boundary and this buffer occupies its
/// second half exactly.
const MSC_DATA: usize      = 8 * PAGE;
const MSC_DATA_LEN: usize  = 8 * PAGE;
const MSC_STRIDE: usize    = 16 * PAGE;

/// Mass-storage devices the pool has blocks for. Two, because that is what a
/// machine booting off a USB stick with a second one plugged in has, and each
/// costs 64 KiB whether or not it is used. A third stick is refused by name
/// rather than served from somebody else's block.
const MSC_BLOCKS: usize = 2;

/// The largest run of 4 KiB blocks one SCSI command moves, which is the data
/// buffer over the trait's block size. Every caller-facing loop batches to it.
const MSC_MAX_BLOCKS: u32 = (MSC_DATA_LEN / 4096) as u32;

/// Device blocks to size the pool for before the controller's slot count is
/// consulted.
///
/// A scratchpad demand that lands `dev_base` on or just under a 2 MiB boundary
/// leaves little or nothing in the page it forced us to allocate anyway, and
/// then MaxSlotsEn is written as that number and the controller enumerates
/// nothing. This is not defensive padding; it is what keeps the pool from
/// having no room for devices.
///
/// Swept over all 1024 demands HCSPARAMS2's two 5-bit fields can express:
/// without this floor `dev_blocks` is **0** for 32 of them (458–473 and
/// 969–984) and as few as 5 for another 32 (442–457, 953–968); with it the
/// smallest is 10, at 426.
const MIN_DEVICE_BLOCKS: usize = 8;

/// Cap the driver at one device block, so a test can drive the path where the
/// controller hands back a slot the pool has no room for. Nothing else can
/// stage it: QEMU's `nec-usb-xhci,slots=N` does not reach HCSPARAMS1 and its
/// Enable Slot ignores MaxSlotsEn, and a real pool holds ~250 devices.
fn device_ceiling() -> usize {
    if crate::actuator::xhci_one_slot() {
        1
    } else {
        usize::MAX
    }
}

/// Where each structure sits in the pool, derived from what the controller
/// reported. Nothing here is a constant except the strides above.
#[derive(Clone, Copy)]
struct Layout {
    scratch_array: usize,
    scratch_buffers: usize,
    scratch_count: usize,
    msc_base: usize,
    dev_base: usize,
    /// Device blocks the pool holds, which is also the MaxSlotsEn written to
    /// CONFIG: the controller is told exactly what the driver can track.
    dev_blocks: usize,
    pool_size: usize,
}

impl Layout {
    /// `max_scratchpad` and `max_slots` come straight off HCSPARAMS, which is
    /// where every number below stops being arbitrary.
    ///
    /// It is also what makes the plain `align_2m` below safe where every other
    /// caller taking a size from outside the kernel needs `align_2m_checked`:
    /// `max_scratchpad` is two 5-bit HCSPARAMS2 fields (`init` masks both with
    /// `0x1F`), so it is at most 1023, and the mass-storage array adds a fixed
    /// 128 KiB on top — `dev_base` is at most 4,390,912 B, or 4.19 MiB, swept
    /// over every demand. A controller cannot report a number that overflows
    /// this, whatever it says.
    fn new(max_scratchpad: usize, max_slots: u8) -> Self {
        let scratch_array = SHARED_SIZE;
        let array_bytes = (max_scratchpad * 8 + PAGE - 1) & !(PAGE - 1);
        let scratch_buffers = scratch_array + array_bytes;
        // Ahead of the device array rather than behind it, so the device array
        // still absorbs all of the pool's slack and MaxSlotsEn is unchanged by
        // storage existing. The alignment is what makes each block's data
        // buffer stay inside one 64 KiB region.
        let msc_base =
            (scratch_buffers + max_scratchpad * PAGE + MSC_STRIDE - 1) & !(MSC_STRIDE - 1);
        let dev_base = msc_base + MSC_BLOCKS * MSC_STRIDE;

        // DmaPool hands out whole 2 MiB pages. The head above already forces
        // one, so every block that fits in its slack is free — and asking for
        // more than the slack buys a second 2 MiB page for devices no root hub
        // has ports for. The floor is what decides how many pages to take; the
        // slack of those pages is what decides how many blocks to carve.
        let pool_size = crate::mm::align_2m(dev_base + MIN_DEVICE_BLOCKS * DEV_STRIDE);
        let dev_blocks = ((pool_size - dev_base) / DEV_STRIDE)
            .min(max_slots as usize)
            .min(device_ceiling());

        Self {
            scratch_array,
            scratch_buffers,
            scratch_count: max_scratchpad,
            msc_base,
            dev_base,
            dev_blocks,
            pool_size,
        }
    }

    /// The block belonging to a 1-based slot id, or `None` when the controller
    /// handed back a slot the pool has no room for.
    fn device(&self, slot_id: u8) -> Option<usize> {
        let index = (slot_id as usize).checked_sub(1)?;
        (index < self.dev_blocks).then(|| self.dev_base + index * DEV_STRIDE)
    }

    /// The `index`-th mass-storage block.
    ///
    /// Private to [`XhciController::claim_msc_block`], which is the only caller
    /// and the only thing that decides what `index` is. That is deliberate:
    /// `bind` used to pass `storage.len()`, and `storage.len()` goes back down.
    fn msc(&self, index: usize) -> usize {
        self.msc_base + index * MSC_STRIDE
    }
}

/// One mass-storage pool block, and whatever is holding it.
///
/// One array and not two. "This block is spoken for" and "there is a disk
/// behind it" were separate state, and a device refused between Configure
/// Endpoint and READ CAPACITY sits in the gap: the block was taken, and
/// nothing anywhere named the port it was taken for. The teardown that gives
/// blocks back walked the disks, so a refused stick that was then *unplugged*
/// kept its block for the life of the boot — two of those and the pool is out
/// for good, boot stick included, on a machine whose only diagnostic channel
/// is the `/log` it can then no longer mount.
#[derive(Clone, Copy)]
struct MscBlock {
    /// The root-hub port whose device claimed this block, and `None` while it
    /// is free. Claimed before Configure Endpoint puts the device's two bulk
    /// endpoints into Running with their transfer rings inside this memory,
    /// and given back only by the teardown that disabled the slot naming it.
    port: Option<u8>,
    /// The disk, once `bring_up` produced one. A block whose device was
    /// refused after its endpoints were configured keeps this `None` and stays
    /// claimed, which is the whole reason `port` is the thing that says taken.
    disk: Option<Disk>,
}

impl MscBlock {
    const FREE: Self = Self { port: None, disk: None };
}

/// A disk this controller brought up, and the number the machine knows it by.
///
/// The number lives beside the device rather than inside it because it exists
/// exactly when the disk does: a device still being interrogated has not been
/// given one, and a field that had to hold something in the meantime would be
/// a sentinel.
#[derive(Clone, Copy)]
struct Disk {
    index: usize,
    dev: msc::MscDevice,
}

/// How many disks this machine has bound since it booted, and so the number
/// the next bind hands out.
///
/// A counter and not a position. The number a disk is bound under is what
/// `usb_storage::open` indexes by and what a mount holds for its whole life,
/// so it has to be a fact about *that disk* — and a position in any list is a
/// fact about every other disk's history instead. Summing `storage.len()`
/// across controllers made a stick plugged into the T14's Thunderbolt xHC
/// renumber the PCH's boot stick underneath the mount holding it: `/log`
/// appended into the middle of the new drive and `/boot` served its bytes as
/// the ESP's.
///
/// Never reused, for the reason the numbers are stable at all: a replugged
/// stick that took its predecessor's number would be read through the handle
/// the predecessor's mount is still holding.
static DISKS_BOUND: AtomicUsize = AtomicUsize::new(0);

fn setup_packet(bm_request_type: u8, b_request: u8, w_value: u16, w_index: u16, w_length: u16) -> u64 {
    (bm_request_type as u64)
        | ((b_request as u64) << 8)
        | ((w_value as u64) << 16)
        | ((w_index as u64) << 32)
        | ((w_length as u64) << 48)
}

// SAFETY: XhciController contains raw pointers to DMA memory that is valid
// for the lifetime of the controller. Access is serialized by the Lock.
unsafe impl Send for XhciController {}

pub struct XhciController {
    /// The function this controller is, so every line about it after `init_one`
    /// has returned can still name which of the machine's controllers it means.
    pci: PciDevice,

    op_base: Mmio,
    db_base: Mmio,
    rt_base: Mmio,

    /// HCSPARAMS1's MaxPorts: every port register this controller has, both
    /// speed-specific views of a paired receptacle included.
    max_ports: u8,

    /// When this controller's root-hub ports were powered, which is the last
    /// instant their connect state is known to have changed and therefore where
    /// [`PORT_DEBOUNCE_NS`] is measured from. Kept per controller so the
    /// debounces of a machine with two overlap instead of adding up.
    powered_at: u64,

    context_size: usize, // 32 or 64
    layout: Layout,

    /// This controller's DMA, and this controller's only. It used to be one
    /// static for the driver, which was sound only while the machine had one
    /// controller: every offset in `Layout` is relative to a pool base, so two
    /// controllers sharing one pool put both their DCBAAs, both their command
    /// rings and both their slot 1 device contexts at the same address.
    pool: DmaPool,

    cmd_ring: TrbRing,

    event_ring: *const Trb,
    event_head: u16,
    event_phase: bool,

    devices: Vec<HidDevice>,

    /// This controller's mass-storage pool blocks and their disks.
    ///
    /// A block is claimed before Configure Endpoint — which puts the device's
    /// two bulk endpoints into the Running state with their transfer rings
    /// inside it — and only *then* is the disk asked what it is. Keying the
    /// block off a count of *bound* disks handed the next disk a block whose
    /// memory a live endpoint context still named, with whatever transfer
    /// `wait_transfer` abandoned on its 2 s deadline still outstanding on it;
    /// that completion lands in the next disk's `MSC_SCRATCH`, which is where
    /// READ CAPACITY's block size and last LBA arrive.
    ///
    /// So a *refused* disk keeps its block for as long as it is on the bus.
    /// **Unplugging is what gives one back**, whether or not it ever became a
    /// disk, and only after `teardown_port` has disabled the slot: a disabled
    /// slot is one whose endpoint contexts no longer name that memory and whose
    /// outstanding TRBs the controller has already abandoned.
    msc: [MscBlock; MSC_BLOCKS],

    /// This controller's root-hub ports, one entry per port register. Sized
    /// from HCSPARAMS1 rather than fixed: `max_ports` is a byte, and a fixed
    /// array would be 255 entries on a controller with five.
    ports: Vec<PortState>,

    /// What each port register speaks, out of the controller.s own Supported
    /// Protocol capabilities. The boot scan reads it directly; the hot-plug
    /// machine was given its port.s copy at bring-up.
    protocols: Protocols,

    /// A Port Status Change Event arrived and the ports have not been read
    /// since. Set where the event is dequeued — which includes the middle of
    /// somebody else's enumeration, since `wait_transfer` drains the whole ring
    /// — and consumed by [`Self::poll`].
    ports_dirty: bool,

    /// The one operation this controller has been given and has not answered.
    ///
    /// **`poll_if_pending` runs at the top of every scheduler pass**, so what
    /// starts there is submitted and left: the completion arrives through the
    /// event ring the poll already drains, and a later pass acts on it. The two
    /// paths this covers — a teardown's Disable Slot and a HID endpoint's
    /// recovery — are exactly what pulling a device out of a running machine
    /// runs, and each used to spin to [`USB_TIMEOUT_NS`] against a device that
    /// by then had nothing to answer with.
    ///
    /// The boot path keeps its waits, because blocking is correct where there
    /// is no scheduler to give a pass back to.
    outstanding: Outstanding<What>,

    /// Ports this driver has written PED=1 to, which on a real controller are
    /// Disabled and read PED clear until they are reset again.
    ///
    /// Kernel feature because nothing on the host side can stage it. QEMU's
    /// `xhci_port_write` clears only `CSC|PEC|WRC|OCC|PRC|PLC|CEC` on a written
    /// '1', and PED is in neither that set nor its read/write set, so a write of
    /// PED=1 is a no-op there (`hw/usb/hcd-xhci.c`). On a real controller it
    /// disables the port. No device or machine property changes that, and no
    /// sequence of register writes reaches a PED=0/CCS=1 port on QEMU either:
    /// clearing PP is the closest and leaves PP=0, which is a different register
    /// state and a different diagnosis.
    ///
    /// What this replaces is the *register*, not a verdict — after the write the
    /// port reads PED clear for every reader, which is the state the T14 showed
    /// on all five of its ports — and only a reset clears it, because a reset is
    /// the one thing that takes a real port out of Disabled (§4.19.1.1.3). Same
    /// reason `xhci-slow-connect` and `xhci-deaf-port` exist.
    software_disabled: PortMask,

    /// The event ring slot a slow device's completion is being held in, and
    /// when it was first seen there. See [`SLOW_TRANSFER_NS`].
    held_event: Option<(u16, u64)>,
}

impl XhciController {
    pub(super) fn dma(&self) -> KernelSlice {
        self.pool.slice()
    }

    /// This controller's `slot_id`, as a [`Slot`] a log line can name a device
    /// by. Never construct one of these from a bare slot id: the controller is
    /// half the identity.
    fn slot(&self, id: u8) -> Slot {
        Slot { bus: self.pci.bus, dev: self.pci.dev, func: self.pci.func, id }
    }

    /// Every read of a port register in this driver, so that what the connect
    /// settle sees and what `init_device` acts on cannot disagree.
    fn read_portsc(&self, port_idx: u8) -> Portsc {
        Portsc::from_raw(self.read_portsc_raw(port_idx))
    }

    fn read_portsc_raw(&self, port_idx: u8) -> u32 {
        let raw = self.op_base.read_u32(OP_PORT_BASE + port_idx as u64 * PORT_REG_SIZE);
        if crate::actuator::xhci_slow_connect() && crate::clock::nanos_since_boot() < SLOW_CONNECT_NS
        {
            return raw & !(PORTSC_CCS | PORTSC_PED | PORTSC_SPEED);
        }
        if crate::actuator::xhci_slow_storage_connect()
            && port_idx == SLOW_STORAGE_PORT
            && !BOOT_SCAN_DONE.load(core::sync::atomic::Ordering::Relaxed)
        {
            return raw & !(PORTSC_CCS | PORTSC_PED | PORTSC_SPEED);
        }
        if crate::actuator::xhci_portsc_rw1c() && port_bit(&self.software_disabled, port_idx) {
            return raw & !PORTSC_PED;
        }
        // A port that never finishes a reset does not read Enabled either — and
        // on QEMU a SuperSpeed port reads Enabled the instant the register is
        // touched. Without this the deaf port is one the driver correctly
        // declines to reset, so the actuator stages nothing at all.
        if crate::actuator::xhci_deaf_port() {
            return raw & !PORTSC_PED;
        }
        raw
    }

    /// Every write of a port register, so the emulation below sees all of them.
    ///
    /// It takes a [`toyos_xhci::portsc::Write`] and not a word: a value of that
    /// type can only be built from a neutral base and offers no way to set PED,
    /// so the two writes that disable a port the driver was enabling are
    /// unreachable rather than asserted against.
    fn write_portsc(&mut self, port_idx: u8, write: toyos_xhci::portsc::Write) {
        let value = write.raw();
        if crate::actuator::xhci_portsc_rw1c() {
            let word = port_idx as usize / 64;
            let bit = 1u64 << (port_idx % 64);
            if value & PORTSC_PED != 0 {
                self.software_disabled[word] |= bit;
            }
            if value & PORTSC_PR != 0 {
                self.software_disabled[word] &= !bit;
            }
        }
        self.op_base.write_u32(OP_PORT_BASE + port_idx as u64 * PORT_REG_SIZE, value);
    }

    /// How many of this controller's ports the driver has disabled by writing
    /// PED=1. Zero on a driver that neutralises PORTSC before writing it, and
    /// the reason the gate can tell "the emulation is compiled in and saw
    /// nothing" from "the emulation is not compiled in".
    fn software_disabled_ports(&self) -> u32 {
        self.software_disabled.iter().map(|w| w.count_ones()).sum()
    }

    /// The root-hub port a slot's device is on, or `None` for a slot no port
    /// has been recorded against yet — which is every slot mid-enumeration,
    /// since `device::finish` is what gives a port its slot. The one caller
    /// that reads this is `wait_transfer`, deciding whether a port that has
    /// gone means the transfer cannot be answered; a disk still inside its
    /// bring-up therefore does not get that shortcut and spends the budget.
    fn port_of_slot(&self, slot: u8) -> Option<u8> {
        self.ports
            .iter()
            .position(|p| p.slot().map(NonZeroU8::get) == Some(slot))
            .map(|at| at as u8)
    }

    fn connected_ports(&self) -> PortMask {
        let mut mask = [0u64; 4];
        for p in 0..self.max_ports {
            if self.read_portsc(p).connected() {
                mask[p as usize / 64] |= 1 << (p % 64);
            }
        }
        mask
    }

    /// Take a free mass-storage pool block for the device on `port_idx`, as
    /// its index in [`Self::msc`] and its byte offset in the pool.
    ///
    /// The only way to obtain one, so there is no path that gets a block
    /// without recording who holds it — which is the property [`Self::msc`]
    /// exists for, and the one `ctrl.layout.msc(ctrl.storage.len())` did not
    /// have. `None` when the pool is out; nothing is spent then, because
    /// nothing was handed out.
    fn claim_msc_block(&mut self, port_idx: u8) -> Option<(usize, usize)> {
        let index = self.msc.iter().position(|block| block.port.is_none())?;
        self.msc[index].port = Some(port_idx);
        Some((index, self.layout.msc(index)))
    }

    /// How many blocks are spoken for, for the line that refuses the disk the
    /// pool has no room for.
    fn msc_blocks_taken(&self) -> usize {
        self.msc.iter().filter(|block| block.port.is_some()).count()
    }

    /// Put a command on the ring and ring the command doorbell, answering with
    /// the address the completion will name it by.
    fn submit_command(&mut self, trb: Trb) -> u64 {
        let at = self.cmd_ring.enqueue(trb);
        fence(Ordering::Release);
        self.db_base.write_u32(0, 0);
        at
    }

    /// One event, or `None` while the controller has not published the next.
    /// Every reader goes through here, because the ring is a single queue
    /// carrying command completions, the enumeration's own control transfers
    /// and every bound device's interrupt completions at once — so a reader
    /// that dequeues an event it did not ask for owes it to whoever did, which
    /// is what `dispatch_event` is.
    fn next_event(&mut self) -> Option<Trb> {
        let event = unsafe { read_volatile(self.event_ring.add(self.event_head as usize)) };
        if ((event.control & 1) != 0) != self.event_phase {
            return None;
        }
        if crate::actuator::usb_slow_device() && !self.slow_device_would_have_answered(&event) {
            return None;
        }
        self.advance_event_ring();
        Some(event)
    }

    /// Whether a stick as slow as the owner's would have answered this transfer
    /// yet. See [`SLOW_TRANSFER_NS`]; `true` for everything that is not a bulk
    /// completion of a bound disk, so the keyboard and the port machine are
    /// untouched.
    ///
    /// Keyed on the ring position, because that is what identifies *this*
    /// event: the head does not advance while an event is held, so a second
    /// look finds the same slot and the same first-seen time, and the entry is
    /// replaced rather than accumulated when the head moves on.
    fn slow_device_would_have_answered(&mut self, event: &Trb) -> bool {
        let slot = ((event.control >> 24) & 0xFF) as u8;
        let dci = ((event.control >> 16) & 0x1F) as u8;
        let is_disk_bulk = (event.control >> 10) & 0x3F == EVENT_TRANSFER
            && dci >= 2
            && self.msc.iter().any(|b| b.disk.is_some_and(|d| d.dev.slot_id() == slot));
        if !is_disk_bulk {
            return true;
        }
        let now = crate::clock::nanos_since_boot();
        let since = match self.held_event {
            Some((head, at)) if head == self.event_head => at,
            _ => {
                self.held_event = Some((self.event_head, now));
                now
            }
        };
        now.saturating_sub(since) >= SLOW_TRANSFER_NS
    }

    /// Give an event to the device it names. A bound device's interrupt
    /// endpoint carries exactly one queued TRB and `requeue` is the only thing
    /// that puts the next one there, so an interrupt completion dropped here —
    /// as one dequeued during a later port's enumeration used to be — leaves
    /// that device with an empty ring for the life of the boot: no log line, no
    /// fault, a keyboard that simply stops.
    ///
    /// **A completion code other than Success or Short Packet is the same
    /// defect wearing a different hat**, and it is the one a Logitech mouse
    /// hot-plugged into the T14 hit: every bind-time line read perfectly and
    /// the device delivered nothing for the 28 seconds it stayed in the port.
    /// So a code this driver did not expect is *recorded* here rather than
    /// dropped, and [`Self::recover_endpoints`] acts on it.
    fn dispatch_event(&mut self, event: Trb) {
        let trb_type = (event.control >> 10) & 0x3F;
        let code = (event.status >> 24) & 0xFF;
        let slot = ((event.control >> 24) & 0xFF) as u8;

        // **The outstanding operation first, and recorded rather than acted
        // on.** The Command TRB pointer's low four bits are reserved in the
        // event, so the address is masked out of it rather than compared whole.
        // The second number each event kind carries goes with it: a Command
        // Completion Event's Slot ID, which is the controller's answer to
        // Enable Slot, and a Transfer Event's residue.
        let answers = match trb_type {
            EVENT_CMD_COMPLETE => Some((Await::Command { trb: event.param & !0xF }, slot as u32)),
            EVENT_TRANSFER => Some((
                Await::Transfer { slot, dci: ((event.control >> 16) & 0x1F) as u8 },
                event.status & 0x00FF_FFFF,
            )),
            _ => None,
        };
        if answers.is_some_and(|(on, param)| self.outstanding.answered(on, code, param)) {
            return;
        }
        // The port id the event carries is not read. It is the *register* that
        // says what a port is, and the driver has to look at every port anyway
        // to tell a connect it has acted on from one it has not — so the event
        // is a reason to look, exactly as an `irq_ring` record is. Believing
        // the id instead would make the driver's picture of the bus depend on
        // an event never being missed.
        if trb_type == EVENT_PORT_STATUS_CHANGE {
            self.ports_dirty = true;
            return;
        }
        if trb_type != EVENT_TRANSFER {
            return;
        }
        // A device whose endpoint is mid-recovery has exactly one transfer
        // outstanding — the one that broke — and its ring is about to be
        // rebuilt under it. Requeueing on that ring puts a TRB where the
        // controller's dequeue pointer is not.
        if matches!(self.outstanding.what(), Some(What::Recovering { slot_id, .. }) if *slot_id == slot)
        {
            return;
        }
        let Some(at) = self.devices.iter().position(|d| d.slot_id == slot) else {
            return;
        };
        #[cfg(feature = "boot-actuators")]
        let code = self.devices[at].stage_break(code);
        let dev = &mut self.devices[at];
        if code == CC_SUCCESS || code == CC_SHORT_PACKET {
            dev.failures = 0;
            dev.dispatch_report();
            dev.requeue(&self.db_base);
            return;
        }
        dev.broke_with = Some(code);
    }

    /// Start the recovery of one bound HID interrupt endpoint a completion code
    /// broke, if one is owed and the controller is not already answering
    /// something else.
    ///
    /// **Separate from the code that reads the code, and that is the whole
    /// reason it exists.** `dispatch_event` runs inside `wait_command` and
    /// `wait_transfer`, which are draining this same event ring on behalf of a
    /// caller waiting for one particular event. A recovery issued from there
    /// submits commands whose completions that caller would consume — a disk's
    /// data phase disappearing because a mouse stalled.
    ///
    /// One at a time, and never more: [`Self::outstanding`] is one slot, and a
    /// second device's recovery is owed until the first is answered. That is
    /// the serialization the submit-and-wait pairs this replaces had by
    /// construction.
    fn recover_endpoints(&mut self) {
        if self.outstanding.busy() {
            return;
        }
        let Some((slot_id, code)) =
            self.devices.iter().find_map(|d| Some((d.slot_id, d.broke_with?)))
        else {
            return;
        };
        self.recover_hid(slot_id, code);
    }

    /// One HID device's interrupt endpoint, on its way back to delivering — or
    /// off the bus.
    fn recover_hid(&mut self, slot_id: u8, code: u32) {
        let Some(at) = self.devices.iter().position(|d| d.slot_id == slot_id) else {
            return;
        };
        // The device stays on the list. It used to be taken off for the
        // duration, because the recovery drained the event ring and a device
        // let go from inside that would move every index after its own; the
        // recovery no longer drains anything, and a device that is not on the
        // list is one a teardown of its port cannot find.
        let dev = &mut self.devices[at];
        dev.broke_with = None;
        let kind = dev.kind();
        let (ep_addr, dci, port_idx, block) =
            (dev.ep_addr, dev.int_ep_dci, dev.port_idx, dev.block);

        // **When the disconnect and the transfer error race, the disconnect
        // wins.** A transfer outstanding on an endpoint whose device is pulled
        // completes with a transaction error, and that code is the same one a
        // device with a bad cable gives — the completion cannot tell them
        // apart, and the port register is the only thing that can. Everything
        // below is aimed at a device that is still on the bus: it spends a
        // failure out of the budget, issues Reset Endpoint and a
        // CLEAR_FEATURE(HALT) control transfer against a device the owner is
        // holding in their hand, and then tells them to unplug it. The T14 did
        // all of that four times over, once per ordinary unplug.
        //
        // CSC as well as CCS, for the reason `service_port` reads it: a device
        // replugged between two looks reads connected again, and the transfer
        // that died still died with the old one. Read and not cleared —
        // acknowledging it is `service_port`'s job, and clearing it here would
        // steal the evidence that runs the teardown.
        let slot = self.slot(slot_id);
        let portsc = self.read_portsc(port_idx);
        if !portsc.connected() || portsc.connect_changed() {
            log!("xHCI: USB {kind} on {slot}: interrupt endpoint {ep_addr:#04x} \
                 completed with {} as its port went away; leaving it to the disconnect",
                Completion(code));
            return;
        }

        let dev = &mut self.devices[at];
        dev.failures += 1;
        let failures = dev.failures;
        log!("xHCI: USB {kind} on {slot}: interrupt endpoint {ep_addr:#04x} (dci {dci}) \
             completed with {}; failure {failures} of {MAX_HID_FAILURES}",
            Completion(code));

        if failures >= MAX_HID_FAILURES {
            self.let_go(at, format_args!("it has failed {MAX_HID_FAILURES} transfers in a row"));
            return;
        }
        let state = self.endpoint_state(block, dci);
        log!("xHCI: {slot} endpoint {dci} is {state}, recovering");
        match Recovery::begin(state) {
            Ok((seq, act)) => self.step_recovery(slot_id, seq, act),
            Err(NeedsConfigure(state)) => {
                log_unrecoverable(slot, dci, state);
                self.let_go(at, format_args!("endpoint {ep_addr:#04x} could not be restarted"));
            }
        }
    }

    /// Perform one act of a HID endpoint's recovery and record what ends it.
    ///
    /// Nothing here waits: the completion arrives through the event ring the
    /// poll already drains, and [`Self::advance_outstanding`] asks the sequence
    /// what is owed next.
    fn step_recovery(&mut self, slot_id: u8, seq: Recovery, act: Act) {
        let Some(at) = self.devices.iter().position(|d| d.slot_id == slot_id) else {
            return;
        };
        let slot = self.slot(slot_id);
        let dev = &mut self.devices[at];
        let (dci, ep_addr, ring_at) = (dev.int_ep_dci, dev.ep_addr, dev.block + DEV_INT_RING);
        match act {
            Act::Running => {
                dev.requeue(&self.db_base);
                log!("xHCI: {slot} endpoint {dci} is delivering again");
            }
            Act::Command(cmd) => {
                // Copied out and written back rather than borrowed across the
                // call: `recovery_trb` reads the pool through `self` and this
                // is the whole of the window, with nothing in it that could
                // re-enter and find a ring that is neither the old one nor the
                // new one.
                let mut ring = dev.int_ring;
                let trb = self.recovery_trb(cmd, slot_id, dci, &mut ring, ring_at);
                self.devices[at].int_ring = ring;
                let on = Await::Command { trb: self.submit_command(trb) };
                let what = What::Recovering { slot_id, seq, issued: cmd.name() };
                self.outstanding.submit(what, on, Stages::One, deadline());
            }
            Act::ClearHalt => {
                enqueue_control(
                    &mut self.devices[at].ep0_ring, 0x02, 0x01, 0, ep_addr as u16, None, 0,
                );
                self.ring_doorbell(slot_id, 1);
                let on = Await::Transfer { slot: slot_id, dci: 1 };
                let what =
                    What::Recovering { slot_id, seq, issued: "CLEAR_FEATURE(ENDPOINT_HALT)" };
                self.outstanding.submit(what, on, Stages::One, deadline());
            }
        }
    }

    /// The controller answered a recovery step. Ask the sequence what is owed
    /// next, or let the device go.
    fn recovery_stepped(
        &mut self,
        slot_id: u8,
        mut seq: Recovery,
        issued: &str,
        outcome: Outcome,
    ) {
        if outcome.succeeded() {
            let act = seq.completed();
            self.step_recovery(slot_id, seq, act);
            return;
        }
        log!("xHCI: {}: {issued} failed: {}", self.slot(slot_id), Answer(outcome));
        if let Some(at) = self.devices.iter().position(|d| d.slot_id == slot_id) {
            self.let_go(at, format_args!("its interrupt endpoint could not be restarted"));
        }
    }

    /// Drop a recovery outstanding for a device on a port whose device has
    /// gone, because **a transfer error on a port that has gone belongs to the
    /// disconnect**. The command it is waiting for will not be answered by
    /// anything still on the bus, and the teardown behind it would spend the
    /// whole deadline finding that out. A completion that arrives afterwards is
    /// an event addressed to nobody, which is what every abandoned wait in this
    /// driver already produced.
    fn cancel_recovery_on(&mut self, port_idx: u8) {
        let Some(What::Recovering { slot_id, .. }) = self.outstanding.what() else {
            return;
        };
        let slot_id = *slot_id;
        if !self.devices.iter().any(|d| d.slot_id == slot_id && d.port_idx == port_idx) {
            return;
        }
        self.outstanding.cancel();
        log!("xHCI: {}'s endpoint recovery is abandoned; its port has gone", self.slot(slot_id));
    }

    /// Everything a HID device the driver has given up on leaves behind.
    ///
    /// The order [`Self::teardown_port`] uses and for its reasons — input
    /// first, so a keyboard's held keys and a pointer's button-table entry go
    /// back before anything else, then the slot — with one deliberate
    /// difference: **the port stays marked attached**. A port whose `attached`
    /// went false with the device still physically in it reads as a fresh
    /// connect on the next pass, and the driver would enumerate the same
    /// endpoint again every debounce for as long as it stayed plugged in.
    /// Unplugging is what clears it, which is what the line says to do.
    ///
    /// The port's slot is this device's: one root-hub port carries one device
    /// here, and `parse_config` gives that device one function.
    fn let_go(&mut self, at: usize, why: core::fmt::Arguments) {
        let mut dev = self.devices.remove(at);
        log!("xHCI: USB {} on {} is being let go — {why}. Unplug it and plug it in again.",
            dev.kind(), self.slot(dev.slot_id));
        dev.unbind();
        if let Some(slot) = self.ports[dev.port_idx as usize].take_slot() {
            self.submit_disable_slot(slot.get(), AfterSlot::LetGo);
        }
    }

    /// The command `cmd` names against (`slot_id`, `dci`), with the ring
    /// rebuilt where the command is the one that hands the controller a fresh
    /// dequeue pointer.
    ///
    /// The two have to happen together or they disagree: the TRBs behind the
    /// transfer that broke belong to nobody, and Set TR Dequeue is the only
    /// thing that tells the controller so.
    fn recovery_trb(
        &self,
        cmd: recovery::Command,
        slot_id: u8,
        dci: u8,
        ring: &mut TrbRing,
        ring_at: usize,
    ) -> Trb {
        let mut trb = Trb::ZERO;
        let kind = match cmd {
            recovery::Command::ResetEndpoint => TRB_RESET_ENDPOINT,
            recovery::Command::StopEndpoint => TRB_STOP_ENDPOINT,
            recovery::Command::SetDequeue => {
                *ring = TrbRing::init(self.dma().subslice(ring_at, PAGE));
                trb.param = ring.dequeue();
                TRB_SET_TR_DEQUEUE
            }
        };
        trb.control = kind | ((slot_id as u32) << 24) | ((dci as u32) << 16);
        trb
    }

    /// Clear whatever change flags one port is holding, so the next change is
    /// one the controller can report.
    ///
    /// Takes the value the caller has already read, rather than reading its
    /// own: a flag raised between the two reads would be cleared without ever
    /// having been looked at, and the machine decides what a port means from
    /// exactly the word this clears.
    fn acknowledge_port_change(&mut self, port_idx: u8, portsc: Portsc) {
        if portsc.any_change() {
            self.write_portsc(port_idx, portsc.neutral().acknowledging(portsc));
        }
    }

    /// Read a port and clear whatever change flags it is holding, for the
    /// callers that have no reason to look at them.
    fn acknowledge_port_read(&mut self, port_idx: u8) {
        let portsc = self.read_portsc(port_idx);
        self.acknowledge_port_change(port_idx, portsc);
    }

    /// The same for every port this controller has.
    fn acknowledge_port_changes(&mut self) {
        for p in 0..self.max_ports {
            self.acknowledge_port_read(p);
        }
    }

    /// Record what the boot scan's enumeration left behind, so the hot-plug
    /// machine starts from what that path already did. The slot is what
    /// [`Self::teardown_port`] gives back, and it is recorded even when no
    /// device came out the far end: an Enable Slot that succeeded is the
    /// controller's resource whatever happened after it.
    fn port_bound(&mut self, port_idx: u8, slot: Option<u8>) {
        self.ports[port_idx as usize].adopt(slot.and_then(NonZeroU8::new));
    }

    /// Step every port that is not where the driver left it, and say when it
    /// wants to be looked at again.
    ///
    /// One step per call and no wait anywhere in it — see [`PortWork`]. The
    /// enumeration it eventually runs *is* blocking, and it is the same
    /// `wait::boot::configure` the boot path runs; what this removes from the
    /// blocking part is the debounce and the port reset, which on the T14 are
    /// 100 ms and 55 ms against roughly 14 ms for everything else.
    fn service_ports(&mut self) -> Option<u64> {
        let now = crate::clock::nanos_since_boot();
        (0..self.max_ports)
            .filter_map(|p| self.service_port(p, now))
            .min()
    }

    /// One port's step, and when it next wants one.
    ///
    /// **The decision is [`PortState::step`]'s and every line here is an
    /// effect.** The loop is what the machine's contract asks for: do the one
    /// thing it said, read the register again, ask again — because an effect
    /// changes the register, and a decision taken from a word that predates the
    /// last write is a decision about a port that no longer exists.
    ///
    /// The bound is not a timeout. The machine issues one effect per state it
    /// leaves and the longest legitimate run is teardown, acknowledge,
    /// debounce; exceeding it means looping, and looping here is a scheduler
    /// pass that never ends.
    fn service_port(&mut self, port_idx: u8, now: u64) -> Option<u64> {
        const MAX_EFFECTS: usize = 16;
        for _ in 0..MAX_EFFECTS {
            let portsc = self.read_portsc(port_idx);
            // CCS *or* CSC, for the reason the machine reads both: a device
            // replugged between two looks reads connected again and the one
            // that was here has still gone.
            if !portsc.connected() || portsc.connect_changed() {
                self.cancel_recovery_on(port_idx);
                device::cancel_on(self, port_idx);
            }
            // A port inside an effect a previous pass began — a teardown
            // waiting on Disable Slot — is not decided about at all until the
            // controller has answered for it. The machine says so itself, and
            // asking it costs a register read to be told nothing.
            //
            // The `expect` is a driver bug and not a device one: the only
            // effect that outlives a pass is the one that filled the slot, so
            // a port left working with nothing outstanding is a port no pass
            // will ever come back for — #151's shape, and silent.
            if self.ports[port_idx as usize].working().is_some() {
                let at = self.outstanding.wake_at().expect(
                    "a port is inside an effect the controller was never asked to perform",
                );
                return Some(at);
            }
            // Read before the machine is asked, because by then its own borrow
            // of this port is live. The two effects below that need the
            // controller's answer to the *last* thing it was given — a
            // teardown's Disable Slot and an enumeration's Enable Slot — defer
            // on it; a register write, an acknowledge and a reset do not.
            let busy = self.outstanding.wake_at();
            match self.ports[port_idx as usize].step(portsc, now) {
                Step::Idle => return None,
                Step::Wait(at) => return Some(at),
                Step::GaveUp(why) => {
                    match why {
                        GaveUp::ResetNeverFinished(kind) => log!(
                            "xHCI: port {} never finished its {} reset (PORTSC {:#010x}); \
                             skipping it",
                            port_idx + 1,
                            match kind {
                                Reset::Hot => "hot",
                                Reset::Warm => "warm",
                            },
                            portsc.raw()
                        ),
                        // A USB3 link that would not train even warm. §4.19.1.2
                        // has nothing further, so this is the port's end and
                        // not one step short of it.
                        GaveUp::LinkNeverTrained => log!(
                            "xHCI: port {} is SuperSpeed and its link would not train, warm reset \
                             included (PORTSC {:#010x}, link {:?}); skipping it",
                            port_idx + 1,
                            portsc.raw(),
                            portsc.link_state()
                        ),
                    }
                    return None;
                }
                Step::Write(write) => self.write_portsc(port_idx, write),
                Step::Reset(kind, write) => {
                    match kind {
                        Reset::Hot => log!("xHCI: port {} connected", port_idx + 1),
                        // The line the T14 could not produce, because the
                        // driver had no such command.
                        Reset::Warm => log!(
                            "xHCI: port {} warm reset, link was {:?}",
                            port_idx + 1,
                            portsc.link_state()
                        ),
                    }
                    self.write_portsc(port_idx, write);
                }
                Step::Teardown(why, pending) => {
                    if busy.is_some() {
                        return busy;
                    }
                    pending.running();
                    match why {
                        Gone::Disconnected => log!("xHCI: port {} disconnected", port_idx + 1),
                        Gone::Replugged => log!(
                            "xHCI: port {} was unplugged and plugged back in between two looks; \
                             tearing the old device down before enumerating what is there now",
                            port_idx + 1
                        ),
                    }
                    if self.teardown_port(port_idx) {
                        self.ports[port_idx as usize].torn_down();
                    } else {
                        // The slot is outstanding, so this port is inside an
                        // effect until the controller answers for it.
                        return self.outstanding.wake_at();
                    }
                }
                Step::Enumerate { trained, pending } => {
                    // A slot the controller has been asked to disable is one it
                    // may hand straight back to the Enable Slot below, and this
                    // driver would then zero the DCBAA entry the new device's
                    // context sits in.
                    if busy.is_some() {
                        return busy;
                    }
                    pending.running();
                    if trained {
                        // No reset was issued and none was needed: a SuperSpeed
                        // link trains itself and this port was already Enabled.
                        // The driver that did not know that reset the port into
                        // Inactive and then had no way back.
                        log!("xHCI: port {} connected, link already trained", port_idx + 1);
                    }
                    device::begin(self, port_idx);
                    // Either the enumeration is under way and the port is
                    // inside an effect until it answers, or it refused before
                    // spending a command and the port is already reported.
                    return self.outstanding.wake_at();
                }
            }
        }
        log!("xHCI: port {} produced {MAX_EFFECTS} effects without settling; leaving it",
            port_idx + 1);
        None
    }

    /// Everything a device that is no longer on the bus leaves behind, in the
    /// order the pieces stop being reachable.
    ///
    /// **Input first**, because a keyboard yanked mid-chord holds its keys in
    /// the machine-wide held set and a pointer holds its button in the merge,
    /// and both are published by every *other* device from then on. **Then the
    /// slot**, which is what takes the device's endpoints out of Running and
    /// abandons whatever TRB was queued on them. **Then the pool block**, which
    /// is only safe in that order: while the slot lives, its endpoint contexts
    /// still name that memory.
    /// **`true` when the port is already empty**, and `false` when the
    /// controller still has to answer for the slot — in which case
    /// [`Self::slot_gone`] finishes it, and until then the port is inside an
    /// effect and nothing decides anything else about it.
    fn teardown_port(&mut self, port_idx: u8) -> bool {
        while let Some(at) = self.devices.iter().position(|d| d.port_idx == port_idx) {
            let mut dev = self.devices.remove(at);
            let role = dev.role;
            dev.unbind();
            match role {
                hid::HidRole::Keyboard => log!(
                    "xHCI: USB keyboard on slot {} unplugged from port {}",
                    dev.slot_id, port_idx + 1
                ),
                // The source is in the line for the reason it is in the bind
                // line: it is the only place the button merge is visible, and
                // an entry released and an entry leaked read the same from
                // every other angle until the machine runs out of them.
                hid::HidRole::Pointer(source) => log!(
                    "xHCI: USB pointer on slot {} unplugged from port {}, source {} released",
                    dev.slot_id, port_idx + 1, source.id()
                ),
            }
        }
        let Some(slot) = self.ports[port_idx as usize].take_slot() else {
            self.release_blocks(port_idx);
            return true;
        };
        self.submit_disable_slot(slot.get(), AfterSlot::Teardown(port_idx));
        false
    }

    /// The pool blocks a port's device held, back in the pool.
    ///
    /// **After the slot and never before it**: while the slot lives, its
    /// endpoint contexts still name this memory. Every block this port claimed
    /// and not only the ones a disk came out of — `bind` claims before
    /// Configure Endpoint, so a device refused after that point holds one with
    /// no disk behind it, and the pool holds [`MSC_BLOCKS`] of them.
    fn release_blocks(&mut self, port_idx: u8) {
        for at in 0..MSC_BLOCKS {
            if self.msc[at].port != Some(port_idx) {
                continue;
            }
            // The disk goes and its number does not come back: everything above
            // here holds that number — a mount holds it for its whole life —
            // and what it now names is a disk that is not there, which every
            // caller already has an answer for.
            match core::mem::replace(&mut self.msc[at], MscBlock::FREE).disk {
                Some(disk) => log!("usb-storage: disk {} unplugged from port {}; it is offline",
                    disk.index, port_idx + 1),
                None => log!("usb-storage: the device this driver refused on port {} is gone; \
                    its pool block is free again", port_idx + 1),
            }
        }
    }

    /// Ask the controller for a slot back, and record what its answer is owed.
    ///
    /// The one command that takes a slot out of any state (xHCI 1.2 §4.6.4), so
    /// there is no state a device that has been pulled can be in that makes
    /// this the wrong one — which is exactly what is not true of Reset
    /// Endpoint, and why `restart_endpoint` reads the endpoint state first.
    fn submit_disable_slot(&mut self, slot_id: u8, then: AfterSlot) {
        let mut disable = Trb::ZERO;
        disable.control = TRB_DISABLE_SLOT | ((slot_id as u32) << 24);
        let on = Await::Command { trb: self.submit_command(disable) };
        self.outstanding.submit(What::SlotGone { slot: slot_id, then }, on, Stages::One, deadline());
    }

    /// The slot is the controller's again, or it is not and this driver has no
    /// second question to ask about it.
    fn slot_gone(&mut self, slot: u8, then: AfterSlot, outcome: Outcome) {
        if outcome.succeeded() {
            // After the command, never before: until it completes the
            // controller may still be writing this device's output context.
            unsafe {
                let dcbaa = self.dma().ptr_at(OFF_DCBAA) as *mut u64;
                write_volatile(dcbaa.add(slot as usize), 0);
            }
            log!("xHCI: slot {slot} disabled");
        } else {
            log!("xHCI: Disable Slot failed: {}", Answer(outcome));
        }
        // The blocks go back whatever the controller said, because the
        // alternative is a port whose device has left holding one for the life
        // of the boot — and two of those is a machine with no disks at all,
        // boot stick included. A controller that will not disable a slot is
        // already past what this driver can repair.
        if let AfterSlot::Teardown(port_idx) = then {
            self.release_blocks(port_idx);
            self.ports[port_idx as usize].torn_down();
        }
    }

    /// Act on whatever the controller has answered, and issue whatever that
    /// answer owes next.
    ///
    /// **Never from inside a wait.** The drain that records an answer runs on
    /// behalf of a caller after one particular event, and everything below
    /// submits commands and frees memory.
    ///
    /// The loop ends because each turn either leaves the slot empty or fills it
    /// with an operation that has no answer yet and a deadline in the future.
    fn advance_outstanding(&mut self) {
        let now = crate::clock::nanos_since_boot();
        while let Some((what, outcome)) = self.outstanding.finished(now) {
            match what {
                What::SlotGone { slot, then } => self.slot_gone(slot, then, outcome),
                What::Recovering { slot_id, seq, issued } => {
                    self.recovery_stepped(slot_id, seq, issued, outcome)
                }
                What::SlotWanted { port_idx, speed, packet, seq } => {
                    device::slot_answered(self, port_idx, speed, packet, seq, outcome)
                }
                What::Enumerating(state) => device::stepped(self, state, outcome),
            }
        }
    }

    /// Drain the event ring and step the ports, and say when this controller
    /// wants to be polled again.
    ///
    /// The ports are read only when something says they might have moved: a
    /// Port Status Change Event since the last look, or a port the driver has
    /// not finished acting on. Otherwise this is one read of the event ring's
    /// next TRB, which is what every pass on every CPU pays.
    fn poll(&mut self) -> Option<u64> {
        while let Some(event) = self.next_event() {
            self.dispatch_event(event);
        }
        // After the drain and not inside it: an answer the drain recorded owes
        // commands and frees memory, and it is issued where nobody else is
        // waiting on this ring.
        self.advance_outstanding();
        self.recover_endpoints();

        // Nothing below reads the event ring, which is what makes one advance
        // enough: every step `service_ports` takes is a submit, so no answer can
        // arrive inside it and none can be left behind it. The last thing that
        // drained on its own behalf was the enumeration, and the only one left
        // is the disk bring-up behind `msc::bind` — which runs above, so a port
        // change it consumed is already in `ports_dirty` when this reads it.
        let mut wake_at = None;
        if self.ports_dirty || self.ports.iter().any(PortState::outstanding) {
            self.ports_dirty = false;
            wake_at = self.service_ports();
        }
        earliest(wake_at, self.outstanding.wake_at())
    }

    /// One dword of one *device context*, in the input context `ctx_base`
    /// points at: index 0 is the input control context, 1 the slot context,
    /// and `dci + 1` an endpoint's. The old name for this parameter was
    /// `slot_index`, which named the one thing no caller ever passes.
    ///
    /// No bound, and it needs none: `Endpoint::dci` is 2..=31 by construction
    /// and its field is private, so the largest index any of the 23 call sites
    /// can reach is 32, and `32 * 64 + 4 * 4` is 2064 bytes into the 4096 the
    /// input context is. That sentence is what `Endpoint`'s private field is
    /// for; before it, a struct literal under `xhci` could put this write
    /// 12,880 bytes in.
    fn write_ctx32(&self, ctx_base: *mut u8, ctx_index: usize, dword: usize, val: u32) {
        let offset = (ctx_index * self.context_size) + (dword * 4);
        unsafe { write_volatile(ctx_base.add(offset) as *mut u32, val); }
    }

    /// The Endpoint State the controller published for (`dev_block`'s device,
    /// `dci`).
    ///
    /// The output device context is DMA the controller owns, so this is a
    /// volatile read of its dword 0. Endpoint contexts are indexed by DCI there
    /// — unlike the *input* context, where the Input Control Context shifts
    /// everything by one.
    fn endpoint_state(&self, dev_block: usize, dci: u8) -> EndpointState {
        let at = dev_block + DEV_OUT_CTX + dci as usize * self.context_size;
        EndpointState::decode(unsafe { read_volatile(self.dma().ptr_at(at) as *const u32) })
    }

    fn advance_event_ring(&mut self) {
        self.event_head = (self.event_head + 1) % RING_SIZE as u16;
        if self.event_head == 0 {
            self.event_phase = !self.event_phase;
        }
        let erdp = self.dma().phys() + OFF_EVT_RING as u64 + (self.event_head as u64) * 16;
        self.rt_base.write_u64(IR0_ERDP, erdp | (1 << 3)); // EHB clears interrupt pending
        self.rt_base.write_u32(IR0_IMAN, 3); // clear IP (W1C) + keep IE
    }

    fn ring_doorbell(&self, slot: u8, dci: u8) {
        fence(Ordering::Release);
        self.db_base.write_u32(slot as u64 * 4, dci as u32);
    }

}

/// Every xHCI controller on the machine, in PCI enumeration order.
///
/// A `Vec` and not an `Option`, because the machine this project targets has
/// two: Tiger Lake carries a USB4 xHCI in the Thunderbolt block *ahead* of the
/// PCH's on the bus, and the laptop's own ports hang off the second one. The
/// driver that kept one controller reported that the T14 had no USB input.
static XHCI: Lock<Vec<XhciController>> = Lock::new(Vec::new());

/// Process xHCI events if this CPU has an unserviced interrupt record, or if a
/// port's state machine is due to be stepped.
///
/// Records live on the CPU that took the interrupt (which its ISR forces
/// into the scheduler via need_resched), so on every other CPU this is one
/// uncontended atomic op on its own cache line — callers need no cpu gate.
///
/// [`PORT_WORK_AT`] is the second reason and it is global rather than per CPU,
/// because the wait it represents is wall clock and belongs to no CPU: the
/// interrupt that started a debounce is the last one the controller has to
/// give, so *some* CPU has to come back for it. Reading a deadline rather than
/// a flag is what keeps that from being a lock every CPU takes every pass for
/// the length of the debounce.
///
/// Every controller is polled, because every controller's message carries the
/// same vector and `irq_ring` keeps one record per source: the record says
/// that *an* xHC interrupted, never which. Polling a quiet controller costs one
/// read of its event ring's next TRB.
///
/// Thread context only. It takes `XHCI` and dispatches HID reports, which take
/// the keyboard held-set and both event queues; an ISR calling this would spin
/// on whichever of those the thread it interrupted holds.
///
/// **And `drain_irqs` is the only caller there should ever be.** This is not
/// bookkeeping — it enumerates hot-plugged devices and recovers broken
/// endpoints, and both spin on deadlines measured in seconds while holding
/// `XHCI`, which is a ticket spinlock and therefore preemption off for its
/// whole life. Called from a syscall, it makes that syscall's thread the
/// driver's engine and stops the CPU rescheduling for as long as the bus takes.
/// `fd::try_read` called it for `Descriptor::{Keyboard, Mouse}` so a read would
/// see a report that had just landed; on the T14 that made the compositor's own
/// mouse read the hot-plug engine and froze the desktop for seconds at a time,
/// with a live kernel and nothing dropped. A caller that wants fresh input
/// wants the scheduler pass that is already about to run, not this.
pub fn poll_if_pending() {
    let interrupted = crate::irq_ring::pending(crate::irq_ring::IrqSource::Xhci);
    if !interrupted {
        match PORT_WORK_AT.load(Ordering::Relaxed) {
            0 => return,
            at if crate::clock::nanos_since_boot() < at => return,
            _ => {}
        }
    }
    // **Decline rather than queue.** Every CPU reaches this at the top of
    // every pass, and while a port has work outstanding every one of them
    // finds it due — so `lock()` here puts as many CPUs as the machine has on
    // one ticket queue, each spinning with preemption disabled, at the one
    // place the scheduler cannot afford to be.
    //
    // A `try_lock` loses nothing, and that is the argument rather than a
    // mitigation: the CPU holding this lock is doing precisely the work this
    // CPU came to do, against one shared event ring and one shared port
    // machine. Waiting for it buys a second look at a state somebody else has
    // already advanced. The decline costs one pass of latency, and the idle
    // loop's pre-halt check reads `irq_ring` and `PORT_WORK_AT` directly, so
    // the CPU comes straight back instead of sleeping through it.
    let Some(mut guard) = XHCI.try_lock() else { return };
    // Consumed only now that the work will actually be done. `take` clears the
    // slot and an ISR coalesces into an empty one, so a CPU that took its
    // record and then declined the lock would have dropped a wake with nothing
    // left to re-post it.
    crate::irq_ring::take(crate::irq_ring::IrqSource::Xhci);
    let mut wake_at: Option<u64> = None;
    for ctrl in guard.iter_mut() {
        if let Some(at) = ctrl.poll() {
            wake_at = Some(wake_at.map_or(at, |w: u64| w.min(at)));
        }
    }
    PORT_WORK_AT.store(wake_at.unwrap_or(0), Ordering::Relaxed);
}

/// Everything above the controller needs to know about one bound disk.
#[derive(Clone, Copy, Debug)]
pub struct StorageGeometry {
    /// What the device itself addresses in, straight off READ CAPACITY.
    pub logical_block_bytes: u32,
    /// The same capacity in the 4 KiB blocks `BlockDevice` is written in.
    pub blocks: u64,
}

/// How many disk numbers this machine has issued, so every value below it
/// names a disk that was bound at some point in this boot.
pub fn storage_count() -> usize {
    DISKS_BOUND.load(Ordering::Relaxed)
}

/// Run `f` against the machine's `index`-th disk, wherever it is.
///
/// A search and not arithmetic. Which controller a disk is on and which of its
/// pool blocks it took are both fixed for the disk's life, but neither is
/// derivable from a number handed out machine-wide — and the number is what the
/// block layer holds.
fn with_disk<R>(index: usize, f: impl FnOnce(&mut XhciController, usize) -> R) -> Option<R> {
    let mut guard = XHCI.lock();
    for ctrl in guard.iter_mut() {
        if let Some(at) = ctrl
            .msc
            .iter()
            .position(|block| block.disk.is_some_and(|d| d.index == index))
        {
            return Some(f(ctrl, at));
        }
    }
    None
}

/// The geometry of the machine's `index`-th disk, and `None` where there is no
/// such disk — which after an unplug includes an index that used to name one.
/// That is what stops a fresh `usb_storage::open` handing out a handle to a
/// device that has been pulled.
pub fn storage_geometry(index: usize) -> Option<StorageGeometry> {
    with_disk(index, |ctrl, at| Some(ctrl.msc[at].disk?.dev.geometry())).flatten()
}

/// Whether the machine's `index`-th disk is still being spoken to. `Some(false)`
/// and not `None` for one that was unplugged: the caller asking is one that
/// already holds a handle, and "it is gone" is an answer where "there is no
/// such index" would be a lie.
#[cfg(feature = "boot-actuators")]
pub fn storage_online(index: usize) -> Option<bool> {
    (index < storage_count()).then(|| {
        with_disk(index, |ctrl, at| ctrl.msc[at].disk.is_some_and(|d| d.dev.online()))
            .unwrap_or(false)
    })
}

/// Under-deliver the next READ(10) on the disk the gate is driving. Armed by
/// `usb_gate`, so which transfer it lands on is a known one — see
/// [`msc::short_read`].
#[cfg(feature = "boot-actuators")]
pub fn arm_short_read() {
    msc::short_read::arm();
}

