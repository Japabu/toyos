use std::process::Command;

fn main() {
    Command::new("qemu-system-x86_64")
        .arg("-cpu").arg("qemu64")
        .arg("-drive").arg("if=pflash,format=raw,unit=0,file=ovmf/OVMF_CODE-pure-efi.fd,readonly=on")
        .arg("-drive").arg("if=pflash,format=raw,unit=1,file=ovmf/OVMF_VARS-pure-efi.fd")
        .arg("-net").arg("none")

        .arg("-drive").arg("format=raw,file=target/bootable.img")
        
        .spawn()
        .expect("failed to execute process");
}
