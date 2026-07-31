use std::fs::File;
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
    /// and nothing else — no virtio device anywhere, no USB HID, and by
    /// default no 16550 either. The screen is the only channel out.
    Metal,
}

impl Profile {
    fn virtio(self) -> bool {
        self != Profile::Metal
    }
}

pub struct Options {
    pub debug: bool,
    pub dump_audio: bool,
    pub profile: Profile,
    pub smp: u32,
    /// Give `Profile::Metal` a 16550 on stdio anyway. The T14 has none, so a
    /// metal-sim run that uses this is a debugging aid and not a simulation:
    /// anything it proves has to be re-proved without it.
    pub uart: bool,
}

pub fn launch(opts: &Options) {
    let mut qemu = Command::new("qemu-system-x86_64");

    if kvm_available() {
        qemu.arg("-accel").arg("kvm");
        qemu.arg("-cpu").arg("host,+rdrand,+smap,+fsgsbase,+x2apic");
    } else {
        qemu.arg("-cpu")
            .arg("qemu64,+rdrand,+smap,+fsgsbase,+x2apic");
    }

    qemu.arg("-machine")
        .arg("q35")
        .arg("-smp")
        .arg(format!("cores={}", opts.smp))
        .arg("-m")
        .arg("2G")
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=0,file=ovmf/OVMF_CODE-pure-efi.fd,readonly=on")
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=1,file=ovmf/OVMF_VARS-pure-efi.fd,readonly=on")
        .arg("-device")
        .arg("nec-usb-xhci,id=xhci")
        .arg("-drive")
        .arg("if=none,id=stick,format=raw,file=target/bootable.img")
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick,bootindex=0")
        .arg("-drive")
        .arg("if=none,id=nvme0,format=raw,file=target/nvme.img")
        .arg("-device")
        .arg("nvme,serial=deadbeef,drive=nvme0");

    // The T14's own keyboard and touchpad are PS/2 and I2C-HID; it has no USB
    // HID at all. Leaving these in would let metal-sim pass on a device the
    // machine does not have.
    if opts.profile != Profile::Metal {
        qemu.arg("-device")
            .arg("usb-kbd,bus=xhci.0")
            .arg("-device")
            .arg("usb-tablet,bus=xhci.0");
    }

    // The GOP profile is the display path a real laptop takes: firmware
    // publishes a linear framebuffer, the kernel maps it, and there is no
    // virtio device to fall back to. It is also the only config in which the
    // on-screen panic console renders anything.
    if opts.profile == Profile::Virtio {
        qemu.arg("-vga")
            .arg("none")
            .arg("-device")
            .arg("virtio-gpu-pci,xres=1280,yres=720");
    } else {
        qemu.arg("-vga").arg("std");
    }

    if opts.profile.virtio() {
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
    } else if opts.uart {
        eprintln!("metal-sim: 16550 on stdio — a channel the T14 does not have");
        qemu.arg("-serial").arg("stdio");
    } else {
        qemu.arg("-serial").arg("none");
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

fn kvm_available() -> bool {
    cfg!(target_arch = "x86_64") && std::path::Path::new("/dev/kvm").exists()
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
