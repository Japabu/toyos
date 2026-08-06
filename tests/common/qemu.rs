use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{fs, thread};

use super::compile;

/// When true, serial output is printed to stderr as it arrives.
pub static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Distinguishes every file one QEMU boot owns from every other boot's within
/// one test process — the wav capture, the UART log, the QMP socket, the
/// screendump, and the bootable image itself.
static BOOT_SEQ: AtomicU32 = AtomicU32::new(0);

/// Guests that have been booted and not yet dropped.
///
/// Gate A's numbers were recorded with one QEMU on the host and nothing else
/// (`tests/audio-baseline.toml`), so "the parallel phase has drained" is a
/// precondition of the audio block rather than a property of where it sits in
/// `main`. This is what lets it be asserted instead of arranged — see
/// [`live_instances`].
static LIVE: AtomicU32 = AtomicU32::new(0);

/// How many guests are up right now, across every thread.
pub fn live_instances() -> u32 {
    LIVE.load(Ordering::SeqCst)
}

/// How many guests the phase now running may have up at once.
///
/// The harness's own wall-clock margins are margins on the *host*, and they were
/// all derived when one guest had it to itself. Four guests is a different
/// machine, so such a margin has to be stated against the regime it runs in
/// rather than widened outright — which is what this multiplies. A serial phase
/// sets it back to 1 and gets the number it always had.
static WIDTH: AtomicU32 = AtomicU32::new(1);

pub fn set_width(width: u32) {
    assert!(width >= 1, "a phase runs at least one guest");
    WIDTH.store(width, Ordering::SeqCst);
}

/// A liveness ceiling, stated for one guest and paid out for the phase's.
///
/// Every timeout a test hands [`QemuInstance::run_test`] and its relatives is a
/// guard against a wedge, never a verdict: the assertion is what the guest
/// *said*, and a test whose pass depended on a deadline expiring would be
/// asserting on the host's clock. So the number in the source stays the number
/// its author reasoned about — one guest, this host — and the phase multiplies
/// it, exactly as `wait_for_ready` has multiplied the boot timeout since the
/// parallel phase landed.
///
/// The cost of getting this wrong in the generous direction is that a wedge
/// takes longer to report. The cost in the other direction is a red run that
/// says a guest hung when it was only sharing a machine, which is the failure
/// mode that put the whole shared block in the serial tail.
pub fn budget(one_guest: Duration) -> Duration {
    one_guest * WIDTH.load(Ordering::SeqCst)
}

/// The hardware shape QEMU presents to the guest.
///
/// Not a display setting: each variant is a whole machine. `Headless` is the
/// historical test config -- no VGA and no GPU device at all, so firmware
/// publishes no GOP and `kernel_args.gop_framebuffer` is zero. `Gop` swaps in
/// `-vga std` so firmware publishes a linear framebuffer, which is the path a
/// laptop takes and the only one in which the on-screen panic console renders
/// anything. `Metal` goes the whole way to the target laptop's shape.
#[derive(Clone, Copy, PartialEq)]
pub enum Profile {
    Headless,
    Gop,
    /// M1 metal-sim: GOP, NVMe, xHCI with the boot stick on it, i8042 from
    /// q35, and nothing else -- no virtio device and no USB HID. This is the
    /// machine shape that gets flashed, so it is the one the input tests run
    /// on. The 16550 stays: every defect metal-sim has found came from the
    /// device shape, and with a console the guest can be driven over the
    /// ===TEST_START=== protocol like any other. [`BootOptions::mute`] takes
    /// it away for the one test that certifies the T14's literal shape.
    Metal,
    /// metal-sim with the T14's internal xHCI actually populated: the boot
    /// stick plus five more devices, two of them keyboards. The laptop's
    /// controller carries a camera, Bluetooth and a fingerprint reader
    /// alongside whatever is plugged in, and a profile with one USB device
    /// cannot see any defect that needs a fourth.
    MetalUsb,
    /// metal-sim with the T14's actual NVMe capacity instead of a token
    /// image. Device *size* is a shape dimension and it was the one nobody
    /// had varied: every test disk was small enough that a per-device-block
    /// index fit under the object allocator's 2 MiB ceiling, so the first
    /// boot on the laptop was the first time anything asked for a
    /// device-sized allocation.
    MetalDisk,
    /// metal-sim with no NVMe controller at all.
    ///
    /// Device *presence* is the shape dimension underneath size and sector
    /// size, and it was the one nobody had varied for storage: every profile
    /// gave the guest a disk, so nothing asked what the kernel does without
    /// one. The answer was `.expect("NVMe: no controller found")` at 0.08 s.
    /// The bootloader reads the initrd through UEFI before ExitBootServices,
    /// so a machine really can boot ToyOS with no NVMe -- and a controller
    /// hidden behind a firmware setting looks exactly the same.
    Diskless,
    /// metal-sim with a namespace formatted in 8 KiB logical blocks.
    ///
    /// Sector size is a shape dimension in exactly the sense
    /// `specs/device-test-strategy.md` means, and it was one the harness could
    /// not express: every profile got QEMU's implicit 512-byte namespace, so
    /// nothing asked the driver what it does with a device it cannot address.
    /// The answer was `4096 / sector_size == 0` and then a divide by zero, at
    /// 0.068 s, before storage is up and before there is a console to report
    /// it on.
    ///
    /// 8192 rather than something absurd because it is real: 8 KiB-format
    /// namespaces ship, and this driver's whole stack above the sector layer
    /// is written in 4096-byte blocks. The guest is expected to refuse the
    /// device by name, so this profile boots no userland at all.
    NvmeWideSector,
    /// metal-sim with a second USB stick beside the boot stick.
    ///
    /// The boot stick is on the bus in every profile and is the one device the
    /// guest must never write to, so a storage test needs a *second* disk —
    /// one the harness stages on the host, stamps as writable, and reads back
    /// afterwards. Presence of that disk is the shape dimension; every other
    /// profile is its absence.
    UsbDisk,
    /// [`Profile::UsbDisk`] with the second stick formatted in 4 KiB logical
    /// blocks. Sector size is a shape dimension for USB exactly as it is for
    /// NVMe, and it is the one that produced a divide-by-zero there.
    UsbDisk4k,
    /// [`Profile::UsbDisk`] with a 3 TB external disk instead of a stick.
    ///
    /// Past 2 TiB a 512-byte-sector device has more sectors than a READ(10)
    /// command can address, and READ CAPACITY(10) stops being able to report
    /// the size at all — so this is the profile where the 16-byte form runs
    /// and where the driver has to refuse a device rather than serve the first
    /// 2 TiB of it. Sparse, so the host pays for the blocks the guest touches.
    UsbDiskHuge,
    /// [`Profile::UsbDisk`] with the second stick's backing opened read-only.
    ///
    /// The only configuration in this suite where a *device* refuses an I/O
    /// the driver was right to issue: QEMU answers WRITE(10) on a write-
    /// protected LUN with a CHECK CONDITION, which is a CSW status of 1 and
    /// the REQUEST SENSE path behind it. Reads on the same disk still work, so
    /// one boot shows the error channel carrying a failure and not carrying a
    /// success.
    UsbDiskReadOnly,
    /// [`Profile::UsbDiskHuge`] with the 3 TB disk attached *ahead* of the boot
    /// stick, so the controller enumerates the disk the driver refuses first.
    ///
    /// Order is the whole shape. `bind` configures a device's two bulk
    /// endpoints into a pool block and only then asks the disk how big it is,
    /// so a disk refused for its size has already pointed the controller's
    /// endpoint contexts at that block. Every other USB profile puts the boot
    /// stick on port 1, where it binds successfully and the question never
    /// arises; here the refusal comes first, and what the *next* disk is given
    /// is the assertion. QEMU assigns ports in device-creation order, measured
    /// against the kernel's own `port N connected` lines.
    UsbDiskRefusedFirst,
    /// More USB disks on one controller than its DMA pool has blocks for.
    ///
    /// `MSC_BLOCKS` is 2 and the boot stick takes one of them, so the second
    /// data disk here is the first one past the ceiling. Every other profile
    /// declares one disk, which is why nothing could ask what a caller sees when
    /// the bound is hit — and the bound is policy, so that answer is the whole
    /// question. Both disks are stamped: the one that binds is written, and the
    /// one the pool had no room for has to come back byte-identical, which is
    /// the claim a log line cannot make.
    ///
    /// Two and not three, though the pool would refuse either way.
    /// `nec-usb-xhci` offers four SuperSpeed ports and QEMU puts the fifth
    /// device behind an auto-created hub, which this driver walks past — so a
    /// third data disk is not one the guest refuses, it is one the guest never
    /// sees, and a count that included it would be measuring QEMU's port
    /// allocation. Measured: `class=0x9 vendor=0409 product=55aa` on port 8 at
    /// full speed, with `no HID boot interface found, skipping`.
    UsbDiskCrowd,
    /// metal-sim with a device that attaches at **full speed**.
    ///
    /// Speed is a shape dimension and it was one no profile varied: every USB
    /// device in this suite is high or SuperSpeed, and those two are the speeds
    /// whose EP0 max packet size is fixed by the specification. Full speed is
    /// the one where it is not — 8, 16, 32 or 64, and unknown until the first
    /// eight bytes of the device descriptor have been read over the very
    /// endpoint being sized. A T14 port answered a USB Transaction Error to a
    /// driver that assumed 64 and read 18 bytes in one go, and no test here
    /// could have seen it.
    ///
    /// Two of them, because `bMaxPacketSize0` is the dimension under test and a
    /// profile with one value of it cannot tell "the driver read the device's
    /// answer" from "the driver's guess happened to match": the tablet answers
    /// **8** and the smartcard reader answers **64**, so one boot carries both
    /// the correction and its absence. Both are full-speed only — QEMU gives
    /// each a `.full` descriptor set and no `.high` one, so `usb_desc_attach`
    /// has no faster speed to pick — and neither needs a chardev, drive or
    /// audiodev to enumerate. Measured with `info usb` on QEMU 11.0.2: both
    /// report 12 Mb/s, and `usb-kbd`, which every other profile uses, reports
    /// 480.
    MetalFullSpeed,
    /// Two xHCI controllers, with every device on the *second* one.
    ///
    /// The T14 Gen 2's literal shape, and the one that had never been staged:
    /// Tiger Lake puts a USB4 xHCI in the Thunderbolt block at 00:0d.0 and the
    /// PCH's at 00:14.0 — same class, same subclass, same prog_if — and the
    /// laptop's own ports hang off the second. Nothing is attached to the
    /// first here, exactly as nothing is plugged into the laptop's Thunderbolt
    /// ports, so a kernel that stops at the first PCI match sees a machine
    /// with no USB at all. The i8042 is off, which is what stops a PS/2
    /// keyboard delivering the keystroke this profile means to route over USB.
    MetalXhciSecond,
    /// Two xHCI controllers with HID devices on both.
    ///
    /// One held-set and one button merge for the whole machine is a claim
    /// about devices on *different controllers* as much as about two on one
    /// bus, and it is a claim nothing could test: with one controller, an
    /// xHCI slot id was a machine-wide name for a device. It is not — the
    /// device lists here are shaped so both pointers land on the same slot id
    /// of their own controller.
    MetalXhciBoth,
    /// The HID controller has no MSI-X, and nothing else can drain its ring.
    ///
    /// The T14's Thunderbolt xHCI has no MSI-X capability — the laptop's own
    /// boot log says so — and every controller in this suite had one, so the
    /// branch that handles its absence had never executed. It logged "using
    /// polled mode" and returned, and there is no polled mode: the driver
    /// reads an event ring only when vector 0x21 has fired. This profile is
    /// the machine where the driver has to fall through to MSI and where an
    /// injected keystroke is the only thing that can prove it did — which
    /// takes a machine with no USB storage on it at all, for the reason the
    /// shape below states.
    MetalXhciMsi,
    /// Two controllers, the second with neither MSI-X nor MSI.
    ///
    /// A function offering neither is not a machine that ships — QEMU is the
    /// only place it can be built — but "this driver cannot drive this
    /// controller" is a state the code has to be able to reach and say, and
    /// nothing else can stage it. The first controller is ordinary and carries
    /// the boot stick, so the refusal is visibly *per controller*: the machine
    /// boots, and the HID on the crippled one is refused by name rather than
    /// enumerated and left mute.
    MetalXhciNoIrq,
    /// Two controllers, and every input device arrives *after* the boot.
    ///
    /// The T14's shape for the one thing no profile stages: its Thunderbolt
    /// xHCI at 00:0d.0 has five ports and has never had a device on them
    /// (`specs/metal-hardware-inventory.md`), so the controller a user plugs
    /// into is the one that enumerated nothing at boot. Here the second
    /// controller is that one and the boot stick is on the first.
    ///
    /// The boot-time device list is one `usb-tablet`, and every part of that is
    /// load-bearing. It is a pointer, so a late-bound one has to compose with a
    /// source that already exists rather than being the first. It is
    /// *absolute*, so QEMU has no relative handler until a `usb-mouse` is
    /// plugged in — which makes an injected `rel` event ground truth that the
    /// late device is the one delivering, not the boot-time one. And it is not
    /// a keyboard: with `i8042=off` this machine has no keyboard at all until
    /// one is hot-plugged, so a keystroke that arrives can only have come
    /// through the device that was added after the boot.
    MetalHotplug,
    /// metal-sim with no IOMMU at all, so firmware publishes no `DMAR`.
    ///
    /// Presence of the unit is the shape dimension, and it is the one QEMU
    /// gives for free that no real machine gives at all: on hardware, "no
    /// DMAR" and "VT-d disabled in firmware setup" are the same observation
    /// (`specs/iommu-spec.md` §2.2). This is the machine where the kernel has
    /// to say which of the two it cannot tell apart.
    NoIommu,
    /// metal-sim whose unit advertises a 39-bit address width instead of 48.
    ///
    /// `CAP.SAGAW` is a register the guest decodes into a page-table depth,
    /// and a suite with one value of it cannot tell a decode from a constant.
    /// Both widths are real: 39-bit units ship, and the IOVA base every domain
    /// gets is derived from this number (`specs/iommu-spec.md` §5.3).
    IommuNarrow,
    /// metal-sim whose unit cannot remap interrupts.
    ///
    /// Two registers move together — the DMAR's own `INTR_REMAP` flag and the
    /// unit's `ECAP.IR` — and `specs/iommu-spec.md` §2.2 gives them separate
    /// refusals, because a platform that declares it cannot remap and a unit
    /// that cannot are different facts a user can act on differently.
    IommuNoIntremap,
    /// metal-sim with three Intel HDA controllers, shaped so that one boot
    /// runs both arms of every question `specs/hda-driver-plan.md` H0 asks.
    ///
    /// A machine nobody ships, and deliberately so: H0's probe is a diagnostic
    /// aimed at one laptop, and the only thing the harness can certify is that
    /// each branch of it produces a sensible answer when the hardware takes
    /// that shape. Which branch a real machine runs is the T14's to say.
    ///
    /// - `hda0` is function 0 of a multifunction slot with two codecs behind
    ///   it. Its isolation scope is *not* a singleton, which is `iommu-spec.md`
    ///   §7.3's refusal and the T14's own shape (function 3 of five); its two
    ///   codecs are §2.3's first-match trap, where a driver taking the first
    ///   answer binds display audio and plays silence out of the speakers.
    /// - `hda1` is function 1 of that slot with **no codec at all**. That is
    ///   question (b)'s dead answer — `STATESTS` reads zero — and on a real
    ///   Tiger Lake it means the audio path is behind the vendor DSP or on
    ///   SoundWire, and the plan is over on that machine.
    /// - `hda2` has a slot to itself and one codec: the singleton scope §7.3
    ///   permits, and the ordinary alive case.
    ///
    /// Slots 16 and 17 because q35 leaves them free and because a fixed
    /// address is the only way to state "these two are functions of one
    /// device" — which is the whole of the first bullet.
    MetalHda,
}

/// The vIOMMU a profile puts on the machine.
///
/// A whole machine dimension rather than a flag: the unit is what decodes
/// every DMA and every interrupt message on the bus. Two fields, because two
/// are what a guest can tell apart — `aw_bits` moves `CAP.SAGAW`, `intremap`
/// moves `ECAP.IR` and the DMAR's `INTR_REMAP` flag — and a harness that
/// stages one value of each cannot distinguish a kernel that reads those
/// registers from one that prints what it expected to find.
///
/// `caching-mode` is deliberately not a field. It is on everywhere: it is the
/// stricter configuration, it is the only one QEMU can stage, and
/// `specs/iommu-spec.md` §5.5 refuses to branch on it — so a profile that
/// turned it off would be staging a machine no code here distinguishes.
#[derive(Clone, Copy, PartialEq)]
pub struct Iommu {
    /// `aw-bits`. QEMU 11.0.2 takes 39 or 48 and nothing else.
    pub aw_bits: u8,
    /// Interrupt remapping. Off is a platform declaring it cannot remap.
    pub intremap: bool,
}

/// What every profile but the three that vary it declares: the widest address
/// width QEMU offers and interrupt remapping on, which is
/// `specs/iommu-spec.md` §9 stage I0's configuration.
pub const IOMMU_DEFAULT: Iommu = Iommu { aw_bits: 48, intremap: true };

/// The controller every profile but [`Profile::MetalUsb`] gets. `nec-usb-xhci`
/// registers `MAX(p2, p3)` attachable USB ports over `p2 + p3` port registers —
/// the two ranges are two speed-specific views of the same ports, not two sets
/// of them — so the default `p2=4,p3=4` takes **four** devices, two short of the
/// crowded set rather than one.
const XHCI_DEFAULT: &str = "nec-usb-xhci,id=xhci";
/// Eight attachable ports, which is `MAX(p2=8, p3=4)`, over twelve port
/// registers: 1-4 the SuperSpeed view, 5-12 the USB2 view. Measured on QEMU
/// 11.0.2 against the kernel's own lines — `max_ports=12`, and the six devices
/// landing on registers 1 and 6-10. The boot stick is a `usb-storage` with a
/// SuperSpeed descriptor, so it takes the SuperSpeed view of the first port and
/// is enumerated *before* every HID; the five devices below are full or high
/// speed and take the USB2 view of ports 2-6. Six of eight used, two spare.
///
/// `slots=` would have been the natural way to stage slot exhaustion, and it
/// is not: on QEMU 11.0.2 `nec-usb-xhci,slots=N` reads back as N through
/// `qom-get` and HCSPARAMS1 still reports 64, `qemu-xhci` has no such property
/// at all, and Enable Slot ignores the MaxSlotsEn the driver writes to CONFIG.
/// The kernel's own `xhci-one-slot` feature is what drives that path.
const XHCI_WIDE: &str = "nec-usb-xhci,id=xhci,p2=8";
/// A second controller, for the profiles that stage a machine with two. Only
/// the id differs — the point is precisely that the two are indistinguishable
/// by class, subclass and prog_if, which is why taking the first PCI match
/// looked right for as long as it did.
const XHCI_SECOND: &str = "nec-usb-xhci,id=xhci1";
/// A controller with no MSI-X table, which leaves `msi=auto` to give it MSI —
/// the shape of the T14's Thunderbolt xHCI and of Intel PCH parts generally.
const XHCI_MSI_ONLY: &str = "nec-usb-xhci,id=xhci1,msix=off";
/// A controller with no message-signalled interrupts at all, in each of the
/// two bus positions a profile puts one in. Nothing on a PCIe bus is really
/// built this way; it is how the harness reaches the branch where the driver
/// has to refuse a controller instead of driving it blind — and, in the first
/// position, how it takes USB storage off a machine entirely.
const XHCI_NO_IRQ_FIRST: &str = "nec-usb-xhci,id=xhci,msix=off,msi=off";
const XHCI_NO_IRQ_SECOND: &str = "nec-usb-xhci,id=xhci1,msix=off,msi=off";

/// [`Profile::MetalHda`]'s three controllers, in the order QEMU creates them.
///
/// The slot numbers are the whole shape: `10.0` and `10.1` are two functions of
/// one device, which is `iommu-spec.md` §7.3's non-singleton scope and the
/// T14's own arrangement, and `11.0` has a slot to itself, which is the
/// singleton the same rule permits. `cad=` places each codec at a chosen link
/// address, so "two codecs answered" is staged rather than hoped for.
const HDA_THREE: &[&str] = &[
    "intel-hda,id=hda0,addr=10.0,multifunction=on",
    "hda-output,bus=hda0.0,cad=0,audiodev=hdaaud",
    "hda-duplex,bus=hda0.0,cad=1,audiodev=hdaaud",
    "intel-hda,id=hda1,addr=10.1",
    "intel-hda,id=hda2,addr=11.0",
    "hda-output,bus=hda2.0,cad=0,audiodev=hdaaud",
];

/// Everything a profile decides about the machine, in one table. A new
/// variant answers every question here or does not compile — which `self !=
/// Profile::Metal` did the opposite of: it handed anything that was not
/// literally Metal the whole virtio block, a USB keyboard and a console.
struct Shape {
    /// `-vga` mode. "none" leaves firmware with no GOP to publish.
    vga: &'static str,
    /// Video memory, which is what decides the panel: OVMF offers every mode
    /// that fits in it and the bootloader takes the one with the most pixels.
    /// `None` is QEMU's default 16 MiB, whose largest mode is 2048x2048 --- a
    /// panel that is a whole number of glyph rows tall, which no real one has
    /// to be. Declared rather than defaulted because the panel's *size* is a
    /// shape dimension exactly as a disk's is, and the tests that read pixels
    /// were all blind to the remainder until one profile had one.
    vgamem_mb: Option<u32>,
    /// virtio-net, virtio-sound, and the console on virtio-serial.
    virtio: bool,
    /// The `-device` argument for each xHCI controller, port and slot counts
    /// included. A list because a machine can have more than one and the T14
    /// does — its keyboard is on the second.
    xhci: &'static [&'static str],
    /// The bus the boot stick and the second USB disk attach to. Named rather
    /// than assumed, because which controller carries the storage is a shape
    /// dimension once there is more than one: the index the block layer holds
    /// has to name the same disk either way.
    storage_bus: &'static str,
    /// Every USB device besides the boot stick, each naming its own bus.
    /// Absence is what makes an i8042 test measure anything: QEMU activates
    /// one input handler per device class, so with a usb-kbd present every
    /// injected keystroke goes to it.
    usb: &'static [&'static str],
    /// The NVMe namespace's size. The backing file is sparse, so this is free
    /// to state honestly — and it has to be stated, because a kernel
    /// structure sized per device block is bounded by this number and by
    /// nothing else.
    nvme_bytes: u64,
    /// The namespace's logical block size. Stated per profile for the same
    /// reason `nvme_bytes` is: it is a dimension of the device, the driver
    /// turns it into a shift and a divisor, and QEMU's implicit namespace only
    /// ever produced one value of it.
    nvme_lba_bytes: u32,
    /// Every `usb-storage` device besides the boot stick, in the order QEMU
    /// creates them.
    ///
    /// A list and not one device's dimensions. **How many disks are on the bus
    /// is a shape dimension in its own right**: the driver's DMA pool holds
    /// `MSC_BLOCKS` of them and refuses the rest by name, and every profile
    /// that could have asked what happens at that ceiling declared exactly one.
    /// The order is the second half of the same field — QEMU hands out
    /// root-hub ports in device-creation order, so where the boot stick falls
    /// in this list is what decides which disk the controller enumerates first.
    usb_disks: &'static [UsbDisk],
    /// Every Intel HDA controller on the machine and the codecs behind each,
    /// as `-device` arguments in the order QEMU is to create them. Empty is
    /// what every profile but [`Profile::MetalHda`] declares, and it is the
    /// machine this kernel has always booted: audio through virtio-sound or
    /// through nothing at all.
    ///
    /// Presence of a class-0403 *function* is the shape dimension, and it is
    /// separate from whether anything answers on the link behind it — which is
    /// `specs/hda-driver-plan.md` H0's question (b), and what the codec
    /// arguments in this list decide per controller.
    hda: &'static [&'static str],
    /// The unit that decodes this machine's DMA, or its absence. Stated per
    /// profile because absence is a shape and because the unit's own
    /// capabilities are what the kernel reads at boot.
    iommu: Option<Iommu>,
}

/// One `usb-storage` device beside the boot stick.
#[derive(Clone, Copy)]
pub struct UsbDisk {
    /// Its size. Stated for the same reason the namespace's is — the driver
    /// turns it into an LBA, and whether that LBA fits the command it is sent
    /// in is a property of this number. The backing is sparse, so a realistic
    /// one is nearly free.
    pub bytes: u64,
    /// Its logical block size. `usb-storage` takes any power of two from 512 B
    /// up, so unlike the boot stick this is something a profile can choose.
    pub lba_bytes: u32,
    /// Open its backing read-only, so the guest's writes are refused by the
    /// device rather than by the driver. Nothing else in this suite can make a
    /// real device say no to an I/O the driver was right to issue.
    readonly: bool,
    /// Attach it *ahead* of the boot stick. Which disk comes first is a shape
    /// dimension the moment one of them can be refused: a driver that hands the
    /// pool block of a failed bind to the next disk is only observable when the
    /// failure is first.
    before_boot_stick: bool,
}

impl UsbDisk {
    /// The nominal 32 GiB stick this suite's storage tests are staged on, and
    /// what a profile carries when it just needs a disk it may write to.
    const DATA: Self = Self {
        bytes: USB_STICK_BYTES,
        lba_bytes: 512,
        readonly: false,
        before_boot_stick: false,
    };
    /// A 3 TB external disk, which this driver has to refuse by name rather
    /// than serve the first 2 TiB of.
    const HUGE: Self = Self { bytes: USB_HUGE_BYTES, ..Self::DATA };
}

/// QEMU's name for the `i`-th data disk's backing, and for the device in front
/// of it. Derived from the position rather than declared, so a profile cannot
/// give two disks one name.
fn usb_drive_id(i: usize) -> String {
    format!("usbdisk{i}")
}

/// The device id, which is what `device_del` names.
pub fn usb_device_id(i: usize) -> String {
    format!("usbdev{i}")
}

/// What every profile but [`Profile::MetalDisk`] gives the guest. Large
/// enough for a filesystem, small enough that a boot formats it quickly.
const NVME_SMALL: u64 = 128 * 1024 * 1024;

/// What every namespace but [`Profile::NvmeWideSector`]'s reports — QEMU's
/// implicit default, and the T14's.
const NVME_LBA_DEFAULT: u32 = 512;

/// The data stick every USB storage profile but [`Profile::UsbDiskHuge`]
/// carries: a nominal 32 GiB stick, the size of the class of device this
/// project boots from. Chosen rather than measured off one part — but not a
/// token number either, because the last 4 KiB block on it sits at sector
/// 67,108,856, which needs 27 bits of LBA. A 128 MiB scratch image needs 18
/// and could not tell a truncated LBA field from a correct one.
pub const USB_STICK_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// A 3 TB external USB disk: a device that exists, and one this driver cannot
/// address. At 512-byte sectors it has 6,442,450,944 of them, so READ(10)'s
/// 32-bit LBA is a bit short and READ CAPACITY(10) cannot report the size —
/// which is the only configuration in which the 16-byte form runs.
pub const USB_HUGE_BYTES: u64 = 3 * 1024 * 1024 * 1024 * 1024;

/// The T14 Gen 2's namespace, to the byte: 500,118,192 sectors of 512 B.
/// Taken from the laptop's own boot line rather than rounded from "244 GB",
/// so a test that asserts on the block count is asserting against the machine
/// that gets flashed.
pub const NVME_T14_BYTES: u64 = 500_118_192 * 512;
/// The same device as the kernel counts it: 62,514,774 blocks of 4 KiB.
pub const NVME_T14_BLOCKS: u64 = NVME_T14_BYTES / 4096;

impl Profile {
    fn shape(self) -> Shape {
        match self {
            Self::Headless => Shape {
                vga: "none",
                vgamem_mb: None,
                virtio: true,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::Gop => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: true,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::Diskless => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                // Zero is the absence, not a zero-length disk: `nvme_args`
                // emits no controller, no namespace and no backing file.
                nvme_bytes: 0,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::Metal => Shape {
                vga: "std",
                // The T14's panel. 1920x1080x4 is 8,294,400 bytes, so 8 MiB
                // admits it and excludes every mode with more pixels ---
                // 1920x1200 and 2048x1536 both need more, and 1600x1200 has
                // fewer pixels, so this is the one the bootloader picks. It
                // gives 240x67 cells with 8 pixels left over at the bottom,
                // which is the geometry the machine actually has and the one
                // the 2048x2048 default could not express.
                vgamem_mb: Some(8),
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // Two keyboards and two pointers, because the collision this
            // stages is between devices of the same HID class; a hub for a
            // second non-HID device, since it needs no backing file and the
            // driver has to walk past it exactly as it walks past the stick.
            Self::MetalUsb => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_WIDE],
                storage_bus: "xhci.0",
                usb: &[
                    "usb-kbd,bus=xhci.0",
                    "usb-kbd,bus=xhci.0",
                    "usb-mouse,bus=xhci.0",
                    "usb-tablet,bus=xhci.0",
                    "usb-hub,bus=xhci.0",
                ],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::MetalDisk => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_T14_BYTES,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::NvmeWideSector => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: 8192,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDisk => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk::DATA],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDisk4k => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk { lba_bytes: 4096, ..UsbDisk::DATA }],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskHuge => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk::HUGE],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskRefusedFirst => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk { before_boot_stick: true, ..UsbDisk::HUGE }],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskReadOnly => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk { readonly: true, ..UsbDisk::DATA }],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::UsbDiskCrowd => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[UsbDisk::DATA, UsbDisk::DATA],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // The first controller carries nothing at all — not even the boot
            // stick, which is on the second with the HID. That is the laptop
            // exactly: a USB-A port is a PCH port, and the Thunderbolt block's
            // controller is empty until something is plugged into it. It also
            // means the disk index the block layer holds names a device on a
            // controller that is not the first, which nothing else stages.
            Self::MetalFullSpeed => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &["usb-wacom-tablet,bus=xhci.0", "usb-ccid,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::MetalXhciSecond => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT, XHCI_SECOND],
                storage_bus: "xhci1.0",
                usb: &["usb-kbd,bus=xhci1.0", "usb-mouse,bus=xhci1.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // A hub ahead of the second controller's HID, so that controller's
            // devices take the same slot ids as the first's: the boot stick is
            // SuperSpeed and enumerates ahead of every USB2 device, and the hub
            // stands in for it. Both mice therefore land on one slot id, which
            // is the collision a slot-derived pointer source turns into a
            // single button-merge entry.
            Self::MetalXhciBoth => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT, XHCI_SECOND],
                storage_bus: "xhci.0",
                usb: &[
                    "usb-kbd,bus=xhci.0",
                    "usb-mouse,bus=xhci.0",
                    "usb-hub,bus=xhci1.0",
                    "usb-kbd,bus=xhci1.0",
                    "usb-mouse,bus=xhci1.0",
                ],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // The boot stick's controller is the one with no interrupt
            // mechanism at all, so the driver refuses it and the machine does
            // no USB storage I/O whatsoever. That is the load-bearing part of
            // this shape, not decoration: `wait_transfer` drains the *whole*
            // event ring and dispatches every HID report in it, so a keyboard
            // sharing a controller with the boot stick delivers on the back of
            // the ESP log's idle-loop writes whether or not its interrupt
            // works. Measured — the first version of this profile put both on
            // one controller and passed with MSI deliberately left disabled.
            Self::MetalXhciMsi => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_NO_IRQ_FIRST, XHCI_MSI_ONLY],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci1.0", "usb-mouse,bus=xhci1.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // Boot stick on the good controller, HID on the crippled one. A
            // keyboard is what makes the absence assertion mean something:
            // the driver has a device it would otherwise bind and announce.
            Self::MetalXhciNoIrq => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT, XHCI_NO_IRQ_SECOND],
                storage_bus: "xhci.0",
                usb: &["usb-kbd,bus=xhci1.0", "usb-mouse,bus=xhci1.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            Self::MetalHotplug => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT, XHCI_SECOND],
                storage_bus: "xhci.0",
                usb: &["usb-tablet,bus=xhci.0"],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(IOMMU_DEFAULT),
            },
            // The three below are metal-sim with one field of the unit moved,
            // so what differs between their boot logs and Metal's is the unit
            // and nothing else on the machine.
            Self::NoIommu => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: None,
            },
            Self::IommuNarrow => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(Iommu { aw_bits: 39, ..IOMMU_DEFAULT }),
            },
            Self::IommuNoIntremap => Shape {
                vga: "std",
                vgamem_mb: None,
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: &[],
                iommu: Some(Iommu { intremap: false, ..IOMMU_DEFAULT }),
            },
            Self::MetalHda => Shape {
                vga: "std",
                vgamem_mb: Some(8),
                virtio: false,
                xhci: &[XHCI_DEFAULT],
                storage_bus: "xhci.0",
                usb: &[],
                nvme_bytes: NVME_SMALL,
                nvme_lba_bytes: NVME_LBA_DEFAULT,
                usb_disks: &[],
                hda: HDA_THREE,
                iommu: Some(IOMMU_DEFAULT),
            },
        }
    }

    /// The unit this profile puts on the machine, or `None`. A test asserting
    /// on what the guest decoded reads the expectation from here rather than
    /// restating it, exactly as [`Profile::usb_disk`] does for the data stick.
    pub fn iommu(self) -> Option<Iommu> {
        self.shape().iommu
    }

    /// Every `usb-storage` device this profile puts on the bus besides the
    /// boot stick, in creation order. A test asserting on a size or a sector
    /// size has to read it from here rather than restate it.
    pub fn usb_disks(self) -> &'static [UsbDisk] {
        self.shape().usb_disks
    }

    /// The first of them, for the tests that stage exactly one.
    pub fn usb_disk(self) -> Option<(u64, u32)> {
        self.usb_disks().first().map(|d| (d.bytes, d.lba_bytes))
    }
}

pub struct BootOptions {
    pub gdb_stub: bool,
    pub debug_wait: bool,
    pub smp: u32,
    pub profile: Profile,
    /// Open a per-instance QMP socket, which `screendump` needs. Per-instance
    /// because screen tests boot their own QEMU and several may exist at once.
    pub qmp: bool,
    pub kernel_features: &'static [&'static str],
    /// Give the machine an i8042 at all. `-machine q35,i8042=off` is the one
    /// absence scenario QEMU can stage.
    pub i8042: bool,
    /// Take the 16550 away, leaving the framebuffer as the guest's only
    /// channel out. Only [`Profile::Metal`] may set it -- the others carry
    /// their console on it or on virtio-serial. A muted guest has no marker
    /// to wait for and no `run_test` to drive, so it is observed with
    /// [`QemuInstance::screendump_while`] and nothing else.
    pub mute: bool,
    /// The console line that means the boot reached the state under test.
    /// Anything other than [`DEFAULT_READY`] also declares that a panic is the
    /// expected outcome rather than a boot failure -- the early-panic screen
    /// test never reaches userland at all. Ignored when [`BootOptions::mute`]
    /// is set, which leaves no console for a marker to arrive on.
    pub ready_marker: &'static str,
    /// Boot against this disk image instead of the shared scratch one.
    ///
    /// The shared image is created by `create_sparse`, which designates it --
    /// so every ordinary test boots a disk the kernel is allowed to format,
    /// and none of them can observe what it does with one it is not. This is
    /// how a test hands the guest somebody else's disk.
    pub nvme_image: Option<PathBuf>,
    /// Boot this disk image instead of the one this call would build.
    ///
    /// The built image is written fresh every boot and its GPT gets a fresh
    /// random partition GUID with it, so a test that has to know what is on
    /// the boot disk *before* the machine starts cannot use it — and asserting
    /// on the partition table firmware read is exactly that. Such a test
    /// builds the image itself, reads it, and hands it over here.
    pub boot_image: Option<PathBuf>,
    /// Back the profile's data disks with these files instead of blank ones,
    /// in the order the profile declares them. The USB gate stages a file
    /// *before* the boot -- the bytes the guest is meant to find are written
    /// there -- and reads it afterwards, so it has to name the file rather
    /// than discover it. Short lists are allowed: the disks past the end get
    /// the blank image their size would have given them anyway.
    pub usb_images: Vec<PathBuf>,
    /// What the emulated RTC reads when the machine starts, as
    /// `YYYY-MM-DDTHH:MM:SS`.
    ///
    /// The wall clock is a device the host can set, which is what makes the
    /// kernel's reading of it checkable from outside the guest: with this
    /// given, the name and the timestamp of the file the guest writes are both
    /// predictable before the machine exists. `None` leaves QEMU's default,
    /// which is the host's own clock in UTC — and leaves the argument off the
    /// command line entirely, so every existing profile assertion sees the argv
    /// it always saw.
    pub rtc_base: Option<&'static str>,
}

/// The in-guest test runner's startup marker.
pub const DEFAULT_READY: &str = "===READY===";

impl Default for BootOptions {
    fn default() -> Self {
        Self {
            gdb_stub: false,
            debug_wait: false,
            smp: 2,
            profile: Profile::Headless,
            qmp: false,
            kernel_features: &[],
            i8042: true,
            mute: false,
            ready_marker: DEFAULT_READY,
            nvme_image: None,
            boot_image: None,
            usb_images: Vec::new(),
            rtc_base: None,
        }
    }
}

#[derive(Debug)]
pub struct TestResult {
    pub name: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub serial: String,
    pub error: Option<String>,
}

pub struct QemuInstance {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    rx: Receiver<String>,
    _reader_thread: thread::JoinHandle<String>,
    audio_wav: PathBuf,
    uart_log: PathBuf,
    nvme_image: PathBuf,
    usb_images: Vec<PathBuf>,
    qmp_socket: Option<PathBuf>,
    screendump: PathBuf,
    /// The image this boot built for itself, which is the only one it may
    /// delete: a [`BootOptions::boot_image`] belongs to the test that staged it
    /// and is often read back after the guest is gone.
    own_boot_image: Option<PathBuf>,
    boot_log: String,
}

/// The bootable disk image a boot with these arguments would use.
///
/// Public because a test that has to know what is on the boot disk *before*
/// the machine starts — or has to put something there — cannot let
/// `boot_with_options` build it: the image is written fresh every boot and its
/// GPT gets a new random partition GUID with it. Such a test builds the image
/// here, works on it, and hands it back through [`BootOptions::boot_image`].
pub fn build_boot_image(
    test_crate: &Path,
    c_tests: &[(String, Vec<u8>)],
    rust_tests: &[(String, Vec<u8>)],
    kernel_features: &[&str],
) -> Vec<u8> {
    let mut extra_files: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, data) in c_tests {
        extra_files.push((format!("bin/test_c_{name}"), data.clone()));
    }
    for (name, data) in rust_tests {
        if name.ends_with(".so") {
            extra_files.push((format!("lib/{name}"), data.clone()));
        } else {
            extra_files.push((format!("bin/test_rs_{name}"), data.clone()));
        }
    }

    let config_path = test_crate.join("system.toml");
    assert!(
        config_path.exists(),
        "Test crate missing system.toml: {}",
        config_path.display()
    );

    let quiet = !VERBOSE.load(Ordering::Relaxed);
    toyos_build::build::build_test_image(
        &compile::repo_root(),
        &config_path,
        &fold_inert(kernel_features),
        quiet,
        &extra_files,
    )
}

/// Actuators that are a `SYS_DEBUG` action arm and nothing else.
///
/// A boot cannot reach any of them; only a test that asks for one by number
/// can. So the four kernels that differ only in which of them they carry are
/// four builds of the same machine, and [`fold_inert`] makes them one.
///
/// **Membership is a claim about the kernel, not about the test that uses it**,
/// and the claim is checkable: each name below has its `#[cfg]` sites in
/// `arch/syscall.rs`'s `SYS_DEBUG` match and nowhere on any path a boot runs.
/// A feature that changes what `init` does, what a driver reads, or what a
/// ceiling is worth belongs in its own build and is not eligible.
/// `specs/test-cost-audit.md` §5.4.3 classifies every one of them.
const INERT_ACTUATORS: &[&str] = &[
    "test-fatal-halt",
    "test-screen-graffiti",
    "test-double-fault",
    "test-heap-ceiling",
];

/// The feature set to build, with every inert actuator replaced by the union of
/// them. A test still names the actuator it needs — that is what its assertion
/// is about — and the build system stops treating the name as a distinct kernel.
fn fold_inert<'a>(requested: &[&'a str]) -> Vec<&'a str> {
    let mut out: Vec<&'a str> = vec!["test-actuators"];
    out.extend(requested.iter().copied().filter(|f| !INERT_ACTUATORS.contains(f)));
    out
}

/// Build all binaries in a test crate.
pub fn build_toyos_bins(crate_path: &Path) -> Vec<(String, Vec<u8>)> {
    let repo = compile::repo_root();
    let quiet = !VERBOSE.load(Ordering::Relaxed);
    toyos_build::build::build_toyos_bins(&repo, crate_path, quiet)
}

/// All kernel serial output goes through log!() which prepends "[kernel ...]".
/// User program output goes through serial::write directly with no prefix.
pub fn is_kernel_line(line: &str) -> bool {
    line.starts_with("[kernel ")
}

/// The in-guest runner's end-of-test marker. Matched anywhere in the line, not
/// as a prefix: the virtio-console is shared and not line-atomic, so a daemon
/// mid-`println!` pushes the marker into the middle of its line. Anchoring on
/// the prefix made the harness miss the marker and time out — measured at 1 in
/// 120 audio boots, where it looked like a guest hang rather than a lost line.
const END_MARKER: &str = "===TEST_END ";

impl QemuInstance {
    /// Build everything and boot QEMU with test binaries in the initrd.
    /// `test_crate` is the path to the test crate (must contain a `system.toml`).
    pub fn boot(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
    ) -> Self {
        Self::boot_with_options(test_crate, c_tests, rust_tests, BootOptions::default())
    }

    pub fn boot_with_options(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
        options: BootOptions,
    ) -> Self {
        let mut features: Vec<&str> = options.kernel_features.to_vec();
        if options.debug_wait {
            features.push("debug-wait");
        }
        let disk = build_boot_image(test_crate, c_tests, rust_tests, &features);

        let test_dir = super::lane::dir();
        let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);

        // Named for the boot rather than for the process. Two guests handed one
        // image file is not a slow test, it is a guest reading bytes another
        // boot is in the middle of writing — and the lane directory alone would
        // not settle it, since one test may hold two instances at once.
        let boot_image = match &options.boot_image {
            Some(staged) => staged.clone(),
            None => {
                let path = test_dir.join(format!("boot-{seq}.img"));
                fs::write(&path, &disk).expect("Failed to write test boot image");
                path
            }
        };
        let own_boot_image = options.boot_image.is_none().then(|| boot_image.clone());

        // Named by size, so two profiles that disagree about the device do
        // not hand each other a filesystem formatted for the wrong one. Reused
        // across the boots of one lane and shared with no other — which is what
        // `super::lane` is for, and why this is not a per-boot name.
        let nvme_bytes = options.profile.shape().nvme_bytes;
        let nvme_image = match &options.nvme_image {
            Some(path) => path.clone(),
            // A profile with no controller gets no backing file either; the
            // path is never passed to QEMU.
            None if nvme_bytes == 0 => test_dir.join("no-nvme"),
            None => {
                let path = test_dir.join(format!("test-nvme-{nvme_bytes}.img"));
                if !path.exists() {
                    toyos_build::build::create_sparse(&path, nvme_bytes);
                }
                path
            }
        };

        // Named by size and block size for the same reason the namespace is:
        // a stamped image is stamped for one geometry, and handing it to a
        // profile that declares another is the mistake the stamp exists to
        // catch rather than one to make here.
        let usb_images: Vec<PathBuf> = options
            .profile
            .usb_disks()
            .iter()
            .enumerate()
            .map(|(i, disk)| match options.usb_images.get(i) {
                Some(path) => path.clone(),
                None => {
                    let path =
                        test_dir.join(format!("test-usb-{}-{}.img", disk.bytes, disk.lba_bytes));
                    if !path.exists() {
                        let file = fs::File::create(&path).expect("create the USB disk image");
                        file.set_len(disk.bytes).expect("size the USB disk image");
                    }
                    path
                }
            })
            .collect();

        let audio_wav = test_dir.join(format!("audio-{seq}.wav"));
        let _ = fs::remove_file(&audio_wav);

        let qmp_socket = options.qmp.then(|| test_dir.join(format!("qmp-{seq}.sock")));
        if let Some(path) = &qmp_socket {
            let _ = fs::remove_file(path);
        }
        let screendump = test_dir.join(format!("screen-{seq}.ppm"));

        // Per-instance, not a fixed /tmp path: the audio gate boots dozens of
        // guests and a screen test waits on this file, so a shared one would
        // let instances read each other's early boot.
        let uart_log = test_dir.join(format!("uart-{seq}.log"));
        let _ = fs::remove_file(&uart_log);

        let qemu = qemu_command(
            &boot_image,
            &nvme_image,
            &usb_images,
            &audio_wav,
            &uart_log,
            qmp_socket.as_deref(),
            &options,
        );
        spawn_and_wait_ready(
            qemu,
            &options,
            Files {
                seq,
                audio_wav,
                uart_log,
                nvme_image,
                usb_images,
                qmp_socket,
                screendump,
                own_boot_image,
            },
        )
    }

    /// Capture the guest's scanout through QMP and return the decoded PPM.
    ///
    /// After a halt the guest is stopped, so the dump is stable. QEMU writes
    /// the file itself, so the only synchronization needed is the command's
    /// own reply.
    pub fn screendump(&mut self) -> super::screen::Ppm {
        let socket = self
            .qmp_socket
            .clone()
            .expect("screendump needs BootOptions { qmp: true }");
        let out = self.screendump.clone();
        let _ = fs::remove_file(&out);

        // A guest that triple-faults exits QEMU (`-no-reboot`), and the
        // socket then refuses every connect. Without this the retry loop
        // spends its full ten seconds and reports `qmp: cannot connect`,
        // which says nothing about what happened — the worst diagnostic the
        // harness produces, for the failure class the metal profile exists to
        // catch. `wait_for_ready` reports the same event properly, but a muted
        // guest never goes through it and no guest goes through it twice.
        let child = &mut self.child;
        let mut qmp = Qmp::connect_while(&socket, || {
            if let Ok(Some(status)) = child.try_wait() {
                panic!("[qemu] QEMU died before the screendump (status: {status})");
            }
        });
        qmp.execute(&format!(
            "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":\"{}\"}}}}",
            out.display()
        ));

        let bytes = fs::read(&out).expect("screendump: QEMU wrote no file");
        super::screen::Ppm::parse(&bytes)
    }

    /// Screendump until the decoded screen carries `needle`, or the timeout.
    ///
    /// Every fatal path needs this, for one of two reasons. The panic
    /// handler's own path paints after the drain that emits the report, so a
    /// marker on serial does not yet prove a paint. The halt_all_cpus paths
    /// are the other way round and once *did* need only a single dump — but a
    /// report too long for one screen now pages, so the screen a marker
    /// proves is only the first of several and any given dump may hold a
    /// different one.
    pub fn screendump_until(&mut self, needle: &str, timeout: Duration) -> super::screen::Ppm {
        self.screendump_while(timeout, Duration::from_millis(100), |dump| {
            dump.text().contains(needle)
        })
    }

    /// Screendump until `done`, or the timeout. A muted guest has no console,
    /// so this is the only way to observe that boot at all — and what it
    /// watches for is a pixel pattern, not text.
    ///
    /// Returns the last dump either way; a caller that timed out gets the
    /// screen as its diagnostic, which under metal-sim is where the kernel's
    /// boot checkpoints and panic report are.
    pub fn screendump_while(
        &mut self,
        timeout: Duration,
        interval: Duration,
        done: impl Fn(&super::screen::Ppm) -> bool,
    ) -> super::screen::Ppm {
        let deadline = Instant::now() + budget(timeout);
        loop {
            let dump = self.screendump();
            if done(&dump) || Instant::now() >= deadline {
                return dump;
            }
            thread::sleep(interval);
        }
    }

    /// Every console line the guest printed before the ready marker.
    ///
    /// The kernel's own boot lines sit in the log ring until the scheduler
    /// drains them, by which time the virtio-console is the backend — so the
    /// 16550 file holds only the bootloader, and this is the only place a
    /// host test can read what the kernel said while booting. Under
    /// [`Profile::Metal`] the 16550 is the console and carries everything;
    /// empty when [`BootOptions::mute`] takes it away.
    pub fn boot_log(&self) -> &str {
        &self.boot_log
    }

    /// Everything the guest put on the 16550 before it switched to the
    /// virtio-console — the only record a guest that died early leaves.
    pub fn uart_log(&self) -> String {
        fs::read_to_string(&self.uart_log).unwrap_or_default()
    }

    /// The wav file the virtio-sound device records into for this boot.
    /// The RIFF size fields stay 0 until QEMU exits cleanly — parse to EOF.
    pub fn audio_wav_path(&self) -> &Path {
        &self.audio_wav
    }

    /// The NVMe backing file. It is what the *device* received, so it is the
    /// only place a storage assertion can stand outside the guest's own
    /// account of itself.
    pub fn nvme_image(&self) -> &Path {
        &self.nvme_image
    }

    /// The data disks' backing files, which is what the *devices* received.
    /// The guest's own account of a write it made is the thing under test, so
    /// it cannot also be the evidence.
    pub fn usb_images(&self) -> &[PathBuf] {
        &self.usb_images
    }

    pub fn stdin_mut(&mut self) -> &mut BufWriter<ChildStdin> {
        &mut self.stdin
    }

    pub fn flush_stdin(&mut self) {
        self.stdin.flush().expect("Failed to flush QEMU stdin");
    }

    /// Keep collecting serial output for `dur` after a test has returned.
    /// soundd flushes its final stats window when the last client leaves,
    /// which races the client process's exit — so the line the audio gate
    /// reads lands on either side of `===TEST_END===`.
    /// **Not scaled by the width**, and it is the one duration in this file that
    /// is not. Callers use it to *pace* — "let the guest run for 400 ms and tell
    /// me what it said" — so multiplying it does not buy a slow guest more room,
    /// it buys the test a longer sleep. `metal_sim_pointer_churn` has
    /// twenty-four of these; scaled, they made it an 86 s job at width 8 and the
    /// critical path of the whole phase.
    pub fn drain_serial(&mut self, dur: Duration) -> String {
        self.drain_for(dur, |_| false)
    }

    /// Drain until `line` reads true of a line just seen, or until the guest
    /// goes quiet for the rest of `dur`.
    ///
    /// A guest that is *shut down* ends a plain [`Self::drain_serial`] the
    /// moment QEMU exits and the reader disconnects, so the ceiling there costs
    /// nothing. A guest the fatal path has halted does not exit — every CPU is
    /// stopped and the process stays up — so the drain pays the whole ceiling
    /// waiting for a machine that will never speak again. `double_fault_stack`
    /// spent twenty seconds of every run that way, which was 80% of it.
    ///
    /// Here the duration *is* a liveness ceiling — the marker is what ends
    /// it — so it scales.
    pub fn drain_until(&mut self, dur: Duration, line: impl Fn(&str) -> bool) -> String {
        self.drain_for(budget(dur), line)
    }

    fn drain_for(&mut self, dur: Duration, line: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + dur;
        let mut out = String::new();
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return out;
            };
            match self.rx.recv_timeout(remaining) {
                Ok(seen) => {
                    out.push_str(&seen);
                    out.push('\n');
                    if line(&seen) {
                        return out;
                    }
                }
                Err(RecvTimeoutError::Timeout) => return out,
                Err(RecvTimeoutError::Disconnected) => return out,
            }
        }
    }

    /// Wait for `marker` on the console, or the timeout.
    ///
    /// A console is a stream and this consumes it: every line up to and
    /// including the marker is taken from whatever reads next.
    pub fn wait_for_console(&mut self, marker: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + budget(timeout);
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.rx.recv_timeout(left) {
                Ok(line) if line.contains(marker) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    /// Send `command` and wait for `marker` on the console.
    ///
    /// For a guest that will never report `===TEST_END`, which is any guest
    /// the fatal path has run through: every CPU is halted by the time the
    /// marker arrives.
    pub fn command_until(&mut self, command: &str, marker: &str, timeout: Duration) -> bool {
        writeln!(self.stdin, "{command}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");
        self.wait_for_console(marker, timeout)
    }

    /// The QMP socket this instance opened. Injection needs it, and it needs
    /// `BootOptions { qmp: true }`.
    pub fn qmp_socket(&self) -> &Path {
        self.qmp_socket.as_ref().expect("qmp_socket needs BootOptions { qmp: true }")
    }

    pub fn run_test(&mut self, name: &str, timeout: Duration) -> TestResult {
        self.run_test_hooked(name, timeout, "", |_| {})
    }

    /// `run_test`, with `action` run once the guest prints `ready_line`.
    ///
    /// The hook is inside the read loop because that is the only place the
    /// two facts meet: the guest is holding the keyboard fd, and the host has
    /// not injected yet. A sleep would be a guess in both directions.
    pub fn run_test_hooked(
        &mut self,
        name: &str,
        timeout: Duration,
        ready_line: &str,
        action: impl FnOnce(&Path),
    ) -> TestResult {
        let mut action = Some(action);
        self.run_test_paced(name, timeout, |socket, line| {
            if ready_line.is_empty() || !line.contains(ready_line) {
                return;
            }
            if let Some(action) = action.take() {
                action(socket.expect("run_test_hooked needs BootOptions { qmp: true }"));
            }
        })
    }

    /// `run_test`, with `step` run on every console line the guest prints.
    ///
    /// [`Self::run_test_hooked`] injects a whole sequence in one call and holds
    /// the reader while it does, so the host runs at its own speed and what
    /// reaches the guest is whatever survived the queues in between — a packet
    /// the guest was never given reads exactly like one it lost. A step driven
    /// by the guest's own output can stay behind it, which is how an injection
    /// test costs a slow guest wall-clock instead of a verdict.
    pub fn run_test_paced(
        &mut self,
        name: &str,
        timeout: Duration,
        mut step: impl FnMut(Option<&Path>, &str),
    ) -> TestResult {
        writeln!(self.stdin, "run {name}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");

        let mut fire =
            |line: &str, socket: Option<&PathBuf>| step(socket.map(PathBuf::as_path), line);

        let timeout = budget(timeout);
        let start = Instant::now();
        let mut stdout = String::new();
        let mut serial = String::new();
        let mut in_test = false;

        loop {
            if start.elapsed() > timeout {
                return TestResult {
                    name: name.to_string(),
                    exit_code: None,
                    stdout,
                    serial,
                    error: Some(format!("timed out after {}s", timeout.as_secs())),
                };
            }

            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    fire(&line, self.qmp_socket.as_ref());
                    if line.contains("===TEST_START ") {
                        in_test = true;
                    } else if let Some(at) = line.find(END_MARKER) {
                        // Everything before the marker is a line some other
                        // console writer was in the middle of when the runner
                        // printed; it is still real output and the audio gate
                        // reads soundd's stats out of it.
                        if at > 0 && in_test {
                            serial.push_str(&line[..at]);
                            serial.push('\n');
                        }
                        let rest = &line[at + END_MARKER.len()..];
                        let rest = rest.split_once("===").map_or(rest, |(head, _)| head);
                        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                        let (exit_code, error) = if parts.len() > 1 {
                            if let Some(code_str) = parts[1].strip_prefix("exit=") {
                                (code_str.parse::<i32>().ok(), None)
                            } else if let Some(err) = parts[1].strip_prefix("error=") {
                                (None, Some(err.to_string()))
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        };
                        return TestResult {
                            name: name.to_string(),
                            exit_code,
                            stdout,
                            serial,
                            error,
                        };
                    } else if line.contains("KERNEL PANIC") {
                        return TestResult {
                            name: name.to_string(),
                            exit_code: None,
                            stdout,
                            serial,
                            error: Some(format!("kernel panic: {line}")),
                        };
                    } else if in_test {
                        serial.push_str(&line);
                        serial.push('\n');
                        if is_kernel_line(&line) {
                            // pure kernel line
                        } else if let Some(idx) = line.find("[kernel ") {
                            // user output with kernel suffix on same line
                            stdout.push_str(&line[..idx]);
                            stdout.push('\n');
                        } else {
                            stdout.push_str(&line);
                            stdout.push('\n');
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return TestResult {
                        name: name.to_string(),
                        exit_code: None,
                        stdout,
                        serial,
                        error: Some("QEMU disconnected".to_string()),
                    };
                }
            }
        }
    }
}

impl Drop for QemuInstance {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.audio_wav);
        let _ = fs::remove_file(&self.uart_log);
        let _ = fs::remove_file(&self.screendump);
        if let Some(socket) = &self.qmp_socket {
            let _ = fs::remove_file(socket);
        }
        // A per-boot image is hundreds of megabytes and a full run makes ~76 of
        // them; the shared name used to make that one file.
        if let Some(image) = &self.own_boot_image {
            let _ = fs::remove_file(image);
        }
        LIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A QMP session. Line-delimited JSON: greeting, `qmp_capabilities`, then
/// commands; the reply carrying `return` is the completion signal. A handful
/// of commands with fixed shapes does not justify a JSON dependency.
struct Qmp {
    stream: std::os::unix::net::UnixStream,
    pending: Vec<u8>,
}

impl Qmp {
    fn connect(socket: &Path) -> Self {
        Self::connect_while(socket, || {})
    }

    /// `on_retry` runs between connect attempts. It is where a caller holding
    /// the QEMU process turns "connection refused" into "QEMU is gone, and
    /// here is its exit status" — see [`QemuInstance::screendump`].
    fn connect_while(socket: &Path, mut on_retry: impl FnMut()) -> Self {
        use std::os::unix::net::UnixStream;
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(e) => {
                    on_retry();
                    assert!(
                        Instant::now() < deadline,
                        "qmp: cannot connect to {}: {e}",
                        socket.display()
                    );
                    thread::sleep(Duration::from_millis(50));
                }
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut qmp = Self { stream, pending: Vec::new() };
        qmp.await_reply("\"QMP\"");
        qmp.execute("{\"execute\":\"qmp_capabilities\"}");
        qmp
    }

    fn await_reply(&mut self, want: &str) {
        use std::io::Read;
        let start = Instant::now();
        loop {
            if let Some(pos) =
                self.pending.windows(want.len()).position(|w| w == want.as_bytes())
            {
                self.pending.drain(..pos + want.len());
                return;
            }
            // A refused command never produces a `return`, so without this the
            // wait spends its whole timeout and reports `qmp: read failed` —
            // which says nothing about the command QEMU declined or why.
            if let Some(at) = self.pending.windows(7).position(|w| w == b"\"error\"") {
                panic!(
                    "qmp: refused while waiting for {want}: {}",
                    String::from_utf8_lossy(&self.pending[at..])
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "qmp: no {want} in reply: {}",
                String::from_utf8_lossy(&self.pending)
            );
            let mut buf = [0u8; 4096];
            let n = self.stream.read(&mut buf).expect("qmp: read failed");
            assert!(n > 0, "qmp: socket closed waiting for {want}");
            self.pending.extend_from_slice(&buf[..n]);
        }
    }

    fn execute(&mut self, command: &str) {
        self.stream.write_all(command.as_bytes()).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.await_reply("\"return\"");
    }
}

/// An open QMP connection for injecting input.
///
/// One connection rather than one per event, because QEMU delivers each
/// `input-send-event` as its own input sync — so a thousand pointer packets
/// is a thousand commands, and a thousand reconnects on top of that is the
/// difference between a second and a minute.
pub struct QmpInput(Qmp);

impl QmpInput {
    pub fn open(socket: &Path) -> Self {
        Self(Qmp::connect(socket))
    }

    fn send(&mut self, body: &[String]) {
        if body.is_empty() {
            return;
        }
        self.0.execute(&format!(
            "{{\"execute\":\"input-send-event\",\"arguments\":{{\"events\":[{}]}}}}",
            body.join(",")
        ));
    }

    /// Every key transition in `events` as one batch, so a chord like Shift+B
    /// arrives as a chord rather than as a race.
    pub fn keys(&mut self, events: &[(&str, bool)]) {
        let body: Vec<String> = events
            .iter()
            .map(|(qcode, down)| {
                format!(
                    "{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":{{\"type\":\"qcode\",\"data\":\"{qcode}\"}}}}}}"
                )
            })
            .collect();
        self.send(&body);
    }

    /// Type `text` on the guest's keyboard, one character at a time.
    ///
    /// The gap between characters is the wire's, not a settling delay for the
    /// assertion: the i8042 carries one scancode per interrupt and the guest
    /// has to drain each before the next, which is the same reason
    /// `metal_sim_input` spaces its five keys.
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            let (qcode, shift) = qcode(ch);
            if shift {
                self.keys(&[("shift", true), (qcode, true), (qcode, false), ("shift", false)]);
            } else {
                self.keys(&[(qcode, true), (qcode, false)]);
            }
            thread::sleep(Duration::from_millis(15));
        }
    }

    /// `times` relative moves of `dx`, all in one command.
    ///
    /// QEMU syncs its input once per command and its PS/2 device *accumulates*
    /// motion between syncs, so this is one packet carrying the sum however
    /// many moves it names — the deterministic form of what a host holding more
    /// packets outstanding than that device's queue meets by accident.
    pub fn mouse_merged(&mut self, dx: i32, times: usize) {
        let body: Vec<String> = (0..times)
            .map(|_| format!("{{\"type\":\"rel\",\"data\":{{\"axis\":\"x\",\"value\":{dx}}}}}"))
            .collect();
        self.send(&body);
    }

    /// One pointer packet: relative motion and/or a button transition.
    pub fn mouse(&mut self, dx: i32, dy: i32, button: Option<(&str, bool)>) {
        let mut body: Vec<String> = Vec::new();
        if let Some((name, down)) = button {
            body.push(format!(
                "{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"{name}\"}}}}"
            ));
        }
        for (axis, value) in [("x", dx), ("y", dy)] {
            if value != 0 {
                body.push(format!(
                    "{{\"type\":\"rel\",\"data\":{{\"axis\":\"{axis}\",\"value\":{value}}}}}"
                ));
            }
        }
        self.send(&body);
    }
}

/// The QEMU qcode for `ch`, and whether Shift is held to produce it.
///
/// A US layout, because that is what `kernel/src/keyboard.rs` boots with. Only
/// the characters a console test types: an unmapped one panics rather than
/// being dropped, since a command missing a character is a test asserting on
/// output nothing was ever asked to produce.
fn qcode(ch: char) -> (&'static str, bool) {
    const LOWER: [&str; 26] = [
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z",
    ];
    const DIGIT: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
    match ch {
        'a'..='z' => (LOWER[ch as usize - 'a' as usize], false),
        'A'..='Z' => (LOWER[ch as usize - 'A' as usize], true),
        '0'..='9' => (DIGIT[ch as usize - '0' as usize], false),
        ' ' => ("spc", false),
        '\n' => ("ret", false),
        '-' => ("minus", false),
        '_' => ("minus", true),
        '.' => ("dot", false),
        '/' => ("slash", false),
        '&' => ("7", true),
        _ => panic!("no qcode for {ch:?}; add it rather than typing something else"),
    }
}

pub fn qmp_send_keys(socket: &Path, events: &[(&str, bool)]) {
    QmpInput::open(socket).keys(events);
}

/// An open QMP connection for attaching and detaching devices while the guest
/// runs — QEMU's own `device_add`/`device_del`, which is what a person
/// plugging something in looks like from the host side.
///
/// Its own type rather than more methods on [`QmpInput`], and never open at the
/// same time as one: a `-qmp unix:…,server` socket serves one monitor, so a
/// caller that needs both alternates. A type called `QmpInput` with
/// `device_add` on it would also be describing the wrong thing.
pub struct QmpDevices(Qmp);

impl QmpDevices {
    pub fn open(socket: &Path) -> Self {
        Self(Qmp::connect(socket))
    }

    /// Attach `driver` on `bus` as `id`, with `extra` naming any further
    /// properties. Every value is a bare JSON string, which is what every
    /// property these tests set happens to be.
    pub fn add(&mut self, driver: &str, bus: &str, id: &str, extra: &[(&str, &str)]) {
        let mut args = format!("\"driver\":\"{driver}\",\"bus\":\"{bus}\",\"id\":\"{id}\"");
        for (key, value) in extra {
            args.push_str(&format!(",\"{key}\":\"{value}\""));
        }
        self.0.execute(&format!("{{\"execute\":\"device_add\",\"arguments\":{{{args}}}}}"));
    }

    pub fn del(&mut self, id: &str) {
        self.0
            .execute(&format!("{{\"execute\":\"device_del\",\"arguments\":{{\"id\":\"{id}\"}}}}"));
    }

    /// Give QEMU an image to back a device that is not on the machine yet, so
    /// a hot-plugged disk needs nothing in argv. A disk declared at boot is a
    /// disk the guest could have enumerated at boot.
    pub fn blockdev_add(&mut self, node: &str, image: &Path) {
        self.0.execute(&format!(
            "{{\"execute\":\"blockdev-add\",\"arguments\":{{\"node-name\":\"{node}\",\
             \"driver\":\"raw\",\"file\":{{\"driver\":\"file\",\"filename\":\"{}\"}}}}}}",
            image.display()
        ));
    }
}

/// The argv `options` would launch QEMU with, built against placeholder
/// paths. A profile's claim about which devices exist is a claim about this
/// list and nothing else — no screendump can see a device that is present but
/// unused — so this is what a profile assertion has to read.
pub fn profile_argv(options: &BootOptions) -> Vec<String> {
    let p = Path::new("/nonexistent");
    let usb: Vec<PathBuf> = options.profile.usb_disks().iter().map(|_| p.to_path_buf()).collect();
    qemu_command(p, p, &usb, p, p, None, options)
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn qemu_command(
    boot_image: &Path,
    nvme_image: &Path,
    usb_images: &[PathBuf],
    audio_wav: &Path,
    uart_log: &Path,
    qmp_socket: Option<&Path>,
    options: &BootOptions,
) -> Command {
    let shape = options.profile.shape();
    assert!(
        !options.mute || !shape.virtio,
        "mute removes the only console a virtio profile has"
    );

    let repo = compile::repo_root();
    let ovmf_dir = repo.join("ovmf");

    let mut qemu = Command::new("qemu-system-x86_64");

    let kvm = cfg!(target_arch = "x86_64") && Path::new("/dev/kvm").exists();
    if kvm {
        qemu.arg("-accel").arg("kvm");
    }

    // Without this QEMU runs its default-device pass whenever no network
    // option is given, which is exactly and only the Metal profile: measured
    // on QEMU 11.0.2, an e1000e at 00:02.0 with a slirp backend, an empty
    // ide-cd on the ich9-ahci, and an isa-parallel — none of them declared by
    // anything, none of them visible to an argv assertion, and the first of
    // them enough to make netd claim a NIC on the machine whose whole point is
    // that it has none. `-net none` and `-nic none` are gone in QEMU 11; this
    // is the option that does it, and it leaves i8042/ps2-kbd/ps2-mouse alone.
    qemu.arg("-nodefaults");

    // `kernel-irqchip=split` only when there is a unit: interrupt remapping
    // needs the userspace half of the irqchip, and a machine with no unit has
    // no reason to be built differently from the one it has always been.
    let mut machine = String::from("q35");
    if !options.i8042 {
        machine.push_str(",i8042=off");
    }
    if shape.iommu.is_some() {
        machine.push_str(",kernel-irqchip=split");
    }

    if let Some(base) = options.rtc_base {
        qemu.arg("-rtc").arg(format!("base={base}"));
    }

    qemu.arg("-machine")
        .arg(&machine)
        .arg("-cpu")
        .arg(if kvm { "host,+rdrand,+smap,+fsgsbase,+x2apic,+smep" } else { "qemu64,+rdrand,+smap,+fsgsbase,+x2apic,+smep" })
        .arg("-smp")
        .arg(options.smp.to_string())
        .arg("-m")
        .arg("4G")
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=0,file={},readonly=on",
            ovmf_dir.join("OVMF_CODE-pure-efi.fd").display()
        ))
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=1,file={},readonly=on",
            ovmf_dir.join("OVMF_VARS-pure-efi.fd").display()
        ))
        .arg("-drive")
        .arg(format!(
            "if=none,id=stick,format=raw,file={}",
            boot_image.display()
        ));

    // Ahead of every other `-device`: QEMU gives a PCI function the bypassing
    // address space unless the unit exists when the function is created, so a
    // unit emitted after the devices it is meant to decode is a unit that
    // decodes nothing — the vacuity trap `specs/userspace-drivers-spec.md` §7.2
    // is built around, in its harness-side form.
    if let Some(unit) = shape.iommu {
        qemu.arg("-device").arg(format!(
            "intel-iommu,intremap={},caching-mode=on,aw-bits={}",
            if unit.intremap { "on" } else { "off" },
            unit.aw_bits
        ));
    }

    for controller in shape.xhci {
        qemu.arg("-device").arg(*controller);
    }

    // The data disks' own arguments, emitted either side of the boot stick's
    // `-device`. QEMU hands out ports in the order devices are created, so this
    // is the only thing that decides which disk the guest enumerates first.
    // Each carries a device id as well as a drive id, because a test that
    // unplugs one over QMP has to be able to name it.
    let data_sticks: Vec<Vec<String>> = shape
        .usb_disks
        .iter()
        .enumerate()
        .map(|(i, disk)| {
            vec![
                "-drive".to_string(),
                format!(
                    "if=none,id={},format=raw,file={}{}",
                    usb_drive_id(i),
                    usb_images[i].display(),
                    if disk.readonly { ",readonly=on" } else { "" }
                ),
                "-device".to_string(),
                format!(
                    "usb-storage,bus={1},drive={2},id={3},logical_block_size={0},\
                     physical_block_size={0}",
                    disk.lba_bytes,
                    shape.storage_bus,
                    usb_drive_id(i),
                    usb_device_id(i),
                ),
            ]
        })
        .collect();
    for (disk, args) in shape.usb_disks.iter().zip(&data_sticks) {
        if disk.before_boot_stick {
            qemu.args(args);
        }
    }

    qemu.arg("-device")
        .arg(format!(
            "usb-storage,bus={},drive=stick,bootindex=0",
            shape.storage_bus
        ))
        .arg("-vga")
        .arg(shape.vga)
        .arg("-display")
        .arg("none")
        .arg("-no-reboot");
    if let Some(mb) = shape.vgamem_mb {
        qemu.arg("-global").arg(format!("VGA.vgamem_mb={mb}"));
    }

    // Controller and namespace as two devices rather than QEMU's implicit
    // one, so the logical block size is something a profile states instead of
    // something the default decides — and so that stating *zero* bytes gives
    // the guest no controller at all, rather than an empty one. A machine
    // with no NVMe is a shape, and the argv is the only place it is visible:
    // no console line and no screendump can see a device that is absent.
    if shape.nvme_bytes != 0 {
        qemu.arg("-drive")
            .arg(format!(
                "if=none,id=nvme0,format=raw,file={}",
                nvme_image.display()
            ))
            .arg("-device")
            .arg("nvme,serial=deadbeef,id=nvme0ctl")
            .arg("-device")
            .arg(format!(
                "nvme-ns,drive=nvme0,bus=nvme0ctl,logical_block_size={0},physical_block_size={0}",
                shape.nvme_lba_bytes
            ));
    }

    // The mass-storage devices beside the boot stick, and the only ones a test
    // may write to: the boot stick is on the same bus and carries the image the
    // guest is running from. Their logical block sizes are stated rather than
    // left to the default for the same reason the namespace's is.
    for (disk, args) in shape.usb_disks.iter().zip(&data_sticks) {
        if !disk.before_boot_stick {
            qemu.args(args);
        }
    }

    for dev in shape.usb {
        qemu.arg("-device").arg(*dev);
    }

    if !shape.hda.is_empty() {
        // The codecs open a backend even though nothing plays, and `none` is
        // the one that needs no file and no host device. H0 moves no sample:
        // gate A's wav capture arrives with H4, the arm that has something to
        // record.
        qemu.arg("-audiodev").arg("none,id=hdaaud");
        for dev in shape.hda {
            qemu.arg("-device").arg(*dev);
        }
    }

    if shape.virtio {
        qemu.arg("-netdev")
            .arg("user,id=net0")
            .arg("-device")
            .arg("virtio-net-pci-non-transitional,netdev=net0")
            // virtio-sound records everything the guest plays into a per-boot
            // wav for glitch analysis; timer-period matches the interactive
            // config in src/qemu.rs so test timing represents what users hear.
            .arg("-audiodev")
            .arg(format!(
                "wav,id=audio0,path={},timer-period=5000",
                audio_wav.display()
            ))
            .arg("-device")
            .arg("virtio-sound-pci,audiodev=audio0,streams=1")
            // virtio-console on stdio is the primary I/O channel; UART goes to
            // a temp file so early-boot logs and panic fallback still land
            // somewhere when the kernel switches backends.
            .arg("-serial")
            .arg(format!("file:{}", uart_log.display()))
            .arg("-chardev")
            .arg("stdio,id=cs0,signal=off")
            .arg("-device")
            .arg("virtio-serial-pci-non-transitional,id=virtio-serial0,max_ports=1")
            .arg("-device")
            .arg("virtconsole,chardev=cs0,id=console0");
    } else if options.mute {
        qemu.arg("-serial").arg("none");
    } else {
        // The 16550 *is* the console here: no virtio-serial exists, so the
        // kernel's log ring drains to it and the guest reads its commands
        // off it. signal=off matches the virtio console above, so a ^C in
        // the stream reaches the guest rather than killing QEMU.
        qemu.arg("-chardev")
            .arg("stdio,id=uart0,signal=off")
            .arg("-serial")
            .arg("chardev:uart0");
    }

    if options.gdb_stub {
        qemu.arg("-s");
    }
    if let Some(socket) = qmp_socket {
        qemu.arg("-qmp")
            .arg(format!("unix:{},server,nowait", socket.display()));
    }

    qemu
}

/// Every file one boot owns, so that adding another does not lengthen a
/// parameter list eight paths long.
struct Files {
    seq: u32,
    audio_wav: PathBuf,
    uart_log: PathBuf,
    nvme_image: PathBuf,
    usb_images: Vec<PathBuf>,
    qmp_socket: Option<PathBuf>,
    screendump: PathBuf,
    own_boot_image: Option<PathBuf>,
}

fn spawn_and_wait_ready(mut qemu: Command, options: &BootOptions, files: Files) -> QemuInstance {
    let Files {
        seq,
        audio_wav,
        uart_log,
        nvme_image,
        usb_images,
        qmp_socket,
        screendump,
        own_boot_image,
    } = files;

    qemu.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("[qemu {seq}] Launching QEMU...");
    }
    let mut child = qemu.spawn().expect("Failed to launch QEMU");

    let stdin = BufWriter::new(child.stdin.take().unwrap());
    let stdout = child.stdout.take().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let reader_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut full_log = String::new();
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    full_log.push_str(&line);
                    full_log.push('\n');
                    if VERBOSE.load(Ordering::Relaxed) {
                        // The boot's own number, because `--nocapture` on a
                        // wide run is several guests talking into one terminal
                        // and an unattributed line is worse than no line.
                        eprintln!("[serial {seq}] {line}");
                    }
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        full_log
    });

    // A muted guest has no console at all, so there is no marker to wait for:
    // the caller polls the framebuffer. Blocking here would only time out.
    let boot_log = if options.mute {
        String::new()
    } else {
        wait_for_ready(&mut child, &rx, options, &uart_log)
    };

    // Counted from here rather than from the spawn: every panic inside
    // `wait_for_ready` kills the child on its way out and never builds a value
    // to drop, so a guest that failed to come up must not be left on the books.
    LIVE.fetch_add(1, Ordering::SeqCst);
    QemuInstance {
        child,
        stdin,
        rx,
        _reader_thread: reader_thread,
        audio_wav,
        uart_log,
        nvme_image,
        usb_images,
        qmp_socket,
        screendump,
        own_boot_image,
        boot_log,
    }
}

/// Returns every line seen on the way to the marker — see [`QemuInstance::boot_log`].
fn wait_for_ready(
    child: &mut Child,
    rx: &Receiver<String>,
    options: &BootOptions,
    uart_log: &Path,
) -> String {
    let no_timeout = options.debug_wait;
    let ready = options.ready_marker;
    let panic_aborts = ready == DEFAULT_READY;
    // Ten seconds per guest this phase may have up, and never fewer than two
    // guests' worth — the tree runs 15-25 suites a day across several agents
    // (`specs/test-cost-audit.md` §4), so one guest on a quiet host stopped being
    // the regime some time before this did. Measured on 2026-08-03 with other
    // agents building: two boots exceeded the flat ten seconds, one of them in a
    // phase running a single guest.
    //
    // A wedge costs that much longer to report and nothing else. No test asserts
    // on how long a boot took by *this* clock: `i8042_absent` and
    // `xhci_slow_connect` do assert on boot timing and read the guest's own
    // stamps, and both are in the serial tail.
    let boot_timeout = Duration::from_secs(10) * WIDTH.load(Ordering::SeqCst).max(2);
    let start = Instant::now();
    let mut seen = String::new();
    loop {
        if !no_timeout && start.elapsed() > boot_timeout {
            let _ = child.kill();
            panic!("[qemu] Boot timed out waiting for {ready}");
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(line) if line.contains(ready) => {
                seen.push_str(&line);
                seen.push('\n');
                if VERBOSE.load(Ordering::Relaxed) {
                    eprintln!("[qemu] Reached {ready}");
                }
                break;
            }
            Ok(ref line)
                if panic_aborts
                    && !no_timeout
                    && (line.contains("SEGFAULT")
                        || line.contains("KERNEL PANIC")
                        || line.contains("!!! PANIC !!!")) =>
            {
                let mut crash_msg = line.clone();
                let drain_deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < drain_deadline {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(bt_line) => {
                            crash_msg.push('\n');
                            crash_msg.push_str(&bt_line);
                        }
                        Err(_) => break,
                    }
                }
                let _ = child.kill();
                panic!("[qemu] Init process crashed during boot:\n{crash_msg}");
            }
            Ok(line) => {
                seen.push_str(&line);
                seen.push('\n');
                continue;
            }
            // A guest that dies before virtio-console init never reaches
            // stdio at all; the UART file is the only channel it has.
            Err(RecvTimeoutError::Timeout) => {
                if !panic_aborts
                    && fs::read_to_string(uart_log).is_ok_and(|s| s.contains(ready))
                {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                let status = child.wait();
                panic!("[qemu] QEMU died before {ready} (status: {status:?})");
            }
        }
    }
    seen
}
