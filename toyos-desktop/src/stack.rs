use alloc::vec::Vec;
use core::ops::{Index, IndexMut};

use crate::window::{Window, WindowId};

/// The windows, bottom to top.
///
/// The order *is* the Z-order, and one invariant holds over it: **every
/// topmost window sits above every ordinary one**. A window that asked to stay
/// on top is a file picker over the application that opened it, and an
/// ordinary window appearing above it is that dialog lost behind its own
/// parent. [`is_ordered`](Self::is_ordered) states it; [`insert`](Self::insert)
/// and [`raise`](Self::raise) are the only ways in, and both maintain it.
///
/// The `Vec` is private for that reason: `push` was the way in until this
/// existed, and `push` puts an ordinary window above every topmost one.
pub struct Stack<C> {
    windows: Vec<Window<C>>,
    /// The id `insert` hands to the next window that does not already have
    /// one. Monotonic and never reused, so a [`WindowId`] a caller is still
    /// holding never comes to name a different window than the one it named
    /// when it was taken.
    next_id: u64,
}

impl<C> Default for Stack<C> {
    fn default() -> Self {
        // 0 is `WindowId::UNASSIGNED`; starting here is what makes it a value
        // `insert` can never hand out.
        Self { windows: Vec::new(), next_id: 1 }
    }
}

impl<C> Stack<C> {
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    pub fn as_slice(&self) -> &[Window<C>] {
        &self.windows
    }

    pub fn iter(&self) -> core::slice::Iter<'_, Window<C>> {
        self.windows.iter()
    }

    pub fn iter_mut(&mut self) -> core::slice::IterMut<'_, Window<C>> {
        self.windows.iter_mut()
    }

    pub fn find(&self, pred: impl Fn(&Window<C>) -> bool) -> Option<usize> {
        self.windows.iter().position(pred)
    }

    /// The current position of the window named `id`, or `None` if it is no
    /// longer here.
    ///
    /// This is how state that outlives one event-loop pass — a drag, a
    /// resize — has to reach a window: never by the position it had when the
    /// pass began, which `remove`, `retain`, `raise` and `cycle` all make
    /// stale the moment anything else in the stack changes.
    pub fn position(&self, id: WindowId) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    /// Put `w` as high as its topmost flag entitles it to be, and say where it
    /// landed.
    ///
    /// A window with [`WindowId::UNASSIGNED`](crate::window::WindowId) — one
    /// fresh out of `Window::new` — is minted a real, never-repeated id here.
    /// A window that already has one, because this is `raise` moving it
    /// rather than a client's window being seen for the first time, keeps it:
    /// identity survives reorder even though position does not.
    pub fn insert(&mut self, mut w: Window<C>) -> usize {
        if w.id == WindowId::UNASSIGNED {
            w.id = WindowId(self.next_id);
            self.next_id += 1;
        }
        let at = if w.topmost {
            self.windows.len()
        } else {
            self.windows.iter().position(|o| o.topmost).unwrap_or(self.windows.len())
        };
        self.windows.insert(at, w);
        at
    }

    pub fn remove(&mut self, idx: usize) -> Window<C> {
        self.windows.remove(idx)
    }

    pub fn retain(&mut self, keep: impl Fn(&Window<C>) -> bool) {
        self.windows.retain(|w| keep(w));
    }

    /// Bring the window at `idx` as far forward as it may go, and say where it
    /// ended up.
    pub fn raise(&mut self, idx: usize) -> usize {
        let w = self.windows.remove(idx);
        self.insert(w)
    }

    /// The window that has the keyboard: the topmost one that is not
    /// minimized.
    pub fn focused(&self) -> Option<usize> {
        self.windows.iter().rposition(|w| !w.minimized)
    }

    /// Send the top ordinary window to the bottom and reveal the next.
    ///
    /// False when there is nothing to rotate — fewer than two ordinary windows
    /// — so the caller knows whether anything moved and therefore whether the
    /// screen changed. Topmost windows do not take part: cycling through one
    /// would either move it below an ordinary window or do nothing visible.
    pub fn cycle(&mut self) -> bool {
        let ordinary = self.windows.iter().position(|w| w.topmost).unwrap_or(self.windows.len());
        if ordinary < 2 {
            return false;
        }
        let w = self.windows.remove(ordinary - 1);
        self.windows.insert(0, w);
        self.windows[ordinary - 1].minimized = false;
        true
    }

    /// Whether the topmost windows form a suffix — the invariant this type
    /// exists to keep.
    pub fn is_ordered(&self) -> bool {
        let first = self.windows.iter().position(|w| w.topmost).unwrap_or(self.windows.len());
        self.windows[first..].iter().all(|w| w.topmost)
    }
}

impl<C> Index<usize> for Stack<C> {
    type Output = Window<C>;
    fn index(&self, i: usize) -> &Window<C> {
        &self.windows[i]
    }
}

impl<C> IndexMut<usize> for Stack<C> {
    fn index_mut(&mut self, i: usize) -> &mut Window<C> {
        &mut self.windows[i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::CursorStyle;
    use crate::rect::Rect;
    use alloc::string::ToString;

    fn stack(spec: &[(&str, bool)]) -> Stack<&'static str> {
        let mut s = Stack::default();
        for (name, topmost) in spec {
            let name: &'static str = alloc::boxed::Box::leak(name.to_string().into_boxed_str());
            s.insert(Window::new(name, Rect::new(0, 0, 100, 100), name.to_string(), *topmost, CursorStyle::Default));
        }
        s
    }

    fn names<C: Copy>(s: &Stack<C>) -> Vec<C> {
        s.iter().map(|w| w.client).collect()
    }

    #[test]
    fn an_ordinary_window_opened_after_a_topmost_one_goes_under_it() {
        let s = stack(&[("picker", true), ("term", false)]);
        assert_eq!(names(&s), ["term", "picker"]);
        assert!(s.is_ordered());
    }

    #[test]
    fn raising_an_ordinary_window_cannot_lift_it_over_a_topmost_one() {
        let mut s = stack(&[("a", false), ("b", false), ("picker", true)]);
        let at = s.raise(0);
        assert_eq!(names(&s), ["b", "a", "picker"]);
        assert_eq!(at, 1);
        assert!(s.is_ordered());
    }

    #[test]
    fn raising_the_window_already_on_top_leaves_it_there() {
        let mut s = stack(&[("a", false), ("b", false)]);
        assert_eq!(s.raise(1), 1);
        assert_eq!(names(&s), ["a", "b"]);
    }

    #[test]
    fn a_topmost_window_raises_above_every_other_topmost_one() {
        let mut s = stack(&[("p", true), ("q", true)]);
        assert_eq!(s.raise(0), 1);
        assert_eq!(names(&s), ["q", "p"]);
        assert!(s.is_ordered());
    }

    #[test]
    fn focus_is_the_topmost_window_that_is_not_minimized() {
        let mut s = stack(&[("a", false), ("b", false), ("c", false)]);
        assert_eq!(s.focused(), Some(2));
        s[2].minimized = true;
        assert_eq!(s.focused(), Some(1));
        s[1].minimized = true;
        s[0].minimized = true;
        assert_eq!(s.focused(), None);
    }

    #[test]
    fn cycling_sends_the_top_window_to_the_bottom_and_reveals_the_next() {
        let mut s = stack(&[("a", false), ("b", false), ("c", false)]);
        s[1].minimized = true;
        assert!(s.cycle());
        assert_eq!(names(&s), ["c", "a", "b"]);
        assert!(!s[2].minimized, "the window cycling revealed is still hidden");
        assert!(s.is_ordered());
    }

    #[test]
    fn cycling_needs_two_ordinary_windows() {
        let mut empty: Stack<&str> = Stack::default();
        assert!(!empty.cycle());
        let mut one = stack(&[("a", false)]);
        assert!(!one.cycle());
        // One ordinary window and a topmost one is still nothing to rotate.
        let mut mixed = stack(&[("a", false), ("p", true)]);
        assert!(!mixed.cycle());
        assert_eq!(names(&mixed), ["a", "p"]);
    }

    #[test]
    fn cycling_leaves_a_topmost_window_on_top() {
        let mut s = stack(&[("a", false), ("b", false), ("p", true)]);
        assert!(s.cycle());
        assert_eq!(names(&s), ["b", "a", "p"]);
        assert!(s.is_ordered());
    }
}
