/// A half-open rectangle of screen pixels: `x0 <= x < x1`, `y0 <= y < y1`.
///
/// Corners rather than origin-and-extent, because every question the desktop
/// asks of a rectangle is a comparison of edges. [`intersect`](Self::intersect)
/// and [`union`](Self::union) are two mins and two maxes, [`contains`] and
/// [`overlaps`] are four comparisons, and empty is any pair whose edges cross.
/// The origin/extent form needs a subtraction for each of those, and every one
/// of those subtractions used to be open-coded at a call site — the content
/// blit's clip, the wallpaper's strip above the taskbar, the frame around a
/// window — each with its own opportunity to underflow.
///
/// [`contains`]: Self::contains
/// [`overlaps`]: Self::overlaps
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// A point in screen pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Rect {
    pub const EMPTY: Self = Self { x0: 0, y0: 0, x1: 0, y1: 0 };

    /// A rectangle from an origin and an extent.
    ///
    /// Saturating, because both halves cross a trust boundary: `MSG_PRESENT`
    /// carries four `u32`s a client chose, and an origin near [`i32::MAX`] plus
    /// an extent to match is a wrap into a rectangle covering the screen.
    /// Saturation makes it an unbounded rectangle instead, which every
    /// intersection then clips to something real.
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x0: x, y0: y, x1: x.saturating_add(w), y1: y.saturating_add(h) }
    }

    pub const fn corners(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// A rectangle a client sent, in that client's own coordinates.
    ///
    /// The one constructor that takes numbers off the wire. Each is clamped
    /// into `i32` before [`new`](Self::new) sees it, so a `u32` past
    /// [`i32::MAX`] is a rectangle reaching the far edge rather than a
    /// negative one reaching behind the origin.
    pub fn from_wire(x: u32, y: u32, w: u32, h: u32) -> Self {
        const LIMIT: u32 = i32::MAX as u32;
        Self::new(x.min(LIMIT) as i32, y.min(LIMIT) as i32, w.min(LIMIT) as i32, h.min(LIMIT) as i32)
    }

    pub const fn x(self) -> i32 {
        self.x0
    }

    pub const fn y(self) -> i32 {
        self.y0
    }

    pub const fn w(self) -> i32 {
        if self.x1 > self.x0 {
            self.x1 - self.x0
        } else {
            0
        }
    }

    pub const fn h(self) -> i32 {
        if self.y1 > self.y0 {
            self.y1 - self.y0
        } else {
            0
        }
    }

    pub const fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    pub const fn area(self) -> u64 {
        self.w() as u64 * self.h() as u64
    }

    pub fn origin(self) -> Point {
        Point { x: self.x0, y: self.y0 }
    }

    pub fn translate(self, dx: i32, dy: i32) -> Self {
        Self {
            x0: self.x0.saturating_add(dx),
            y0: self.y0.saturating_add(dy),
            x1: self.x1.saturating_add(dx),
            y1: self.y1.saturating_add(dy),
        }
    }

    /// The overlap, empty when there is none.
    pub fn intersect(self, other: Self) -> Self {
        Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }

    /// The smallest rectangle holding both.
    ///
    /// An empty operand is ignored rather than stretched over: `Rect::EMPTY`
    /// sits at the origin, and unioning it in would drag every result back to
    /// the top-left corner of the screen.
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// Whether every pixel of `other` is a pixel of `self`.
    ///
    /// An empty `other` has no pixels, so it is contained by anything —
    /// including an empty `self`.
    pub fn contains(self, other: Self) -> bool {
        other.is_empty()
            || (!self.is_empty()
                && other.x0 >= self.x0
                && other.y0 >= self.y0
                && other.x1 <= self.x1
                && other.y1 <= self.y1)
    }

    pub fn contains_point(self, p: Point) -> bool {
        p.x >= self.x0 && p.x < self.x1 && p.y >= self.y0 && p.y < self.y1
    }

    pub fn overlaps(self, other: Self) -> bool {
        !self.intersect(other).is_empty()
    }

    /// The band of `self` strictly above `y`.
    pub fn above(self, y: i32) -> Self {
        Self { y1: self.y1.min(y), ..self }
    }

    /// `self` moved — never resized — so that as much of it as will fit lies
    /// inside `bounds`.
    ///
    /// A window pushed off the bottom of a smaller screen comes back whole
    /// rather than being cropped, which is what a mode set owes every window
    /// that was not maximized: the client's buffer did not change size, so
    /// neither may the window. Cropping is [`intersect`](Self::intersect), and
    /// the two are deliberately separate operations.
    pub fn confine_to(self, bounds: Rect) -> Self {
        let x = self.x0.min(bounds.x1 - self.w()).max(bounds.x0);
        let y = self.y0.min(bounds.y1 - self.h()).max(bounds.y0);
        Self::new(x, y, self.w(), self.h())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extents_never_go_negative() {
        let crossed = Rect::corners(10, 10, 4, 4);
        assert_eq!(crossed.w(), 0);
        assert_eq!(crossed.h(), 0);
        assert!(crossed.is_empty());
        assert_eq!(crossed.area(), 0);
    }

    #[test]
    fn a_wire_rect_cannot_wrap_into_the_whole_screen() {
        let hostile = Rect::from_wire(u32::MAX, u32::MAX, u32::MAX, u32::MAX);
        let screen = Rect::new(0, 0, 1920, 1080);
        assert!(hostile.intersect(screen).is_empty());

        // The origin is inside the screen and the extent is not: the clip has
        // to be the rest of the screen, not a wrapped rectangle behind it.
        let overlong = Rect::from_wire(100, 100, u32::MAX, u32::MAX);
        assert_eq!(overlong.intersect(screen), Rect::corners(100, 100, 1920, 1080));
    }

    #[test]
    fn union_ignores_an_empty_operand() {
        let r = Rect::new(400, 300, 20, 20);
        assert_eq!(r.union(Rect::EMPTY), r);
        assert_eq!(Rect::EMPTY.union(r), r);
    }

    #[test]
    fn containment_and_overlap_agree_on_touching_edges() {
        let a = Rect::new(0, 0, 10, 10);
        let touching = Rect::new(10, 0, 10, 10);
        assert!(!a.overlaps(touching));
        assert!(a.overlaps(Rect::new(9, 0, 10, 10)));
        assert!(a.contains(Rect::new(0, 0, 10, 10)));
        assert!(!a.contains(Rect::new(0, 0, 11, 10)));
    }

    #[test]
    fn a_point_on_the_far_edge_is_outside() {
        let r = Rect::new(5, 5, 10, 10);
        assert!(r.contains_point(Point { x: 5, y: 5 }));
        assert!(r.contains_point(Point { x: 14, y: 14 }));
        assert!(!r.contains_point(Point { x: 15, y: 14 }));
        assert!(!r.contains_point(Point { x: 4, y: 5 }));
    }

    #[test]
    fn confine_brings_a_window_back_onto_a_smaller_screen() {
        let bounds = Rect::new(0, 0, 800, 600);
        let off_bottom_right = Rect::new(700, 550, 300, 200);
        let back = off_bottom_right.confine_to(bounds);
        assert!(bounds.contains(back));
        assert_eq!(back, Rect::new(500, 400, 300, 200));

        // Wider than the screen: it keeps its size and starts at the left
        // edge, because its client's buffer is that wide whatever the screen
        // does.
        let too_wide = Rect::new(10, 10, 2000, 100);
        assert_eq!(too_wide.confine_to(bounds), Rect::new(0, 10, 2000, 100));
    }
}
