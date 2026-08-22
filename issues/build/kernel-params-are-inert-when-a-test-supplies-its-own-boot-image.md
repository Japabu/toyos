---
status: open
kind: defect
opened: 2026-08-22
---

# `BootOptions::kernel_params` is silently inert when the test also sets `boot_image`

`QemuInstance::boot_with_options` takes `kernel_params` and `boot_image`
(`tests/common/qemu.rs:1714`, `:1744`). The parameters are baked into the *image*
— `build_boot_image` picks `TEST_KERNEL` when the list is non-empty
(`:1873-1882`) and writes the names into the boot config — and `boot_image`
replaces the image the boot would otherwise have built. So a test that sets both
boots the image it supplied, with whatever kernel and whatever parameters *that*
image was built with, and the `kernel_params` it passed do nothing at all.

Nothing refuses it and nothing says so. The harness's own summary line reports
the arm as if it took:

    --- 1 guests, 1 of them not the shipping kernel, 2 kernel build(s): ["", "boot-actuators,test-actuators"]

`kernel_of` (`:1889-1901`) selected the test kernel and the harness built it —
which is what that line counts — and then the guest booted `esp-boot.img`
instead.

Measured 2026-08-22 on `wt/toyos-fsync`. `esp_filesystem` builds its own image
(`tests/common/volumes.rs:242`, with an empty parameter list) and hands it over
at `:292`. Arming `usb-flush-fails` through `BootOptions::kernel_params` alone:
`PASS esp_filesystem (3s)`, no injected sense anywhere in the log. Arming it in
`build_boot_image` as well: `FAIL`, `usb-storage: SCSI 0x35 failed, sense
0x04/0x44/0x00`, `ALONE esp_filesystem: red again`. Same actuator, same call,
one of the two arms inert.

`usb_flush_optional` (`tests/common/usb.rs:857-875`) passes `PARAMS` to both and
is correct today; it reads as belt-and-braces rather than as the requirement it
is, which is how the trap survives. Every other `boot_image` caller in
`tests/common/` happens to pass no parameters, so nothing is currently
mis-armed — but the next test to try it gets a guest that quietly does not carry
what it asked for, and a green run proves nothing.

What is owed: make the two fields refuse each other unless the parameters agree
with the image's, the way `kernel_of` already refuses `kernel_params` together
with `kernel_features` (`:1893-1900`) — "a name is a name on this side of the
wire too, and the guest need not be started to know which kind it is" is that
assert's own argument, and it applies unchanged here. The image is built by the
same process moments earlier, so the parameter list it was built with is
knowable without asking the guest.
