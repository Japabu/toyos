use std::fs::File;
use std::process::Command;

pub fn launch(debug: bool, dump_audio: bool) {
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
        .arg("cores=1")
        .arg("-m")
        .arg("2G")
        // Flash the OVMF UEFI firmware
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=0,file=ovmf/OVMF_CODE-pure-efi.fd,readonly=on")
        .arg("-drive")
        .arg("if=pflash,format=raw,unit=1,file=ovmf/OVMF_VARS-pure-efi.fd,readonly=on")
        // Create xHCI controller for USB
        .arg("-device")
        .arg("nec-usb-xhci,id=xhci")
        // Create a USB stick with the bootable image
        .arg("-drive")
        .arg("if=none,id=stick,format=raw,file=target/bootable.img")
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick,bootindex=0")
        // USB keyboard + mouse
        .arg("-device")
        .arg("usb-kbd,bus=xhci.0")
        .arg("-device")
        .arg("usb-tablet,bus=xhci.0")
        // NVMe SSD
        .arg("-drive")
        .arg("if=none,id=nvme0,format=raw,file=target/nvme.img")
        .arg("-device")
        .arg("nvme,serial=deadbeef,drive=nvme0")
        // VirtIO GPU (no legacy VGA)
        .arg("-vga")
        .arg("none")
        .arg("-device")
        .arg("virtio-gpu-pci,xres=1280,yres=720")
        // VirtIO networking with user-mode (SLIRP) backend
        .arg("-netdev")
        .arg("user,id=net0,hostfwd=tcp::2222-:22")
        .arg("-device")
        .arg("virtio-net-pci-non-transitional,netdev=net0");

    // VirtIO sound — wav file output for analysis or native audio for listening
    if dump_audio {
        eprintln!("Audio output: /tmp/toyos-audio.wav");
        qemu.arg("-audiodev")
            .arg("wav,id=audio0,path=/tmp/toyos-audio.wav");
    } else {
        qemu.arg("-audiodev").arg(format!(
            "{},id=audio0,timer-period=5000,out.buffer-length=20000",
            audio_backend()
        ));
    }
    qemu.arg("-device")
        .arg("virtio-sound-pci,audiodev=audio0,streams=1");

    qemu.arg("-serial")
        .arg("stdio")
        .arg("-no-reboot")
        // Enable gdb at port 1234
        .arg("-s")
        // QMP socket for programmatic control
        .arg("-qmp")
        .arg("unix:/tmp/toyos-qmp.sock,server,nowait")
        // Debug log — captures interrupts, exceptions, MMU faults, triple faults
        .arg("-d")
        .arg("int,cpu_reset")
        .arg("-D")
        .arg("/tmp/toyos-qemu-debug.log");

    if debug {
        eprintln!("Debug mode: kernel will wait for debugger before entering userland");
    }

    // Serial output goes to stdout (stdio), so keep stdout attached to terminal.
    // Capture QEMU's own stderr to a file for post-mortem analysis.
    let stderr_file = File::create("/tmp/toyos-qemu-stderr.log").expect("create stderr log");
    qemu.stderr(stderr_file);

    eprintln!("QEMU logs: /tmp/toyos-qemu-debug.log, /tmp/toyos-qemu-stderr.log");
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
