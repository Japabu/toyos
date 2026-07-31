use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU16, Ordering};
use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::MouseEvent;

static MOUSE_BUF: Lock<VecDeque<MouseEvent>> = Lock::new(VecDeque::new());
static LAST_X: AtomicU16 = AtomicU16::new(0);
static LAST_Y: AtomicU16 = AtomicU16::new(0);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

/// Which physical pointer a report came from. Buttons are tracked per source
/// and published as their OR: with two live pointers a report carrying
/// buttons=0 from one of them would otherwise release a button the other is
/// holding, and the button flaps.
///
/// Keyed by device, not by bus. A USB boot mouse and a USB tablet are two
/// pointers and both are reachable on one machine, so a shared `Usb` slot was
/// the same defect with "the other USB pointer" in place of "PS/2".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointerSource(u8);

impl PointerSource {
    /// The i8042's aux port, of which a machine has at most one.
    pub const PS2: Self = Self(0);

    /// An xHCI device. Slot ids are 1-based, so they index the table directly
    /// and cannot collide with the PS/2 slot.
    pub fn usb(slot_id: u8) -> Self {
        assert!(slot_id != 0, "xHCI slot ids are 1-based");
        Self(slot_id)
    }
}

/// One byte per possible xHCI slot id plus the PS/2 slot, which is the whole
/// space a `PointerSource` can name — no allocation, no eviction, and no way
/// for two devices to alias. The OR runs on every pointer event, at ~100 Hz.
static BUTTONS: [AtomicU8; 256] = [const { AtomicU8::new(0) }; 256];

fn merged_buttons() -> u8 {
    BUTTONS.iter().fold(0, |acc, b| acc | b.load(Ordering::Relaxed))
}

/// Where a report puts the pointer. Absolute (USB tablet) and relative
/// (boot mouse, TrackPoint) accumulate into the same position because the
/// machine has one cursor.
#[derive(Clone, Copy, Debug)]
pub enum Motion {
    Relative { dx: i32, dy: i32 },
    Absolute { x: u16, y: u16 },
}

/// Relative counts scaled into the 0..32767 absolute space the compositor
/// consumes — per axis, because that space is square and screens are not.
///
/// The compositor maps x by width and y by height, so one shared scalar makes a
/// count worth `64*w/32768` px across and `64*h/32768` px down: 3.75 vs 2.34 on
/// a 1920x1200 panel, a fixed skew equal to the aspect ratio. Push the
/// TrackPoint at 45° and the cursor leaves at 32°. No per-source scalar can fix
/// it — a scalar multiplies both axes and the ratio is the *screen's*, not the
/// device's. Both axes get the finer of the two steps, so nothing gets coarser.
const REL_SCALE: i32 = 64;

static SCALE_X: AtomicU16 = AtomicU16::new(REL_SCALE as u16);
static SCALE_Y: AtomicU16 = AtomicU16::new(REL_SCALE as u16);

/// The per-axis scale a screen of this size wants. Pure, so the invariant that
/// matters — `x * width == y * height`, equal pixels per count — is checkable
/// without disturbing the live pointer.
pub fn rel_scale_for(width: u32, height: u32) -> (u16, u16) {
    let short = width.min(height) as i32;
    (
        (REL_SCALE * short / width.max(1) as i32).max(1) as u16,
        (REL_SCALE * short / height.max(1) as i32).max(1) as u16,
    )
}

/// Publish the geometry the absolute space is being mapped onto. Called
/// wherever the framebuffer is claimed or its mode changes; until then both
/// axes carry `REL_SCALE`, which is right for a square screen and no worse than
/// what a relative pointer had before one existed.
pub fn set_screen(width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    let (x, y) = rel_scale_for(width, height);
    SCALE_X.store(x, Ordering::Relaxed);
    SCALE_Y.store(y, Ordering::Relaxed);
    crate::log!("mouse: rel scale x={} y={} (screen {}x{})", x, y, width, height);
}

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

/// Wake every thread blocked on mouse input.
pub fn wake_waiters() {
    crate::sched::waitqs::wake_all(&crate::sched::waitqs::MOUSE);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

/// Queue one pointer update. Returns true iff an event was queued.
///
/// The single production path from a pointer of any kind into `MOUSE_BUF`:
/// it owns the per-source button merge and the relative→absolute
/// accumulation, so a second pointer cannot contradict the first.
pub fn handle_motion(
    source: PointerSource,
    buttons: u8,
    motion: Motion,
    scroll: i8,
) -> bool {
    let prev = BUTTONS[source.0 as usize].swap(buttons, Ordering::Relaxed);
    let merged = merged_buttons();

    let last_x = LAST_X.load(Ordering::Relaxed);
    let last_y = LAST_Y.load(Ordering::Relaxed);
    let (abs_x, abs_y) = match motion {
        Motion::Absolute { x, y } => (x, y),
        Motion::Relative { dx, dy } => (
            (last_x as i32 + dx * SCALE_X.load(Ordering::Relaxed) as i32).clamp(0, 32767) as u16,
            (last_y as i32 + dy * SCALE_Y.load(Ordering::Relaxed) as i32).clamp(0, 32767) as u16,
        ),
    };

    if abs_x == last_x && abs_y == last_y && scroll == 0 && buttons == prev {
        return false;
    }
    LAST_X.store(abs_x, Ordering::Relaxed);
    LAST_Y.store(abs_y, Ordering::Relaxed);
    MOUSE_BUF.lock().push_back(MouseEvent { buttons: merged, scroll, abs_x, abs_y });
    true
}

/// Release everything a source was holding, and publish the result.
///
/// The counterpart to `keyboard::release_all`, and the only thing that can
/// clear a source's bits other than a report from that source: a pointer that
/// goes away mid-drag — quarantined controller, ring overflow, aux written off
/// — otherwise holds its button in the OR for the rest of the boot, and every
/// other pointer's motion republishes it. That is a compositor stuck in a drag.
pub fn release_buttons(source: PointerSource) -> bool {
    let before = merged_buttons();
    BUTTONS[source.0 as usize].store(0, Ordering::Relaxed);
    let after = merged_buttons();
    if before == after {
        return false;
    }
    MOUSE_BUF.lock().push_back(MouseEvent {
        buttons: after,
        scroll: 0,
        abs_x: LAST_X.load(Ordering::Relaxed),
        abs_y: LAST_Y.load(Ordering::Relaxed),
    });
    true
}

/// Process a HID mouse/tablet report. Returns the number of events queued.
///
/// 6-byte tablet report: [buttons, x_lo, x_hi, y_lo, y_hi, scroll]
/// 3/4-byte boot mouse report: [buttons, dx, dy, scroll?]
pub fn handle_report(source: PointerSource, report: &[u8]) -> usize {
    let queued = if report.len() >= 6 {
        handle_motion(
            source,
            report[0],
            Motion::Absolute {
                x: u16::from_le_bytes([report[1], report[2]]),
                y: u16::from_le_bytes([report[3], report[4]]),
            },
            report[5] as i8,
        )
    } else if report.len() >= 3 {
        handle_motion(
            source,
            report[0],
            Motion::Relative { dx: report[1] as i8 as i32, dy: report[2] as i8 as i32 },
            if report.len() > 3 { report[3] as i8 } else { 0 },
        )
    } else {
        false
    };
    queued as usize
}

pub fn has_data() -> bool {
    !MOUSE_BUF.lock().is_empty()
}

pub fn try_read_event() -> Option<MouseEvent> {
    MOUSE_BUF.lock().pop_front()
}
