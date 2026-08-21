use alloc::string::String;

use crate::input::CursorStyle;
use crate::layout::Chrome;
use crate::rect::Rect;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowMode {
    Normal,
    Maximized,
    SnappedLeft,
    SnappedRight,
}

/// A window's identity, stable for as long as it is in a [`Stack`](crate::stack::Stack).
///
/// Its position is not: every close, reorder or dead-client sweep can move a
/// window or remove it, so state that outlives one event-loop pass — a drag,
/// a resize — must not name a window by where it sat when the state began.
/// [`Stack::insert`](crate::stack::Stack::insert) is the only place one is
/// minted, and [`Stack::position`](crate::stack::Stack::position) is how it
/// is turned back into a position, fresh, every time it is needed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WindowId(pub(crate) u64);

impl WindowId {
    /// [`Window::new`]'s placeholder before the window has ever been in a
    /// stack. `Stack::insert` never hands this out — its counter starts at 1
    /// — so a caller can tell an unminted window from a real one.
    pub(crate) const UNASSIGNED: Self = Self(0);
}

/// A window as the desktop reasons about it: geometry, order and state.
///
/// `C` is whatever the compositor needs to *reach* the client — a connection,
/// a pid, a shared mapping, a receive buffer — and nothing here ever looks at
/// it. That is the split: this crate decides where a window is and what it
/// gets, the shell does it. The client rides inside the same value so that
/// reordering the stack cannot desynchronise geometry from the connection it
/// belongs to, which a second parallel list would allow.
pub struct Window<C> {
    /// Assigned by `Stack::insert`; see [`WindowId`].
    pub id: WindowId,
    pub client: C,
    /// The client's own pixels, in screen coordinates.
    pub content: Rect,
    /// The buffer the client actually holds. A drag-resize runs ahead of it:
    /// the window is already bigger than the memory behind it until the
    /// pointer is released.
    pub buf_w: i32,
    pub buf_h: i32,
    pub title: String,
    pub minimized: bool,
    pub topmost: bool,
    pub mode: WindowMode,
    /// Where a maximized or snapped window goes back to.
    pub saved: Rect,
    /// The client has drawn something not yet composited.
    pub presented: bool,
    pub cursor_style: CursorStyle,
}

impl<C> Window<C> {
    pub fn new(
        client: C,
        content: Rect,
        title: String,
        topmost: bool,
        cursor_style: CursorStyle,
    ) -> Self {
        Self {
            id: WindowId::UNASSIGNED,
            client,
            content,
            buf_w: content.w(),
            buf_h: content.h(),
            title,
            minimized: false,
            topmost,
            mode: WindowMode::Normal,
            saved: Rect::EMPTY,
            presented: false,
            cursor_style,
        }
    }

    pub fn frame(&self, chrome: &Chrome) -> Rect {
        chrome.frame(self.content)
    }

    /// Whether everything inside the window's frame comes from this window.
    ///
    /// False while a drag-resize is ahead of the buffer the client was given:
    /// the content blit is clipped to the buffer, so the rest of the frame is
    /// whatever was under it and the wallpaper below still has to be composed.
    pub fn is_opaque(&self) -> bool {
        !self.minimized && self.content.w() <= self.buf_w && self.content.h() <= self.buf_h
    }

    /// The part of the content the client's buffer actually backs.
    pub fn backed(&self) -> Rect {
        Rect::new(
            self.content.x0,
            self.content.y0,
            self.content.w().min(self.buf_w),
            self.content.h().min(self.buf_h),
        )
    }

    /// Remember where to come back to, if this is the first departure from
    /// [`WindowMode::Normal`].
    pub fn save_if_normal(&mut self) {
        if self.mode == WindowMode::Normal {
            self.saved = self.content;
        }
    }

    /// Where a client's own damage claim lands on the screen.
    ///
    /// `claim` is in the window's own pixels and is the payload of
    /// `MSG_PRESENT`. It is clamped to the window rather than believed: a bad
    /// one is a client scribbling on the desktop, and there is no reading of
    /// it that is worth a repaint of the screen.
    pub fn present_damage(&self, claim: Rect) -> Rect {
        claim
            .intersect(Rect::new(0, 0, self.content.w(), self.content.h()))
            .translate(self.content.x0, self.content.y0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn win(content: Rect) -> Window<()> {
        Window::new((), content, "t".to_string(), false, CursorStyle::Default)
    }

    #[test]
    fn a_client_cannot_damage_more_than_its_own_window() {
        let w = win(Rect::new(100, 200, 400, 300));
        assert_eq!(w.present_damage(Rect::new(0, 0, 10, 10)), Rect::new(100, 200, 10, 10));
        assert_eq!(
            w.present_damage(Rect::from_wire(0, 0, u32::MAX, u32::MAX)),
            Rect::new(100, 200, 400, 300)
        );
        assert!(w.present_damage(Rect::new(500, 500, 10, 10)).is_empty());
        assert!(w.present_damage(Rect::from_wire(u32::MAX, u32::MAX, 1, 1)).is_empty());
    }

    #[test]
    fn a_window_mid_resize_is_not_opaque_and_its_backing_is_the_buffer() {
        let mut w = win(Rect::new(10, 10, 400, 300));
        assert!(w.is_opaque());
        w.content = Rect::new(10, 10, 600, 300);
        assert!(!w.is_opaque());
        assert_eq!(w.backed(), Rect::new(10, 10, 400, 300));
    }

    #[test]
    fn a_window_shrunk_below_its_buffer_is_still_opaque() {
        let mut w = win(Rect::new(10, 10, 400, 300));
        w.content = Rect::new(10, 10, 200, 100);
        assert!(w.is_opaque());
        assert_eq!(w.backed(), w.content);
    }

    #[test]
    fn a_minimized_window_is_never_opaque() {
        let mut w = win(Rect::new(10, 10, 400, 300));
        w.minimized = true;
        assert!(!w.is_opaque());
    }

    #[test]
    fn only_the_first_departure_from_normal_is_saved() {
        let mut w = win(Rect::new(10, 10, 400, 300));
        w.save_if_normal();
        assert_eq!(w.saved, Rect::new(10, 10, 400, 300));
        w.mode = WindowMode::Maximized;
        w.content = Rect::new(0, 0, 1920, 1000);
        w.save_if_normal();
        assert_eq!(w.saved, Rect::new(10, 10, 400, 300));
    }
}
