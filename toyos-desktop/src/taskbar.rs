use crate::layout::Desk;
use crate::rect::Rect;

/// Widest status readout the taskbar will show.
///
/// `"65536M/65536M  CPU 100%  23:59"` is 30 characters at the largest figures
/// a machine this runs on produces, and `used_mb` is bounded by `total_mb`.
/// Four more so a bigger machine truncates rather than overflows its box.
pub const MAX_STATUS_CHARS: usize = 34;
pub const STATUS_MARGIN: i32 = 12;

/// The taskbar's geometry at one screen size and window count.
///
/// Every element answers for its own rectangle, so a repaint asks each whether
/// the damaged region reaches it. The clock ticks once a second and nothing
/// else about the bar changes with it: a bar that repainted whole for the clock
/// was a second's worth of tabs and titles per second, visible as a flicker for
/// as long as it was composed straight onto the panel and wasted work after.
///
/// Only [`Desk::taskbar`] builds one, so the window count it lays out is the
/// window count the caller has — a bar carrying its own copy could disagree
/// with the stack about how many tabs there are.
#[derive(Clone, Copy, Debug)]
pub struct Taskbar {
    pub(crate) desk: Desk,
    pub(crate) windows: usize,
}

impl Taskbar {
    /// The whole bar.
    pub fn strip(&self) -> Rect {
        let s = self.desk.screen;
        Rect::corners(s.x0, s.y1 - self.desk.chrome.taskbar, s.x1, s.y1)
    }

    /// One window's tab, whether or not it is on screen.
    pub fn tab(&self, i: usize) -> Rect {
        let strip = self.strip();
        let x = self.desk.chrome.taskbar_item * i as i32;
        Rect::corners(x, strip.y0, x + self.desk.chrome.taskbar_item, strip.y1)
    }

    /// The inset a tab paints its own colour in, leaving the bar's showing.
    pub fn tab_face(&self, i: usize) -> Rect {
        self.inset(self.tab(i))
    }

    fn inset(&self, r: Rect) -> Rect {
        Rect::corners(
            r.x0 + 1,
            r.y0 + self.desk.chrome.taskbar_padding,
            r.x1 - 1,
            r.y1 - self.desk.chrome.taskbar_padding,
        )
    }

    /// The `+` button that opens the launcher, square and after the last tab.
    pub fn new_button(&self) -> Rect {
        let strip = self.strip();
        let x = self.desk.chrome.taskbar_item * self.windows as i32;
        Rect::corners(x, strip.y0, x + self.desk.chrome.taskbar, strip.y1)
    }

    pub fn new_button_face(&self) -> Rect {
        self.inset(self.new_button())
    }

    /// The right-hand readout — memory, CPU and the clock.
    ///
    /// A fixed box rather than one sized to the text: it is repainted once a
    /// second for the clock, and a box that moved with the string's length
    /// would leave the tail of a longer one behind.
    pub fn status(&self) -> Rect {
        let strip = self.strip();
        let w = MAX_STATUS_CHARS as i32 * self.desk.font_w + STATUS_MARGIN * 2;
        Rect::corners((strip.x1 - w).max(strip.x0), strip.y0, strip.x1, strip.y1)
    }

    /// The bar's own background, where neither a tab nor the status box will
    /// cover it.
    pub fn gap(&self) -> Rect {
        let strip = self.strip();
        let tabs_end = self.new_button().x1.min(strip.x1);
        Rect::corners(tabs_end, strip.y0, self.status().x0, strip.y1)
    }

    /// The launcher popup, which grows upward from the `+` button.
    pub fn launcher(&self) -> Rect {
        let strip = self.strip();
        let x = self.desk.chrome.taskbar_item * self.windows as i32;
        let h = self.desk.chrome.launcher_item * self.desk.apps as i32;
        Rect::corners(x, strip.y0 - h, x + self.desk.chrome.launcher_width, strip.y0)
    }

    /// One launcher entry's row.
    pub fn launcher_item(&self, i: usize) -> Rect {
        let l = self.launcher();
        let y = l.y0 + self.desk.chrome.launcher_item * i as i32;
        Rect::corners(l.x0, y, l.x1, y + self.desk.chrome.launcher_item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Chrome;

    fn bar(windows: usize) -> Taskbar {
        Desk { chrome: Chrome::DEFAULT, screen: Rect::new(0, 0, 1920, 1080), font_w: 8, apps: 2 }
            .taskbar(windows)
    }

    #[test]
    fn the_strip_is_the_bottom_of_the_screen_and_nothing_else() {
        let b = bar(0);
        assert_eq!(b.strip(), Rect::corners(0, 1080 - 32, 1920, 1080));
        assert!(!b.strip().overlaps(b.desk.work_area()));
    }

    #[test]
    fn tabs_are_adjacent_and_the_new_button_follows_the_last() {
        let b = bar(3);
        for i in 0..2 {
            assert!(!b.tab(i).overlaps(b.tab(i + 1)));
            assert_eq!(b.tab(i).x1, b.tab(i + 1).x0);
        }
        assert_eq!(b.tab(2).x1, b.new_button().x0);
        assert!(b.strip().contains(b.new_button()));
    }

    #[test]
    fn a_tab_face_stays_inside_its_tab() {
        let b = bar(2);
        assert!(b.tab(1).contains(b.tab_face(1)));
        assert!(b.new_button().contains(b.new_button_face()));
    }

    #[test]
    fn the_gap_is_exactly_what_the_tabs_and_the_status_box_leave() {
        let b = bar(2);
        let gap = b.gap();
        assert!(!gap.overlaps(b.new_button()));
        assert!(!gap.overlaps(b.status()));
        assert_eq!(gap.x0, b.new_button().x1);
        assert_eq!(gap.x1, b.status().x0);
    }

    /// Enough tabs and the bar runs out of room — which must produce an empty
    /// gap and not one reaching backwards across the whole bar.
    #[test]
    fn a_crowded_bar_has_no_gap_rather_than_a_backwards_one() {
        let b = bar(40);
        assert!(b.gap().is_empty());
        assert!(!b.status().is_empty());
    }

    #[test]
    fn a_narrow_screen_keeps_the_status_box_on_it() {
        let desk = Desk {
            chrome: Chrome::DEFAULT,
            screen: Rect::new(0, 0, 200, 400),
            font_w: 8,
            apps: 2,
        };
        let b = desk.taskbar(0);
        assert!(b.strip().contains(b.status()));
    }

    #[test]
    fn the_launcher_sits_on_the_bar_and_grows_upward() {
        let b = bar(2);
        let l = b.launcher();
        assert_eq!(l.y1, b.strip().y0);
        assert_eq!(l.x0, b.new_button().x0);
        assert_eq!(l.h(), 2 * Chrome::DEFAULT.launcher_item);
        assert_eq!(b.launcher_item(0).y0, l.y0);
        assert_eq!(b.launcher_item(1).y1, l.y1);
        assert!(!b.launcher_item(0).overlaps(b.launcher_item(1)));
    }
}
