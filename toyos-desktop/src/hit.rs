use crate::layout::Desk;
use crate::rect::Point;
use crate::stack::Stack;
use crate::window::WindowMode;

/// What is under the pointer.
///
/// The window indices are into the stack as it stands when the test ran; every
/// caller acts on the answer before it reorders anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Desktop,
    TitleBar(usize),
    MinimizeButton(usize),
    MaximizeButton(usize),
    CloseButton(usize),
    Content(usize),
    ResizeCorner(usize),
    TaskbarItem(usize),
    TaskbarNew,
    LauncherItem(usize),
}

/// What the pointer is over, at `p`.
///
/// Front to back, and against exactly the rectangles the renderer paints:
/// `Chrome::buttons` gives both this and the painter their rects, so a click
/// that closes a window is a click on the close button as drawn. It was not —
/// the hit test used an open-coded half-open half-unbounded expression, which
/// made the frame's border pixel beside the button close the window too.
pub fn hit_test<C>(desk: &Desk, stack: &Stack<C>, p: Point, launcher_open: bool) -> Hit {
    let bar = desk.taskbar(stack.len());

    if launcher_open && bar.launcher().contains_point(p) {
        for i in 0..desk.apps {
            if bar.launcher_item(i).contains_point(p) {
                return Hit::LauncherItem(i);
            }
        }
    }

    if bar.strip().contains_point(p) {
        for i in 0..stack.len() {
            if bar.tab(i).contains_point(p) {
                return Hit::TaskbarItem(i);
            }
        }
        if bar.new_button().contains_point(p) {
            return Hit::TaskbarNew;
        }
        return Hit::Desktop;
    }

    for (idx, win) in stack.iter().enumerate().rev() {
        if win.minimized {
            continue;
        }
        let frame = win.frame(&desk.chrome);
        if !frame.contains_point(p) {
            continue;
        }
        let [close, maximize, minimize] = desk.chrome.buttons(frame);
        if close.contains_point(p) {
            return Hit::CloseButton(idx);
        }
        if maximize.contains_point(p) {
            return Hit::MaximizeButton(idx);
        }
        if minimize.contains_point(p) {
            return Hit::MinimizeButton(idx);
        }
        // A maximized or snapped window has no resize corner: its size is the
        // mode's to decide, and dragging one would leave a window whose mode
        // and geometry disagree.
        if win.mode == WindowMode::Normal && desk.chrome.resize_corner(frame).contains_point(p) {
            return Hit::ResizeCorner(idx);
        }
        if desk.chrome.title_strip(frame).contains_point(p) {
            return Hit::TitleBar(idx);
        }
        return Hit::Content(idx);
    }

    Hit::Desktop
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::CursorStyle;
    use crate::layout::Chrome;
    use crate::rect::Rect;
    use crate::window::Window;
    use alloc::string::ToString;

    const DESK: Desk = Desk {
        chrome: Chrome::DEFAULT,
        screen: Rect::new(0, 0, 1920, 1080),
        font_w: 8,
        apps: 2,
    };

    fn at(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    fn one_window(content: Rect) -> Stack<u32> {
        let mut s = Stack::default();
        s.insert(Window::new(0, content, "w".to_string(), false, CursorStyle::Default));
        s
    }

    #[test]
    fn nothing_under_the_pointer_is_the_desktop() {
        let s: Stack<u32> = Stack::default();
        assert_eq!(hit_test(&DESK, &s, at(500, 500), false), Hit::Desktop);
    }

    #[test]
    fn each_button_answers_for_exactly_the_rect_it_is_drawn_in() {
        let content = Rect::new(101, 130, 400, 300);
        let s = one_window(content);
        let frame = DESK.chrome.frame(content);
        let names = [Hit::CloseButton(0), Hit::MaximizeButton(0), Hit::MinimizeButton(0)];
        for (rect, want) in DESK.chrome.buttons(frame).into_iter().zip(names) {
            for p in [
                at(rect.x0, rect.y0),
                at(rect.x1 - 1, rect.y1 - 1),
                at((rect.x0 + rect.x1) / 2, (rect.y0 + rect.y1) / 2),
            ] {
                assert_eq!(hit_test(&DESK, &s, p, false), want, "{p:?} in {rect:?}");
            }
            // One pixel above the button is the frame's border, which is
            // title bar and not a button.
            assert_eq!(hit_test(&DESK, &s, at(rect.x0, rect.y0 - 1), false), Hit::TitleBar(0));
        }
    }

    #[test]
    fn the_resize_corner_belongs_to_normal_windows_only() {
        let content = Rect::new(101, 130, 400, 300);
        let mut s = one_window(content);
        let frame = DESK.chrome.frame(content);
        let corner = at(frame.x1 - 2, frame.y1 - 2);
        assert_eq!(hit_test(&DESK, &s, corner, false), Hit::ResizeCorner(0));
        s[0].mode = WindowMode::Maximized;
        assert_eq!(hit_test(&DESK, &s, corner, false), Hit::Content(0));
    }

    #[test]
    fn the_front_window_wins_an_overlap_and_a_minimized_one_never_does() {
        let mut s = one_window(Rect::new(100, 100, 400, 300));
        s.insert(Window::new(1, Rect::new(150, 150, 400, 300), "b".to_string(), false, CursorStyle::Default));
        let p = at(200, 200);
        assert_eq!(hit_test(&DESK, &s, p, false), Hit::Content(1));
        s[1].minimized = true;
        assert_eq!(hit_test(&DESK, &s, p, false), Hit::Content(0));
    }

    #[test]
    fn the_taskbar_takes_precedence_over_a_window_reaching_into_it() {
        // A window dragged down over the bar: the bar is still clickable, or
        // the last window opened could hide the launcher for good.
        let s = one_window(Rect::new(10, 900, 400, 300));
        let bar = DESK.taskbar(s.len());
        assert_eq!(hit_test(&DESK, &s, at(20, bar.strip().y0 + 4), false), Hit::TaskbarItem(0));
    }

    #[test]
    fn the_bar_past_its_own_buttons_is_desktop() {
        let s = one_window(Rect::new(10, 10, 400, 300));
        let bar = DESK.taskbar(s.len());
        assert_eq!(hit_test(&DESK, &s, at(bar.new_button().x1 + 5, bar.strip().y0 + 4), false), Hit::Desktop);
    }

    #[test]
    fn the_launcher_is_only_hit_while_it_is_open() {
        let s = one_window(Rect::new(10, 10, 400, 300));
        let bar = DESK.taskbar(s.len());
        let item = bar.launcher_item(1);
        let p = at(item.x0 + 4, item.y0 + 4);
        assert_eq!(hit_test(&DESK, &s, p, true), Hit::LauncherItem(1));
        assert_ne!(hit_test(&DESK, &s, p, false), Hit::LauncherItem(1));
    }

    #[test]
    fn an_open_launcher_covers_the_window_beneath_it() {
        let s = one_window(Rect::new(10, 500, 1000, 500));
        let bar = DESK.taskbar(s.len());
        let item = bar.launcher_item(0);
        let p = at(item.x0 + 4, item.y0 + 4);
        assert_eq!(hit_test(&DESK, &s, p, false), Hit::Content(0));
        assert_eq!(hit_test(&DESK, &s, p, true), Hit::LauncherItem(0));
    }
}
