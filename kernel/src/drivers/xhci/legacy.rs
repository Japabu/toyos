//! Taking the xHC away from firmware before resetting it (xHCI spec §4.22.1).
//!
//! A PC's firmware often keeps the USB controller for its own use — legacy
//! keyboard emulation, so a pre-boot menu works without a USB stack — and the
//! mechanism it uses is SMM: an SMI fires on the events named in USBLEGCTLSTS
//! and the handler drives the controller behind the OS's back. Resetting a
//! controller SMM still owns does not fail loudly. It fails the way the T14
//! fails: the firmware and the kernel take turns programming the same
//! registers, and the machine reaches userland with something dead and no line
//! anywhere saying why.
//!
//! **The capability list is firmware's, so it is untrusted input.** CLAUDE.md's
//! corollary applies literally: nothing here panics, nothing here loops on a
//! number firmware chose, and a list that makes no sense costs the handoff and
//! never the boot. The walk terminates for three independent reasons — the next
//! pointer is a strictly positive *forward* delta (spec §7.1.1: dwords from the
//! start of *this* capability, zero meaning end of list), every read is bounds
//! checked against the mapped window, and the iteration count is capped — and it
//! would still terminate with any *one* of them removed. Not any two: with the
//! window check and the cap both gone the forward delta alone climbs through
//! the u64 space, which is what the self-test's last case ("an endless chain
//! inside a window that never ends") is a demonstration of.
//!
//! QEMU cannot exercise any of this. Its xHC publishes no Legacy Support
//! capability and nothing owns the controller once OVMF's USB stack lets go at
//! ExitBootServices, so on the only machine in reach the handoff is a walk that
//! finds nothing. That is why the malformed lists have a self-test
//! (`xhci-xecp-selftest`) rather than a boot to point at.

use crate::log;
use crate::mm::Mmio;

/// Capability ID 1 — USB Legacy Support (spec §7.1.1).
const CAP_ID_LEGACY: u8 = 1;

/// USBLEGSUP bit 16: firmware owns the controller.
const LEGSUP_BIOS_OWNED: u32 = 1 << 16;
/// USBLEGSUP bit 24: the OS claims it.
const LEGSUP_OS_OWNED: u32 = 1 << 24;

/// USBLEGCTLSTS sits one dword past USBLEGSUP (spec §7.1.2).
const LEGCTLSTS: u64 = 4;
/// Its SMI enables: USB SMI, host-system-error, OS-ownership, PCI-command, BAR.
const SMI_ENABLES: u32 = (1 << 0) | (1 << 4) | (1 << 13) | (1 << 14) | (1 << 15);
/// Its write-1-to-clear SMI status bits: OS ownership change, PCI command, BAR.
const SMI_STATUS: u32 = (1 << 29) | (1 << 30) | (1 << 31);

/// How long firmware gets to let go.
///
/// A policy number and not a measurement — nothing in reach can make this wait,
/// and no Lenovo firmware has been timed. One second is the conventional
/// handoff wait: long enough that a machine which was going to release has,
/// short enough that one which never will costs the boot a second rather than
/// the controller.
const HANDOFF_TIMEOUT_NS: u64 = 1_000_000_000;

/// Capabilities the walk will visit before deciding the list is not a list.
/// Every shipping controller publishes a handful; the bound is here because
/// the number comes from firmware, not because any real one approaches it.
const MAX_CAPS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// A capability header that does not fit inside the mapped register
    /// window. The pointer that led here was firmware's.
    OutOfWindow(u64),
    /// More links than `MAX_CAPS`.
    TooMany,
}

/// Byte offset of the first capability with `id`, or `None` if the list has
/// none — walking from `xecp_dwords`, the HCCPARAMS1 field, in dwords from the
/// base of the register window.
///
/// `read` returns `None` for any offset whose dword lies outside that window,
/// which is what turns a firmware pointer into a refusal instead of a fault.
pub fn find(
    read: &dyn Fn(u64) -> Option<u32>,
    xecp_dwords: u32,
    id: u8,
) -> Result<Option<u64>, WalkError> {
    if xecp_dwords == 0 {
        return Ok(None);
    }
    let mut offset = xecp_dwords as u64 * 4;
    for _ in 0..MAX_CAPS {
        let Some(header) = read(offset) else {
            return Err(WalkError::OutOfWindow(offset));
        };
        if header as u8 == id {
            return Ok(Some(offset));
        }
        let next = (header >> 8) & 0xFF;
        if next == 0 {
            return Ok(None);
        }
        // At least one dword forward, at most 255 — so the offset strictly
        // increases and cannot wrap, and the window check above is what ends
        // the walk even if `MAX_CAPS` were raised to anything.
        offset += next as u64 * 4;
    }
    Err(WalkError::TooMany)
}

/// Ask firmware for the controller, and disable its SMIs whatever it answers.
///
/// Returns nothing and refuses nothing: the caller resets the controller either
/// way, because a machine that cannot boot is worse than one whose firmware is
/// fighting it. What this buys is that when the fight happens there is a line
/// naming it, which is the whole difference on a laptop with no serial port.
pub fn take_ownership(bar: &Mmio, bar_size: u64, hccparams1: u32) {
    #[cfg(feature = "xhci-xecp-selftest")]
    selftest();

    let xecp = hccparams1 >> 16;
    let read = |offset: u64| -> Option<u32> {
        (offset.checked_add(4)? <= bar_size).then(|| bar.read_u32(offset))
    };
    let legsup = match find(&read, xecp, CAP_ID_LEGACY) {
        Ok(Some(offset)) => offset,
        Ok(None) => {
            log!("xHCI: no USB Legacy Support capability (xECP={xecp:#x}), nothing to hand over");
            return;
        }
        Err(e) => {
            log!("xHCI: extended capability list unusable ({e:?}, xECP={xecp:#x}) — no handoff");
            return;
        }
    };
    // USBLEGCTLSTS is the dword after the one `find` bounds-checked.
    if legsup + LEGCTLSTS + 4 > bar_size {
        log!("xHCI: USB Legacy Support at {legsup:#x} runs past the register window — no handoff");
        return;
    }

    let before = bar.read_u32(legsup);
    bar.write_u32(legsup, before | LEGSUP_OS_OWNED);

    let started = crate::clock::nanos_since_boot();
    let deadline = started + HANDOFF_TIMEOUT_NS;
    let mut now = bar.read_u32(legsup);
    while now & LEGSUP_BIOS_OWNED != 0 && crate::clock::nanos_since_boot() < deadline {
        core::hint::spin_loop();
        now = bar.read_u32(legsup);
    }
    let waited_us = (crate::clock::nanos_since_boot() - started) / 1_000;

    if now & LEGSUP_BIOS_OWNED != 0 {
        // Not fatal, and deliberately so: proceeding gives a machine that may
        // work with a line saying what to suspect, and refusing gives one that
        // certainly does not boot.
        log!(
            "xHCI: firmware still owns the controller after {}ms (USBLEGSUP {before:#010x} -> {now:#010x}) — resetting it anyway",
            HANDOFF_TIMEOUT_NS / 1_000_000
        );
    } else if before & LEGSUP_BIOS_OWNED != 0 {
        log!("xHCI: firmware released the controller in {waited_us}us (USBLEGSUP {before:#010x} -> {now:#010x})");
    } else {
        log!("xHCI: firmware did not claim the controller (USBLEGSUP {before:#010x})");
    }

    // Whatever the semaphore said. Firmware that did not release is exactly the
    // firmware whose SMI handler is about to meet a reset it did not expect,
    // and firmware that never claimed ownership can still have SMI generation
    // armed — the two bits are independent.
    let ctl = bar.read_u32(legsup + LEGCTLSTS);
    bar.write_u32(legsup + LEGCTLSTS, (ctl & !SMI_ENABLES) | SMI_STATUS);
    let after = bar.read_u32(legsup + LEGCTLSTS);
    log!("xHCI: USBLEGCTLSTS {ctl:#010x} -> {after:#010x} (SMI generation off)");
}

/// The malformed lists no controller in reach can produce.
///
/// Every case here is a shape firmware can hand us and QEMU cannot: it
/// publishes no extended capabilities the walk can go wrong on, so without this
/// the bounds in [`find`] would ship having never been executed. Each expected
/// value is the *refusal*, not the answer — the property under test is that a
/// list which makes no sense costs the handoff and nothing else.
#[cfg(feature = "xhci-xecp-selftest")]
fn selftest() {
    /// A 16-dword register window, so a dword at or past 0x40 is outside it.
    const WINDOW: u64 = 64;
    const CASES: usize = 8;

    fn header(id: u8, next: u8) -> u32 {
        id as u32 | ((next as u32) << 8)
    }

    let mut passed = 0usize;
    let mut check = |name: &str,
                     read: &dyn Fn(u64) -> Option<u32>,
                     xecp: u32,
                     want: Result<Option<u64>, WalkError>| {
        let got = find(read, xecp, CAP_ID_LEGACY);
        if got == want {
            passed += 1;
        } else {
            log!("xHCI: xecp selftest FAILED on {name}: got {got:?}, want {want:?}");
        }
    };

    // Dword `n` of the window is at byte offset `4n`. A real controller's own
    // capability registers occupy the first few, so no list starts at zero.
    fn windowed(cells: &[u32; 16]) -> impl Fn(u64) -> Option<u32> + '_ {
        move |offset: u64| -> Option<u32> {
            if offset % 4 != 0 || offset.checked_add(4)? > WINDOW {
                return None;
            }
            Some(cells[(offset / 4) as usize])
        }
    }

    check("no list at all", &windowed(&[0; 16]), 0, Ok(None));

    let mut one = [0u32; 16];
    one[4] = header(2, 0);
    check("one capability, not ours", &windowed(&one), 4, Ok(None));

    let mut third = [0u32; 16];
    third[4] = header(2, 2);
    third[6] = header(2, 4);
    third[10] = header(CAP_ID_LEGACY, 0);
    check("ours, third in the list", &windowed(&third), 4, Ok(Some(40)));

    let mut last = [0u32; 16];
    last[4] = header(2, 11);
    last[15] = header(CAP_ID_LEGACY, 0);
    check("ours in the last dword there is", &windowed(&last), 4, Ok(Some(60)));

    check(
        "xECP points outside the window",
        &windowed(&[0; 16]),
        64,
        Err(WalkError::OutOfWindow(256)),
    );

    let mut jump = [0u32; 16];
    jump[4] = header(2, 255);
    check(
        "a link that leaves the window",
        &windowed(&jump),
        4,
        Err(WalkError::OutOfWindow(16 + 1020)),
    );

    // What an unmapped or powered-down region reads as. id 0xFF matches
    // nothing and next 0xFF walks off the end, which is the refusal we want
    // rather than 16384 reads of a dead bus.
    check(
        "a window reading all ones",
        &windowed(&[u32::MAX; 16]),
        4,
        Err(WalkError::OutOfWindow(16 + 1020)),
    );

    // The pathological *conformant* list: every link one dword forward, which
    // is as slowly as the encoding permits, over a window large enough that
    // the bounds check never fires. Only the iteration cap can end this one,
    // which is the point — it is the bound that has no other backstop.
    check(
        "an endless chain inside a window that never ends",
        &|_| Some(header(2, 1)),
        4,
        Err(WalkError::TooMany),
    );

    log!("xHCI: xecp selftest {passed}/{CASES} malformed lists refused");
}
