use std::process::Command;

fn main() {
    Command::new("qemu-system-x86_64")
        .arg("-cpu").arg("qemu64")
        .arg("-m").arg("1G")
        // Flash the OVMF UEFI firmware
        .arg("-drive").arg("if=pflash,format=raw,unit=0,file=ovmf/OVMF_CODE-pure-efi.fd,readonly=on")
        .arg("-drive").arg("if=pflash,format=raw,unit=1,file=ovmf/OVMF_VARS-pure-efi.fd,readonly=on")

        // Create xHCI controller for USB
        .arg("-device").arg("nec-usb-xhci,id=xhci")

        // Create a USB stick with the bootable image
        .arg("-drive").arg("if=none,id=stick,format=raw,file=target/bootable.img")
        .arg("-device").arg("usb-storage,bus=xhci.0,drive=stick")
        
        .arg("-serial").arg("stdio")

        .arg("-no-reboot")

        // Enable gdb at port 1234
        .arg("-s")

        .status()
        .expect("failed to execute process");
}
