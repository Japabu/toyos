---
status: open
kind: finding
opened: 2026-08-01
---

# `/boot` exists only on a machine that boots from USB

`fat32_adapter::mount` resolves the `DeviceId` in `gpt::Volume` through
`usb_storage::open`, and there is no second arm. A machine that boots from an
internal disk has its NVMe taken by `page_cache::init` at storage time, and
there is no second handle to it — so `gpt::boot_volume()` would answer and the
mount would still refuse. Closing it means either a shared block-device handle
or moving the page cache off sole ownership; neither is a two-line change, and
the machine this project targets boots from a stick.
