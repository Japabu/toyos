use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

/// The hardware shape QEMU presents to the guest.
///
/// Not a display setting: each variant is a whole machine. `Virtio` and `Gop`
/// differ only in how the framebuffer arrives, but `Metal` removes every
/// virtio device, which changes the console, the network, and audio too.
#[derive(Clone, Copy, PartialEq)]
pub enum Profile {
    /// virtio-gpu for the display, virtio-console for the console, plus
    /// virtio-net and virtio-sound. What every boot in this tree used to be.
    Virtio,
    /// `-vga std`, so firmware publishes a GOP and the kernel takes the
    /// laptop's display path. Every virtio device is still present.
    Gop,
    /// M1 metal-sim: the shape a ThinkPad T14 presents. Firmware framebuffer,
    /// i8042 (q35 gives it for free), NVMe, xHCI with the boot stick on it,
    /// and nothing else — no virtio device anywhere and no USB HID. The 16550
    /// stays on unless [`Options::mute`] takes it away: the T14 has no serial
    /// port, but every defect metal-sim has found came from the device shape,
    /// and a console is what makes the shape drivable.
    Metal,
}

/// Everything a profile decides about the machine, in one table. A new
/// variant answers every question here or does not compile — which `self !=
/// Profile::Metal` did the opposite of: it handed anything that was not
/// literally Metal the whole virtio block and a USB keyboard.
struct Shape {
    /// The display device. `Virtio` is the only profile with no firmware GOP.
    virtio_gpu: bool,
    /// virtio-net, virtio-sound, and the console on virtio-serial.
    virtio: bool,
    /// A USB HID on the xHCI. The T14's keyboard is PS/2 and its touchpad
    /// I2C-HID; it has no USB HID at all.
    usb_hid: bool,
    /// A vIOMMU, in the configuration `specs/iommu-spec.md` §9 stage I0 names:
    /// interrupt remapping on, caching mode on, 48-bit addresses. Every
    /// interactive profile has one, because the machine this project targets
    /// has one and a development shape without it is a shape where the
    /// kernel's discovery path never runs. The harness varies the unit's own
    /// capabilities; here there is nothing to vary against.
    iommu: bool,
}

impl Profile {
    fn shape(self) -> Shape {
        match self {
            Self::Virtio => Shape { virtio_gpu: true, virtio: true, usb_hid: true, iommu: true },
            Self::Gop => Shape { virtio_gpu: false, virtio: true, usb_hid: true, iommu: true },
            Self::Metal => Shape { virtio_gpu: false, virtio: false, usb_hid: false, iommu: true },
        }
    }
}

pub struct Options {
    pub debug: bool,
    pub dump_audio: bool,
    pub profile: Profile,
    pub smp: u32,
    /// Take `Profile::Metal`'s 16550 away, leaving the framebuffer as the
    /// only channel out — the T14's literal shape. Everything else about the
    /// machine is identical, so this is the observability question and not
    /// the device-shape one.
    pub mute: bool,
    /// The image to boot from, which `--diag-boot` moves off `bootable.img`.
    /// Passed rather than hardcoded so a diag build cannot launch the ordinary
    /// image and read the wrong screen.
    pub image: PathBuf,
}

pub fn launch(opts: &Options) {
    let shape = opts.profile.shape();
    let mut qemu = Command::new("qemu-system-x86_64");

    // Without this QEMU runs its default-device pass whenever no network
    // option is given, which is exactly and only the Metal profile: measured
    // on QEMU 11.0.2, an e1000e at 00:02.0 with a slirp backend, an empty
    // ide-cd on the ich9-ahci, and an isa-parallel. `-net none` and `-nic
    // none` are gone in QEMU 11; this is the option that does it, and it
    // leaves i8042/ps2-kbd/ps2-mouse alone.
    qemu.arg("-nodefaults");

    if toyos_build::kvm_usable() {
        qemu.arg("-accel").arg("kvm");
        qemu.arg("-cpu").arg("host,+rdrand,+smap,+fsgsbase,+x2apic");
    } else {
        qemu.arg("-cpu")
            .arg("qemu64,+rdrand,+smap,+fsgsbase,+x2apic");
    }

    qemu.arg("-machine")
        .arg(if shape.iommu { "q35,kernel-irqchip=split" } else { "q35" })
        .arg("-smp")
        .arg(format!("cores={}", opts.smp))
        .arg("-m")
        .arg("2G")
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=0,file=ovmf/OVMF_CODE-pure-efi.fd,readonly=on")
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=1,file=ovmf/OVMF_VARS-pure-efi.fd,readonly=on");

    // Before every other `-device`: a PCI function created ahead of the unit
    // gets QEMU's bypassing address space and is never decoded by it.
    if shape.iommu {
        qemu.arg("-device")
            .arg("intel-iommu,intremap=on,caching-mode=on,aw-bits=48");
    }

    qemu.arg("-device")
        .arg("nec-usb-xhci,id=xhci")
        .arg("-drive")
        .arg(format!(
            "if=none,id=stick,format=raw,file={}",
            opts.image.display()
        ))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick,bootindex=0")
        .arg("-drive")
        .arg("if=none,id=nvme0,format=raw,file=target/nvme.img")
        .arg("-device")
        .arg("nvme,serial=deadbeef,drive=nvme0");

    if shape.usb_hid {
        qemu.arg("-device")
            .arg("usb-kbd,bus=xhci.0")
            .arg("-device")
            .arg("usb-tablet,bus=xhci.0");
    }

    // `-vga std` is the display path a real laptop takes: firmware publishes a
    // linear framebuffer, the kernel maps it, and there is no virtio device to
    // fall back to. It is also the only config in which the on-screen panic
    // console renders anything.
    if shape.virtio_gpu {
        qemu.arg("-vga")
            .arg("none")
            .arg("-device")
            .arg("virtio-gpu-pci,xres=1280,yres=720");
    } else {
        qemu.arg("-vga").arg("std");
    }

    if shape.virtio {
        qemu.arg("-netdev")
            .arg("user,id=net0,hostfwd=tcp::2222-:22")
            .arg("-device")
            .arg("virtio-net-pci-non-transitional,netdev=net0");

        // VirtIO sound — wav file output for analysis or native audio for
        // listening. Both backends must keep the same host mixer timer-period,
        // or wav-based timing measurements stop representing what a user hears.
        if opts.dump_audio {
            eprintln!("Audio output: /tmp/toyos-audio.wav");
            qemu.arg("-audiodev")
                .arg("wav,id=audio0,path=/tmp/toyos-audio.wav,timer-period=5000");
        } else {
            qemu.arg("-audiodev").arg(format!(
                "{},id=audio0,timer-period=5000,out.buffer-length=20000",
                audio_backend()
            ));
        }
        qemu.arg("-device")
            .arg("virtio-sound-pci,audiodev=audio0,streams=1");

        // Console wiring: virtio-console on stdio is the primary I/O channel
        // (the kernel switches to it once virtio-console init completes —
        // see drivers/virtio_console.rs). UART stays on a file so early-boot
        // logs (before virtio is up) and panic fallback are still captured.
        qemu.arg("-serial")
            .arg("file:/tmp/toyos-uart-early.log")
            .arg("-chardev")
            .arg("stdio,id=cs0,signal=off")
            .arg("-device")
            .arg("virtio-serial-pci-non-transitional,id=virtio-serial0,max_ports=1")
            .arg("-device")
            .arg("virtconsole,chardev=cs0,id=console0");
    } else if opts.mute {
        eprintln!("metal-sim: no 16550 — the framebuffer is the only channel out");
        qemu.arg("-serial").arg("none");
    } else {
        qemu.arg("-serial").arg("stdio");
    }

    qemu.arg("-no-reboot")
        // Enable gdb at port 1234
        .arg("-s")
        // QMP socket for programmatic control
        .arg("-qmp")
        .arg("unix:/tmp/toyos-qmp.sock,server,nowait");

    if opts.debug {
        eprintln!("Debug mode: kernel will wait for debugger before entering userland");
        // Interrupt/exception log — formatting every interrupt on the vCPU
        // thread costs latency and writes hundreds of MB per session, so it
        // is debug-only.
        qemu.arg("-d")
            .arg("int,cpu_reset")
            .arg("-D")
            .arg("/tmp/toyos-qemu-debug.log");
        // A triple fault requests a reset; -no-reboot turns that into a
        // shutdown, and shutdown=pause parks QEMU instead of exiting so the
        // faulting CPU state stays inspectable via gdb/QMP.
        qemu.arg("-action").arg("shutdown=pause");
        eprintln!("QEMU interrupt log: /tmp/toyos-qemu-debug.log");
    }

    // Serial output goes to stdout (stdio), so keep stdout attached to terminal.
    // Capture QEMU's own stderr to a file for post-mortem analysis.
    let stderr_file = File::create("/tmp/toyos-qemu-stderr.log").expect("create stderr log");
    qemu.stderr(stderr_file);

    eprintln!("QEMU stderr log: /tmp/toyos-qemu-stderr.log");
    qemu.status().expect("failed to execute QEMU");
}

fn audio_backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "coreaudio"
    } else if cfg!(target_os = "linux") {
        "pipewire"
    } else if cfg!(target_os = "windows") {
        "dsound"
    } else {
        "none"
    }
}
