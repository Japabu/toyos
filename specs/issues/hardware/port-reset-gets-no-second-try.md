---
status: open
kind: defect
opened: 2026-08-02
---

# A port that fails its reset gets no second try, and no warm reset

xHCI 1.2 §4.19.5: "Only a USB3 protocol port may fail the bus reset sequence.
USB2 protocol ports never fail." A USB3 port that does fail comes back with
PLS = RxDetect, PRC set, the speed field zero and **CCS cleared** — so the
failure is distinguishable from success at the register, and `init_device`
distinguishes nothing: it checks PED, logs `reset but not enabled` and drops the
port for the life of the boot.

The spec's answer is §4.19.5.1, a Warm Port Reset: software writes WPR (bit 31)
instead of PR, which resets the USB3 link itself rather than only the device.
This driver never writes WPR, and `PORTSC_RWS` deliberately excludes it, so
there is no path to one. Linux retries either way — `PORT_RESET_TRIES` is 5 and
`PORT_INIT_TRIES` 4 in `drivers/usb/core/hub.c`, and `hub_port_reset` escalates
a failed hot reset to a warm one.

Doing this properly needs the Supported Protocol capability above, because
"retry as a warm reset" is only correct on a USB3 port and WPR is RsvdZ on a
USB2 one. It costs a device on a receptacle whose link does not train first
time; on the T14 the receptacle in question is the one the boot stick is in.
Nothing in QEMU can fail a reset — `xhci_port_reset` sets PED for every speed it
knows and never takes the failure path — so this needs an actuator of its own,
and `xhci-portsc-rw1c`'s shape (replace what the register reads) is the one that
fits.
