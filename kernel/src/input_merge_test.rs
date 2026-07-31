//! `test-input-merge`: the only place the two-keyboard / two-pointer merge
//! can be exercised at all.
//!
//! QEMU activates one input handler per device class, so a USB keyboard and
//! a PS/2 keyboard can never both be live in a guest — the merge is a
//! kernel-internal invariant and this is a kernel-internal check. Nothing
//! here crossed a trust boundary, so a mismatch panics.

use crate::keyboard::{self, RawKeyEvent, MOD_RELEASED, MOD_SHIFT};
use crate::mouse::{self, Motion, PointerSource};

fn next_key(what: &str) -> RawKeyEvent {
    keyboard::try_read_event().unwrap_or_else(|| panic!("input-merge: no event for {what}"))
}

fn drain() {
    while keyboard::try_read_event().is_some() {}
    while mouse::try_read_event().is_some() {}
}

pub fn run() {
    drain();

    // Shift on one keyboard, the letter on another. The capital is the
    // observable form of "the modifier state is not per-driver".
    assert!(keyboard::handle_key(0xE1, true), "input-merge: Shift down queued nothing");
    assert!(keyboard::handle_key(0x04, true), "input-merge: 'a' down queued nothing");
    assert!(keyboard::handle_key(0xE1, false), "input-merge: Shift up queued nothing");
    // A make with no intervening break is what PS/2 typematic repeat looks
    // like, and what an unchanged USB report looks like after diffing.
    assert!(!keyboard::handle_key(0x04, true), "input-merge: repeat make queued an event");

    let shift_down = next_key("Shift down");
    assert!(
        shift_down.keycode == 0xE1 && shift_down.modifiers & MOD_RELEASED == 0,
        "input-merge: {:#x} mods {:#x} is not Shift down",
        shift_down.keycode,
        shift_down.modifiers
    );
    let letter = next_key("'a' down");
    assert!(
        letter.modifiers & MOD_SHIFT != 0,
        "input-merge: the letter did not see the other keyboard's Shift"
    );
    assert!(
        &letter.translated[..letter.len as usize] == b"A",
        "input-merge: Shift+a translated to {:?}, not a capital",
        &letter.translated[..letter.len as usize]
    );
    let shift_up = next_key("Shift up");
    assert!(
        shift_up.keycode == 0xE1 && shift_up.modifiers & MOD_RELEASED != 0,
        "input-merge: {:#x} mods {:#x} is not Shift up",
        shift_up.keycode,
        shift_up.modifiers
    );

    // A device that resets behind our back leaves its keys down; this is
    // what stops a modifier sticking for the rest of the boot.
    assert_eq!(keyboard::release_all(), 1, "input-merge: release_all missed the held key");
    assert!(
        next_key("release_all").modifiers & MOD_RELEASED != 0,
        "input-merge: release_all queued a press"
    );
    assert_eq!(keyboard::modifiers(), 0, "input-merge: a modifier survived release_all");

    // The wake guard's whole premise: an unchanged report queues nothing, so
    // its caller has something to test before waking anyone.
    let report = [0u8, 0, 0x05, 0, 0, 0, 0, 0];
    assert_eq!(keyboard::handle_report(&report), 1, "input-merge: new report queued nothing");
    assert_eq!(
        keyboard::handle_report(&report),
        0,
        "input-merge: an unchanged report queued an event"
    );
    assert_eq!(keyboard::handle_report(&[0u8; 8]), 1, "input-merge: the release went missing");

    drain();

    // One pointer holds a button while the other reports none. Publishing
    // the second report's buttons verbatim is what made the button flap.
    assert!(
        mouse::handle_motion(PointerSource::Ps2, 1, Motion::Relative { dx: 1, dy: 0 }, 0),
        "input-merge: PS/2 motion queued nothing"
    );
    assert!(
        mouse::handle_motion(PointerSource::Usb, 0, Motion::Absolute { x: 100, y: 100 }, 0),
        "input-merge: tablet motion queued nothing"
    );
    let mut last = None;
    while let Some(ev) = mouse::try_read_event() {
        last = Some(ev);
    }
    let last = last.expect("input-merge: no pointer event");
    assert_eq!(
        last.buttons, 1,
        "input-merge: a tablet report with no buttons released the button the other pointer holds"
    );

    mouse::handle_motion(PointerSource::Ps2, 0, Motion::Relative { dx: 0, dy: 0 }, 0);
    mouse::handle_motion(PointerSource::Usb, 0, Motion::Absolute { x: 0, y: 0 }, 0);
    drain();

    log!("input-merge: ok");
}
