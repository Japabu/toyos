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
    let mut one = [0u8; 8];
    let report = [0u8, 0, 0x05, 0, 0, 0, 0, 0];
    assert_eq!(
        keyboard::handle_report(&mut one, &report),
        1,
        "input-merge: new report queued nothing"
    );
    assert_eq!(
        keyboard::handle_report(&mut one, &report),
        0,
        "input-merge: an unchanged report queued an event"
    );

    // A second HID keyboard, idle. Its report is a snapshot of *itself*, so it
    // says nothing about what the first one is holding — diffing the two
    // against one shared array released the held key and the next identical
    // report pressed it again, flapping at the combined polling rate.
    let mut two = [0u8; 8];
    assert_eq!(
        keyboard::handle_report(&mut two, &[0u8; 8]),
        0,
        "input-merge: an idle second keyboard released the first one's key"
    );
    assert_eq!(keyboard::handle_report(&mut one, &[0u8; 8]), 1, "input-merge: the release went missing");

    drain();

    // One pointer holds a button while the other reports none. Publishing
    // the second report's buttons verbatim is what made the button flap.
    let tablet = PointerSource::claim().expect("input-merge: no button-table entry for the tablet");
    let usb_mouse = PointerSource::claim().expect("input-merge: no button-table entry for the mouse");
    assert!(tablet != usb_mouse, "input-merge: two pointers claimed one source");
    assert!(
        mouse::handle_motion(PointerSource::PS2, 1, Motion::Relative { dx: 1, dy: 0 }, 0),
        "input-merge: PS/2 motion queued nothing"
    );
    assert!(
        mouse::handle_motion(tablet, 0, Motion::Absolute { x: 100, y: 100 }, 0),
        "input-merge: tablet motion queued nothing"
    );
    assert_eq!(
        last_button_state(),
        1,
        "input-merge: a tablet report with no buttons released the button the other pointer holds"
    );

    // And the same across two devices on the *same* bus, which is where a
    // bus-keyed source aliased them into one slot.
    assert!(
        mouse::handle_motion(tablet, 2, Motion::Absolute { x: 101, y: 100 }, 0),
        "input-merge: tablet button queued nothing"
    );
    assert!(
        mouse::handle_motion(usb_mouse, 0, Motion::Relative { dx: 1, dy: 0 }, 0),
        "input-merge: second USB pointer queued nothing"
    );
    assert_eq!(
        last_button_state(),
        3,
        "input-merge: one USB pointer's empty report cleared another USB pointer's button"
    );

    // Nothing but this can clear a source: a controller that quarantines with a
    // button down would otherwise hold it in the OR for the rest of the boot.
    assert!(mouse::release_buttons(tablet), "input-merge: releasing a held source queued nothing");
    assert_eq!(last_button_state(), 1, "input-merge: the tablet's button survived its release");
    assert!(
        !mouse::release_buttons(usb_mouse),
        "input-merge: releasing a source that held nothing queued an event"
    );
    assert!(mouse::release_buttons(PointerSource::PS2), "input-merge: PS/2 release queued nothing");
    assert_eq!(last_button_state(), 0, "input-merge: a button survived every release");

    drain();

    // Equal pixels per count on both axes. Relative motion accumulates in a
    // square 0..32767 space that the compositor maps by width and by height, so
    // one shared scalar skews the pointer by the aspect ratio — 1.6x on the
    // panel this milestone targets, and no per-source scalar can correct a
    // ratio that belongs to the screen. QEMU's framebuffer is square, so this
    // is the only place the arithmetic is exercised at all.
    for (w, h) in [(1920u32, 1200u32), (1024, 768), (3840, 2160), (1200, 1920), (800, 800)] {
        let (sx, sy) = mouse::rel_scale_for(w, h);
        assert!(sx >= 1 && sy >= 1, "input-merge: {w}x{h} scaled a count to nothing");
        let across = sx as u64 * w as u64;
        let down = sy as u64 * h as u64;
        // Each scale is a truncated integer, so each product carries under one
        // screen dimension of error.
        assert!(
            across.abs_diff(down) < (w + h) as u64,
            "input-merge: {w}x{h} moves {across} across and {down} down per count"
        );
    }

    log!("input-merge: ok");
}

/// The buttons the last queued pointer event published, draining the queue.
fn last_button_state() -> u8 {
    let mut last = None;
    while let Some(ev) = mouse::try_read_event() {
        last = Some(ev);
    }
    last.expect("input-merge: no pointer event").buttons
}
