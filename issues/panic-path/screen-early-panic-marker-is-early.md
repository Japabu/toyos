---
status: open
kind: defect
opened: 2026-08-03
---

# `screen_early_panic`'s ready marker is published one step before the screen it asserts on

`ready_marker` for that boot is `!!! EARLY PANIC !!!`, and the early branch of
`#[panic_handler]` (`kernel/src/main.rs:142`) does, in this order:

```
log!("!!! EARLY PANIC !!!: {}", info);   // into the ring
drivers::panic_console::capture();
unsafe { drivers::serial::panic_flush() };   // <- the harness stops waiting HERE
drivers::panic_console::render();            // <- the pixels it then asserts on
cpu::halt();
```

So the harness is released by the flush and may take its screendump before
`render()` — a full-screen MMIO blit of an 8x16 text grid — has put a glyph
anywhere. The failure is `"!!! EARLY PANIC !!!" not on screen` with a
**completely empty** decoded screen, which is that and not a rendering defect: a
render that ran and got the wrong glyphs would decode to something.

Measured at HEAD `6abed71`, one session, on a host shared with other agents:
**2 failures in 7 runs** (one inside a full suite, one isolated, five isolated
passes). It is not the concurrent-build window `issues/build/` describes — that one reports
as a `panicked at src/build.rs` and has no decoded screen at all — and it is not
the guest dying, which `screendump` reports separately.

The ordering itself is deliberate and should not move: the comment beside it
says the flush goes first so a fault inside the renderer "costs the screen and
never the serial report", which is the right trade on a machine with no
exception handlers yet. What is wrong is the *marker*: it names an event that
precedes the thing under test. The fix is a second line after `render()` for the
harness to wait on, or a screendump that retries until it decodes something.

Noticed while verifying #94's suite runs; nothing in the hotplug path can reach
it, since this boot panics at `main.rs:276` and `xhci::init` is at `main.rs:391`.
