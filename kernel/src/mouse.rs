use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU16, AtomicU64, Ordering};
use crate::io_uring::RingId;
use crate::sync::Lock;
pub use toyos_abi::input::MouseEvent;

static MOUSE_BUF: Lock<VecDeque<MouseEvent>> = Lock::new(VecDeque::new());

/// How many pointer updates the kernel holds for a reader that is not reading.
///
/// The keyboard's bound and the keyboard's argument
/// ([`crate::keyboard::MAX_QUEUED_EVENTS`]), and the drop-oldest policy is
/// even clearer here: every event carries an absolute position, so the newest
/// is where the pointer *is* and the oldest is where it was.
pub const MAX_QUEUED_EVENTS: usize = 512;
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
///
/// Numbered as devices bind, and *not* derived from an xHCI slot id, which is
/// what it used to be: slot ids are per controller, and a Tiger Lake machine
/// has two controllers. A pointer on slot 1 of the Thunderbolt xHC and one on
/// slot 1 of the PCH xHC would be a single source — the aliasing this type
/// exists to prevent, restaged one level up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PointerSource(u8);

/// One bit per entry of `BUTTONS`, set while a device holds it. Bit 0 is the
/// i8042's aux port, which is claimed for as long as the machine exists.
///
/// A monotone counter is what this used to be, and a counter cannot be handed
/// back: a USB pointer that is unplugged and plugged in again took a fresh
/// entry each time, so after 255 of them `claim` returns `None` and the next
/// mouse is refused on a machine that has one pointer attached. A dock is that
/// many plug cycles in a working week.
static IN_USE: [AtomicU64; 4] = [
    AtomicU64::new(1),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

impl PointerSource {
    /// The i8042's aux port, of which a machine has at most one.
    pub const PS2: Self = Self(0);

    /// An entry in the button table for a pointer that is binding, or `None`
    /// when every entry is already held.
    ///
    /// A caller that cannot get one must not bind the device: handing it
    /// somebody else's entry is worse than not having it, because the other
    /// device's buttons then flap on every report.
    ///
    /// The lowest free entry, so the numbers a machine uses stay the numbers
    /// its pointers need — an entry [`unbind`] gave back is the one the next
    /// pointer takes.
    pub fn claim() -> Option<Self> {
        for (word, bits) in IN_USE.iter().enumerate() {
            let mut seen = bits.load(Ordering::Relaxed);
            while seen != u64::MAX {
                let bit = seen.trailing_ones();
                match bits.compare_exchange_weak(
                    seen,
                    seen | (1 << bit),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Some(Self((word as u32 * 64 + bit) as u8)),
                    Err(now) => seen = now,
                }
            }
        }
        None
    }

    /// Which entry of the button table this source publishes into. For a log
    /// line: two devices printing the same number is the aliasing defect, and
    /// nothing else in the system can show it.
    pub fn id(self) -> u8 {
        self.0
    }
}

/// One byte per source a `PointerSource` can name, so there is no allocation,
/// no eviction, and no way for two devices to alias. The OR runs on every
/// pointer event, at ~100 Hz.
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
    queue(MouseEvent { buttons: merged, scroll, abs_x, abs_y });
    true
}

fn queue(event: MouseEvent) {
    let mut buf = MOUSE_BUF.lock();
    if buf.len() >= MAX_QUEUED_EVENTS {
        buf.pop_front();
    }
    buf.push_back(event);
}

/// Throw away everything queued, for the reason
/// [`crate::keyboard::discard_queued`] gives.
pub fn discard_queued() {
    MOUSE_BUF.lock().clear();
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
    queue(MouseEvent {
        buttons: after,
        scroll: 0,
        abs_x: LAST_X.load(Ordering::Relaxed),
        abs_y: LAST_Y.load(Ordering::Relaxed),
    });
    true
}

/// A pointer that is gone: release whatever it was holding and give its entry
/// back. Returns whether the release changed the merge.
///
/// The counterpart to [`PointerSource::claim`] and the only thing that frees an
/// entry — it clears the buttons first, because an entry handed to the next
/// device with a byte still set publishes that byte as the new device's
/// buttons until its first report, which is the aliasing this type exists to
/// prevent one step removed.
///
/// Distinct from [`release_buttons`], which is for a pointer that is still
/// there and may report again: a quarantined controller keeps its source, an
/// unplugged device does not.
pub fn unbind(source: PointerSource) -> bool {
    assert!(
        source != PointerSource::PS2,
        "mouse: the i8042's aux port cannot be unplugged, and freeing entry 0 would let a USB \
         pointer alias it"
    );
    let published = release_buttons(source);
    IN_USE[source.0 as usize / 64].fetch_and(!(1u64 << (source.0 % 64)), Ordering::Relaxed);
    published
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
