# bootable

Top-level build orchestrator that compiles the kernel, bootloader, and all userland programs, assembles them into a bootable disk image with an initrd, and launches QEMU.

Depends on tyfs for initrd creation.
