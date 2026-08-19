use toyos_abi::input::{MouseEvent, RawKeyEvent};

use crate::hit::{hit_test, Hit};
use crate::layout::{set_mode, Desk};
use crate::rect::{Point, Rect};
use crate::stack::Stack;
use crate::window::{WindowId, WindowMode};

/// HID usage codes for the keys the desktop keeps for itself.
///
/// USB HID Usage Tables, Keyboard/Keypad page. The kernel delivers usages and
/// never characters, so these are the same numbers on every layout — which is
/// the point: Super+Q closes a window on a Swiss German keyboard too.
mod usage {
    pub const N: u8 = 0x11;
    pub const Q: u8 = 0x14;
    pub const V: u8 = 0x19;
    pub const ESCAPE: u8 = 0x29;
    pub const TAB: u8 = 0x2B;
    pub const RIGHT: u8 = 0x4F;
    pub const LEFT: u8 = 0x50;
    pub const DOWN: u8 = 0x51;
    pub const UP: u8 = 0x52;
}

/// What a key transition means to the desktop.
///
/// [`Forward`](Self::Forward) is the default and the important one: the
/// compositor translates nothing, so all it decides is whether a transition is
/// its own or the focused window's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    /// Hand the whole transition to the focused window, if there is one.
    Forward,
    /// The desktop consumed it and nothing changed.
    Ignore,
    CloseLauncher,
    CycleFocus,
    /// Put the focused window into this mode; [`WindowMode::Normal`] restores
    /// it to where it came from.
    SetMode(WindowMode),
    Minimize,
    CloseFocused,
    Paste,
    SpawnTerminal,
}

/// What `ev` does, given the focused window's mode and whether the launcher is
/// showing.
///
/// `focused` is `None` when nothing has the keyboard, and every combination
/// that acts on a window is [`Ignore`](KeyAction::Ignore) then — a desktop with
/// no windows must not close one.
///
/// Releases are forwarded whatever they are. A chord is a press; a client that
/// saw the press of a key it is holding needs the release of it too, and
/// swallowing that leaves the window believing the key is still down.
pub fn key_action(ev: RawKeyEvent, focused: Option<WindowMode>, launcher_open: bool) -> KeyAction {
    if !ev.pressed() {
        return KeyAction::Forward;
    }
    if launcher_open && ev.keycode == usage::ESCAPE {
        return KeyAction::CloseLauncher;
    }
    if ev.alt() && ev.keycode == usage::TAB {
        return KeyAction::CycleFocus;
    }
    if ev.gui() {
        let Some(mode) = focused else {
            return KeyAction::Ignore;
        };
        let toggle = |wanted: WindowMode| {
            KeyAction::SetMode(if mode == wanted { WindowMode::Normal } else { wanted })
        };
        return match ev.keycode {
            usage::LEFT => toggle(WindowMode::SnappedLeft),
            usage::RIGHT => toggle(WindowMode::SnappedRight),
            usage::UP => toggle(WindowMode::Maximized),
            usage::DOWN => {
                if mode == WindowMode::Normal {
                    KeyAction::Minimize
                } else {
                    KeyAction::SetMode(WindowMode::Normal)
                }
            }
            usage::Q => KeyAction::CloseFocused,
            usage::V => KeyAction::Paste,
            _ => KeyAction::Forward,
        };
    }
    if ev.ctrl() && ev.keycode == usage::N {
        return KeyAction::SpawnTerminal;
    }
    KeyAction::Forward
}

/// One report off the mouse fd.
pub const MOUSE_EVENT_LEN: usize = 6;

const _: () = {
    assert!(core::mem::size_of::<MouseEvent>() == MOUSE_EVENT_LEN);
    assert!(core::mem::offset_of!(MouseEvent, buttons) == 0);
    assert!(core::mem::offset_of!(MouseEvent, scroll) == 1);
    assert!(core::mem::offset_of!(MouseEvent, abs_x) == 2);
    assert!(core::mem::offset_of!(MouseEvent, abs_y) == 4);
};

fn decode(b: &[u8]) -> MouseEvent {
    MouseEvent {
        buttons: b[0],
        scroll: b[1] as i8,
        abs_x: u16::from_le_bytes([b[2], b[3]]),
        abs_y: u16::from_le_bytes([b[4], b[5]]),
    }
}

/// A whole read of the mouse fd, folded into the one thing that happened.
///
/// The kernel queues reports and one read drains them, so a batch can contain
/// a press and its release, several scroll notches and a dozen positions. The
/// desktop acts on the *transitions* and the last position: replaying every
/// intermediate one would hit-test against a pointer that is no longer there.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MouseSample {
    pub reports: usize,
    pub buttons: u8,
    /// The left button went down at some point in the batch.
    pub pressed: bool,
    /// And came up at some point in the batch.
    pub released: bool,
    pub scroll: i32,
    pub abs_x: u16,
    pub abs_y: u16,
}

impl MouseSample {
    pub fn left_held(&self) -> bool {
        self.buttons & 1 != 0
    }
}

/// Fold a read of the mouse fd, given what the buttons were before it.
///
/// A trailing partial report is ignored rather than being decoded from
/// whatever follows it in the buffer.
pub fn fold_mouse(prev_buttons: u8, bytes: &[u8]) -> MouseSample {
    let mut s = MouseSample { buttons: prev_buttons, ..MouseSample::default() };
    for chunk in bytes.chunks_exact(MOUSE_EVENT_LEN) {
        let ev = decode(chunk);
        s.reports += 1;
        s.scroll += ev.scroll as i32;
        s.abs_x = ev.abs_x;
        s.abs_y = ev.abs_y;
        let was = s.buttons & 1 != 0;
        let now = ev.buttons & 1 != 0;
        s.pressed |= now && !was;
        s.released |= was && !now;
        s.buttons = ev.buttons;
    }
    s
}

/// Full scale of the absolute axes the USB tablet reports on.
const ABS_RANGE: i32 = 32768;

/// Where a tablet's absolute position lands on the screen.
pub fn cursor_from_abs(abs_x: u16, abs_y: u16, screen: Rect) -> Point {
    Point {
        x: (screen.x0 + abs_x as i32 * screen.w() / ABS_RANGE)
            .clamp(screen.x0, (screen.x1 - 1).max(screen.x0)),
        y: (screen.y0 + abs_y as i32 * screen.h() / ABS_RANGE)
            .clamp(screen.y0, (screen.y1 - 1).max(screen.y0)),
    }
}

/// How far the pointer must travel before a press on a maximized title bar is
/// a drag rather than a click.
pub const DRAG_THRESHOLD: i32 = 5;

/// How close to an edge a drag must end for the window to snap against it.
const SNAP_MARGIN: i32 = 3;

/// What the pointer is doing to a window between a press and its release.
///
/// Named by [`WindowId`] and not by position: a drag or a resize outlives
/// many event-loop passes, and a client can close — its own window, or
/// nothing to do with it — on any pass in between. A position taken when the
/// grab started is stale the moment that happens; an id is re-resolved
/// against the stack on every use instead (`Stack::position`), so a window
/// that is gone is *found* to be gone rather than silently standing in for
/// whatever slid into its old slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Grab {
    #[default]
    None,
    /// Pressed on the title bar of a window that is maximized or snapped.
    ///
    /// It keeps its mode until the pointer has actually travelled, so a click
    /// that only focuses a maximized window does not un-maximize it. A normal
    /// window has nothing to defer and goes straight to
    /// [`Dragging`](Self::Dragging), which is why this carries no "was it
    /// maximized" flag: it exists only in the case where the answer is yes.
    Pending { window: WindowId, start: Point },
    Dragging { window: WindowId },
    Resizing { window: WindowId },
}

/// What one pointer sample did while the button was held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Held {
    /// Nothing is grabbed — the pointer is moving over the desktop.
    Free,
    /// Grabbed, and this sample changed nothing.
    Idle,
    /// The window left its mode under the pointer. Its content rect is the
    /// size it needs a buffer for.
    Restored { window: usize },
    /// It moved or resized inside the buffer it already has. `from` and `to`
    /// are its frames, and both are damage: a window that jumped clear of
    /// where it was exposes exactly `from`.
    Moved { window: usize, from: Rect, to: Rect },
}

/// What releasing the button did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Released {
    Nothing,
    /// The drag ended against an edge, so the window takes that mode.
    Snapped { window: usize, mode: WindowMode },
    /// A drag-resize ended, so the buffer has to catch up with the window.
    Resized { window: usize },
}

impl Grab {
    /// What a press on a window's title bar starts.
    pub fn on_title(window: WindowId, mode: WindowMode, at: Point) -> Self {
        if mode == WindowMode::Normal {
            Self::Dragging { window }
        } else {
            Self::Pending { window, start: at }
        }
    }

    /// Advance the grab by one pointer sample with the button still down.
    ///
    /// A name that no longer resolves — the window closed on some earlier
    /// pass — ends the grab and reports the sample as free, rather than
    /// indexing a position that may not exist, or may now belong to a window
    /// that never asked to move.
    pub fn hold<C>(
        &mut self,
        desk: &Desk,
        stack: &mut Stack<C>,
        at: Point,
        delta: Point,
    ) -> Held {
        match *self {
            Self::None => Held::Free,
            Self::Pending { window, start } => {
                let Some(idx) = stack.position(window) else {
                    *self = Self::None;
                    return Held::Free;
                };
                if (at.x - start.x).abs() <= DRAG_THRESHOLD
                    && (at.y - start.y).abs() <= DRAG_THRESHOLD
                {
                    return Held::Idle;
                }
                let old_frame_w = stack[idx].frame(&desk.chrome).w();
                let restored = set_mode(desk, stack, idx, WindowMode::Normal);
                stack[idx].content =
                    desk.chrome.restore_under_pointer(restored, start.x, old_frame_w, at);
                *self = Self::Dragging { window };
                Held::Restored { window: idx }
            }
            Self::Dragging { window } => {
                let Some(idx) = stack.position(window) else {
                    *self = Self::None;
                    return Held::Free;
                };
                let from = stack[idx].frame(&desk.chrome);
                stack[idx].content = desk.chrome.drag_to(stack[idx].content, delta.x, delta.y);
                Held::Moved { window: idx, from, to: stack[idx].frame(&desk.chrome) }
            }
            Self::Resizing { window } => {
                let Some(idx) = stack.position(window) else {
                    *self = Self::None;
                    return Held::Free;
                };
                let from = stack[idx].frame(&desk.chrome);
                stack[idx].content =
                    desk.chrome.resize_to(stack[idx].content, delta.x, delta.y);
                Held::Moved { window: idx, from, to: stack[idx].frame(&desk.chrome) }
            }
        }
    }

    /// End the grab, and say what the desktop owes the window.
    ///
    /// `Released::Nothing` is also the answer for a window that is no longer
    /// in `stack`: there is nothing left to snap or resettle, and the grab is
    /// discarded either way.
    pub fn release<C>(&mut self, desk: &Desk, stack: &Stack<C>, at: Point) -> Released {
        let was = core::mem::take(self);
        match was {
            Self::None | Self::Pending { .. } => Released::Nothing,
            Self::Dragging { window } => {
                let Some(idx) = stack.position(window) else { return Released::Nothing };
                match edge_snap(desk, at) {
                    Some(mode) => Released::Snapped { window: idx, mode },
                    None => Released::Nothing,
                }
            }
            Self::Resizing { window } => match stack.position(window) {
                Some(idx) => Released::Resized { window: idx },
                None => Released::Nothing,
            },
        }
    }
}

/// Which mode a window dragged to `at` and let go takes.
///
/// The vertical edge is tested before the horizontal one, so a pointer in the
/// top-left corner snaps left and does not maximize: a user aiming at the left
/// edge overshoots upward far more often than one aiming at the top overshoots
/// sideways.
pub fn edge_snap(desk: &Desk, at: Point) -> Option<WindowMode> {
    let s = desk.screen;
    if at.x - s.x0 < SNAP_MARGIN {
        Some(WindowMode::SnappedLeft)
    } else if s.x1 - at.x <= SNAP_MARGIN {
        Some(WindowMode::SnappedRight)
    } else if at.y - s.y0 < SNAP_MARGIN {
        Some(WindowMode::Maximized)
    } else {
        None
    }
}

/// What clicking a window's taskbar tab does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabAction {
    /// It was minimized: bring it back and to the front.
    Reveal,
    /// It already had the focus: put it away.
    Minimize,
    /// It was behind something: bring it forward.
    Raise,
}

pub fn tab_action<C>(stack: &Stack<C>, idx: usize) -> TabAction {
    if stack[idx].minimized {
        TabAction::Reveal
    } else if stack.focused() == Some(idx) {
        TabAction::Minimize
    } else {
        TabAction::Raise
    }
}

/// Which pointer sprite the desktop shows.
///
/// A window's own choice is decoded into this at the syscall boundary, so a
/// client that sends a style number nobody implements gets the default rather
/// than an index into something.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CursorStyle {
    #[default]
    Default,
    Crosshair,
    Resize,
}

/// Which sprite belongs under the pointer at `at`.
///
/// A resize in progress keeps the resize cursor wherever the pointer has got
/// to, because a drag that left the corner is still a drag.
pub fn cursor_style<C>(
    desk: &Desk,
    stack: &Stack<C>,
    grab: &Grab,
    at: Point,
    launcher_open: bool,
) -> CursorStyle {
    if matches!(grab, Grab::Resizing { .. }) {
        return CursorStyle::Resize;
    }
    match hit_test(desk, stack, at, launcher_open) {
        Hit::ResizeCorner(_) => CursorStyle::Resize,
        // The window under the pointer, not the focused one: a crosshair
        // belongs to the surface it is over.
        Hit::Content(idx) => stack[idx].cursor_style,
        _ => CursorStyle::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Chrome;
    use crate::window::Window;
    use alloc::string::ToString;
    use toyos_abi::input::{MOD_ALT, MOD_CTRL, MOD_GUI, MOD_RELEASED};

    const DESK: Desk = Desk {
        chrome: Chrome::DEFAULT,
        screen: Rect::new(0, 0, 1920, 1080),
        font_w: 8,
        apps: 2,
    };

    fn key(keycode: u8, modifiers: u8) -> RawKeyEvent {
        RawKeyEvent { keycode, modifiers }
    }

    fn stack_of(n: usize) -> Stack<usize> {
        let mut s = Stack::default();
        for i in 0..n {
            s.insert(Window::new(
                i,
                Rect::new(100 + 20 * i as i32, 100 + 20 * i as i32, 400, 300),
                "w".to_string(),
                false,
                CursorStyle::Default,
            ));
        }
        s
    }

    #[test]
    fn a_release_is_always_the_window_s() {
        for code in [usage::Q, usage::LEFT, usage::TAB, usage::ESCAPE] {
            let ev = key(code, MOD_GUI | MOD_ALT | MOD_RELEASED);
            assert_eq!(key_action(ev, Some(WindowMode::Normal), true), KeyAction::Forward);
        }
    }

    #[test]
    fn escape_closes_the_launcher_only_while_it_is_open() {
        let esc = key(usage::ESCAPE, 0);
        assert_eq!(key_action(esc, None, true), KeyAction::CloseLauncher);
        assert_eq!(key_action(esc, Some(WindowMode::Normal), false), KeyAction::Forward);
    }

    #[test]
    fn the_super_chords_toggle_against_the_mode_the_window_is_in() {
        let cases = [
            (usage::LEFT, WindowMode::Normal, WindowMode::SnappedLeft),
            (usage::LEFT, WindowMode::SnappedLeft, WindowMode::Normal),
            (usage::LEFT, WindowMode::SnappedRight, WindowMode::SnappedLeft),
            (usage::RIGHT, WindowMode::Normal, WindowMode::SnappedRight),
            (usage::RIGHT, WindowMode::SnappedRight, WindowMode::Normal),
            (usage::UP, WindowMode::Normal, WindowMode::Maximized),
            (usage::UP, WindowMode::Maximized, WindowMode::Normal),
            (usage::DOWN, WindowMode::Maximized, WindowMode::Normal),
        ];
        for (code, from, want) in cases {
            assert_eq!(
                key_action(key(code, MOD_GUI), Some(from), false),
                KeyAction::SetMode(want),
                "{code:#x} from {from:?}"
            );
        }
        assert_eq!(
            key_action(key(usage::DOWN, MOD_GUI), Some(WindowMode::Normal), false),
            KeyAction::Minimize
        );
    }

    #[test]
    fn no_window_means_no_super_chord_does_anything() {
        for code in [usage::LEFT, usage::RIGHT, usage::UP, usage::DOWN, usage::Q, usage::V] {
            assert_eq!(key_action(key(code, MOD_GUI), None, false), KeyAction::Ignore);
        }
    }

    #[test]
    fn an_unclaimed_super_chord_reaches_the_window() {
        assert_eq!(
            key_action(key(0x04, MOD_GUI), Some(WindowMode::Normal), false),
            KeyAction::Forward
        );
    }

    #[test]
    fn alt_tab_cycles_even_with_nothing_focused_and_ctrl_n_spawns() {
        assert_eq!(key_action(key(usage::TAB, MOD_ALT), None, false), KeyAction::CycleFocus);
        assert_eq!(key_action(key(usage::N, MOD_CTRL), None, false), KeyAction::SpawnTerminal);
        // Without the modifier both are the window's.
        assert_eq!(
            key_action(key(usage::TAB, 0), Some(WindowMode::Normal), false),
            KeyAction::Forward
        );
        assert_eq!(key_action(key(usage::N, 0), Some(WindowMode::Normal), false), KeyAction::Forward);
    }

    fn report(buttons: u8, scroll: i8, x: u16, y: u16) -> [u8; 6] {
        let mut b = [0u8; 6];
        b[0] = buttons;
        b[1] = scroll as u8;
        b[2..4].copy_from_slice(&x.to_le_bytes());
        b[4..6].copy_from_slice(&y.to_le_bytes());
        b
    }

    #[test]
    fn a_batch_folds_to_its_transitions_and_its_last_position() {
        let mut buf = [0u8; 18];
        buf[0..6].copy_from_slice(&report(0, 1, 100, 100));
        buf[6..12].copy_from_slice(&report(1, 1, 200, 200));
        buf[12..18].copy_from_slice(&report(0, -1, 300, 400));
        let s = fold_mouse(0, &buf);
        assert_eq!(s.reports, 3);
        assert!(s.pressed && s.released);
        assert!(!s.left_held());
        assert_eq!(s.scroll, 1);
        assert_eq!((s.abs_x, s.abs_y), (300, 400));
    }

    #[test]
    fn a_button_already_down_is_not_a_press() {
        let s = fold_mouse(1, &report(1, 0, 10, 10));
        assert!(!s.pressed && !s.released && s.left_held());
    }

    #[test]
    fn a_truncated_report_is_not_decoded_from_what_follows_it() {
        let mut buf = [0u8; 9];
        buf[0..6].copy_from_slice(&report(1, 0, 7, 9));
        let s = fold_mouse(0, &buf);
        assert_eq!(s.reports, 1);
        assert_eq!((s.abs_x, s.abs_y), (7, 9));
    }

    #[test]
    fn the_tablet_range_maps_onto_the_screen_and_never_off_it() {
        let screen = Rect::new(0, 0, 1920, 1080);
        assert_eq!(cursor_from_abs(0, 0, screen), Point { x: 0, y: 0 });
        assert_eq!(cursor_from_abs(32767, 32767, screen), Point { x: 1919, y: 1079 });
        assert_eq!(cursor_from_abs(16384, 16384, screen), Point { x: 960, y: 540 });
        for a in [0u16, 1, 4321, 32767, 65535] {
            let p = cursor_from_abs(a, a, screen);
            assert!(screen.contains_point(p), "{a} -> {p:?}");
        }
    }

    #[test]
    fn a_press_on_a_normal_title_bar_drags_at_once() {
        assert_eq!(
            Grab::on_title(WindowId(2), WindowMode::Normal, Point { x: 0, y: 0 }),
            Grab::Dragging { window: WindowId(2) }
        );
    }

    #[test]
    fn a_click_on_a_maximized_title_bar_does_not_unmaximize_it() {
        let mut stack = stack_of(1);
        let id = stack[0].id;
        stack[0].mode = WindowMode::Maximized;
        stack[0].saved = Rect::new(100, 100, 400, 300);
        stack[0].content = DESK.chrome.content(DESK.chrome.mode_frame(WindowMode::Maximized, DESK.screen).unwrap());
        let start = Point { x: 900, y: 10 };
        let mut grab = Grab::on_title(id, WindowMode::Maximized, start);
        // Every sample inside the threshold leaves it maximized.
        for d in [0, 1, DRAG_THRESHOLD] {
            let at = Point { x: start.x + d, y: start.y };
            assert_eq!(grab.hold(&DESK, &mut stack, at, Point { x: d, y: 0 }), Held::Idle);
            assert_eq!(stack[0].mode, WindowMode::Maximized);
        }
        assert_eq!(grab.release(&DESK, &stack, Point { x: 905, y: 10 }), Released::Nothing);
        assert_eq!(stack[0].mode, WindowMode::Maximized);
    }

    #[test]
    fn travelling_past_the_threshold_restores_the_window_under_the_pointer() {
        let mut stack = stack_of(1);
        let id = stack[0].id;
        stack[0].mode = WindowMode::Maximized;
        stack[0].saved = Rect::new(100, 100, 400, 300);
        stack[0].content = DESK.chrome.content(DESK.chrome.mode_frame(WindowMode::Maximized, DESK.screen).unwrap());
        let start = Point { x: 900, y: 10 };
        let mut grab = Grab::on_title(id, WindowMode::Maximized, start);
        let at = Point { x: start.x + DRAG_THRESHOLD + 1, y: start.y };
        assert_eq!(grab.hold(&DESK, &mut stack, at, Point { x: 6, y: 0 }), Held::Restored { window: 0 });
        assert_eq!(stack[0].mode, WindowMode::Normal);
        assert_eq!((stack[0].content.w(), stack[0].content.h()), (400, 300));
        assert!(stack[0].frame(&DESK.chrome).contains_point(at), "the pointer let go of the title bar");
        assert_eq!(grab, Grab::Dragging { window: id });
    }

    #[test]
    fn dragging_reports_both_the_rect_it_left_and_the_one_it_took() {
        let mut stack = stack_of(1);
        let mut grab = Grab::Dragging { window: stack[0].id };
        let before = stack[0].frame(&DESK.chrome);
        let held = grab.hold(&DESK, &mut stack, Point { x: 500, y: 500 }, Point { x: 300, y: 200 });
        let Held::Moved { from, to, .. } = held else { panic!("{held:?}") };
        assert_eq!(from, before);
        assert_eq!(to, before.translate(300, 200));
        assert_eq!((to.w(), to.h()), (before.w(), before.h()));
    }

    #[test]
    fn a_resize_grab_changes_the_size_and_not_the_origin() {
        let mut stack = stack_of(1);
        let mut grab = Grab::Resizing { window: stack[0].id };
        let before = stack[0].frame(&DESK.chrome);
        let held = grab.hold(&DESK, &mut stack, Point { x: 0, y: 0 }, Point { x: 50, y: -20 });
        let Held::Moved { to, .. } = held else { panic!("{held:?}") };
        assert_eq!(to.origin(), before.origin());
        assert_eq!((to.w(), to.h()), (before.w() + 50, before.h() - 20));
    }

    #[test]
    fn releasing_a_drag_at_an_edge_snaps_and_anywhere_else_does_not() {
        let corner = Point { x: 0, y: 0 };
        assert_eq!(edge_snap(&DESK, corner), Some(WindowMode::SnappedLeft));
        assert_eq!(edge_snap(&DESK, Point { x: 1919, y: 500 }), Some(WindowMode::SnappedRight));
        assert_eq!(edge_snap(&DESK, Point { x: 900, y: 0 }), Some(WindowMode::Maximized));
        assert_eq!(edge_snap(&DESK, Point { x: 900, y: 500 }), None);

        let stack = stack_of(2);
        let mut grab = Grab::Dragging { window: stack[1].id };
        assert_eq!(
            grab.release(&DESK, &stack, corner),
            Released::Snapped { window: 1, mode: WindowMode::SnappedLeft }
        );
        assert_eq!(grab, Grab::None);
    }

    #[test]
    fn releasing_a_resize_asks_for_the_buffer_and_never_snaps() {
        let stack = stack_of(1);
        let mut grab = Grab::Resizing { window: stack[0].id };
        assert_eq!(grab.release(&DESK, &stack, Point { x: 0, y: 0 }), Released::Resized { window: 0 });
        assert_eq!(grab, Grab::None);
    }

    /// `issues/kernel/compositor-holds-stale-window-index.md`: a client
    /// that exits while its window is being dragged must not panic the next
    /// sample and must not let the drag jump to whatever window slid into its
    /// slot. Named by `WindowId`, `hold` finds the window gone and ends the
    /// grab instead of indexing a position that may not even exist anymore.
    #[test]
    fn a_window_closed_mid_drag_ends_the_grab_instead_of_panicking_or_moving_another() {
        let mut stack = stack_of(2);
        let dragged = stack[1].id;
        let survivor_before = stack[0].content;
        let mut grab = Grab::Dragging { window: dragged };

        // The dragged window's client exits — the same shape as the dead-client
        // sweep's `Stack::retain`, which shifts every later index down by one.
        stack.remove(1);

        assert_eq!(
            grab.hold(&DESK, &mut stack, Point { x: 999, y: 999 }, Point { x: 50, y: 50 }),
            Held::Free,
            "a name that no longer resolves must not index whatever is left"
        );
        assert_eq!(grab, Grab::None, "the grab must not survive a window it can no longer find");
        assert_eq!(survivor_before, stack[0].content, "the surviving window must not have moved");

        // The same id, offered to `release`, must not resolve to the survivor
        // either — a lookup missing entirely is not the same failure as a
        // lookup landing on the wrong window because it recycled a position.
        let mut grab = Grab::Resizing { window: dragged };
        assert_eq!(grab.release(&DESK, &stack, Point { x: 0, y: 0 }), Released::Nothing);
    }

    /// The other half of the same defect: a window *below* the dragged one
    /// closing must not make the drag silently act on whichever window the
    /// shift left behind at the old numeric position.
    #[test]
    fn dragging_survives_a_lower_window_closing_and_still_moves_the_right_one() {
        let mut stack = stack_of(3);
        let dragged = stack[2].id;
        let mut grab = Grab::Dragging { window: dragged };

        // Removing index 0 shifts the dragged window from position 2 to 1.
        stack.remove(0);
        assert_eq!(stack.position(dragged), Some(1));

        let before = stack[1].frame(&DESK.chrome);
        let held = grab.hold(&DESK, &mut stack, Point { x: 500, y: 500 }, Point { x: 10, y: 10 });
        let Held::Moved { from, to, .. } = held else { panic!("{held:?}") };
        assert_eq!(from, before);
        assert_eq!(to, before.translate(10, 10));
        assert_eq!(stack[1].id, dragged, "the window that moved is still the one that was being dragged");
    }

    #[test]
    fn a_tab_click_reveals_minimizes_or_raises() {
        let mut stack = stack_of(3);
        assert_eq!(tab_action(&stack, 2), TabAction::Minimize);
        assert_eq!(tab_action(&stack, 0), TabAction::Raise);
        stack[0].minimized = true;
        assert_eq!(tab_action(&stack, 0), TabAction::Reveal);
    }

    #[test]
    fn the_cursor_belongs_to_the_window_under_it_and_not_to_the_focused_one() {
        let mut stack = stack_of(2);
        stack[0].cursor_style = CursorStyle::Crosshair;
        let over_back = Point { x: 110, y: 110 };
        assert_eq!(crate::hit::hit_test(&DESK, &stack, over_back, false), Hit::Content(0));
        assert_eq!(stack.focused(), Some(1));
        assert_eq!(cursor_style(&DESK, &stack, &Grab::None, over_back, false), CursorStyle::Crosshair);
    }

    #[test]
    fn a_resize_in_progress_keeps_the_resize_cursor_wherever_the_pointer_went() {
        let stack = stack_of(1);
        let far_away = Point { x: 5, y: 5 };
        assert_eq!(
            cursor_style(&DESK, &stack, &Grab::Resizing { window: stack[0].id }, far_away, false),
            CursorStyle::Resize
        );
        assert_eq!(
            cursor_style(&DESK, &stack, &Grab::None, far_away, false),
            CursorStyle::Default
        );
    }
}
