---
status: open
kind: finding
opened: 2026-08-03
---

# The T14's mouse may not have been this defect at all, and the next boot is what says

Fixed in passing and **unverifiable in this suite by construction**, so it is
recorded rather than claimed. The HID endpoint context's dword 4 was a flat `8`
copied from EP0's, where a control endpoint has no Max ESIT Payload and 8 is a
setup stage's Average TRB Length. Every periodic endpoint this driver configured
therefore declared that it moves **zero bytes per service interval** — the term
xHCI 1.2 §6.2.3.8 defines and §4.14.2 makes the periodic scheduler's input.
Linux's `xhci_endpoint_init` writes `max_packet` into both halves for a low- or
full-speed interrupt endpoint; the driver now does the same. QEMU has no
bandwidth scheduler and never reads the field, so no test here can tell the two
values apart.

That leaves two candidates for the 28 silent seconds, and they are
**distinguishable on the next metal boot**, which is why closing the first did
not close this:

1. the endpoint's first transfer completed with an error — the new line names
   the device, the endpoint and the code, and the recovery runs;
2. the endpoint was never scheduled at all — **no line, because no completion
   event ever arrives**, and the mouse is still silent.

Ruled out already: SET_PROTOCOL is sent to every boot-interface HID and the T14
log carries no failure line for it, so EP0 was not left halted (see the open
item on that in this section). The interval encoding is legal —
`bInterval=10` frames at low speed gives `log2(10 × 8) = 6`, inside Table 6-12's
3..10. `SET_IDLE` (HID 1.11 §7.2.4) is the one class request the enumeration
path does **not** send, where Linux's `usbhid_parse` sends it unconditionally
and ignores the result; its absence leaves the device on its default idle rate,
which is chattier and not silent, so it is not a candidate for this — but a
device that expects it is a real class of hardware and nothing here has one.
