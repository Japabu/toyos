use alloc::vec::Vec;

use crate::layout::Desk;
use crate::rect::Rect;
use crate::stack::Stack;
use crate::window::Window;

/// One thing to draw into the back buffer, in the order it must be drawn.
///
/// The plan is the decision and the renderer is the effect: what is visible in
/// a damaged region, in what order, clipped to what. Nothing here knows how to
/// fill a pixel, which is what makes "the wallpaper under an opaque window is
/// never composed" a property a host test can state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layer {
    /// The wallpaper, at this rect of the screen and of the scaled image.
    Wallpaper(Rect),
    /// A window's chrome and content, clipped to `clip`.
    Window { index: usize, clip: Rect },
    Taskbar { clip: Rect },
    Launcher(Rect),
}

/// Everything visible inside `region`, bottom to top.
///
/// The wallpaper is the bottom layer, so any of it under something opaque is
/// composed and then thrown away. The taskbar covers its strip always, and a
/// window that is not mid-resize covers its own frame.
pub fn compose<C>(desk: &Desk, stack: &Stack<C>, region: Rect, launcher_open: bool) -> Vec<Layer> {
    let mut out = Vec::new();
    let bar = desk.taskbar(stack.len());

    let uncovered = region.above(bar.strip().y0);
    let hidden = stack.iter().any(|w| w.is_opaque() && w.frame(&desk.chrome).contains(uncovered));
    if !uncovered.is_empty() && !hidden {
        out.push(Layer::Wallpaper(uncovered));
    }

    for (index, win) in stack.iter().enumerate() {
        if !win.minimized && region.overlaps(win.frame(&desk.chrome)) {
            out.push(Layer::Window { index, clip: region });
        }
    }

    if region.overlaps(bar.strip()) {
        out.push(Layer::Taskbar { clip: region });
    }

    // Last, so it is over every window: it is a menu and not a window.
    if launcher_open {
        let l = bar.launcher();
        if region.overlaps(l) {
            out.push(Layer::Launcher(l));
        }
    }

    out
}

/// Which pixels of a window's buffer go where on the screen.
///
/// `src` is an offset into the client's buffer in pixels, and `dst` is where
/// they land. The intersection is taken against what the buffer actually backs
/// rather than against the window, because a drag-resize runs ahead of the
/// memory: a window 40 pixels wider than its buffer must blit 40 fewer, not
/// read past the mapping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Blit {
    pub dst: Rect,
    pub src_x: i32,
    pub src_y: i32,
}

pub fn content_blit<C>(win: &Window<C>, clip: Rect) -> Option<Blit> {
    let dst = win.backed().intersect(clip);
    if dst.is_empty() {
        return None;
    }
    Some(Blit { dst, src_x: dst.x0 - win.content.x0, src_y: dst.y0 - win.content.y0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::CursorStyle;
    use crate::layout::Chrome;
    use crate::window::WindowMode;
    use alloc::string::ToString;

    const DESK: Desk = Desk {
        chrome: Chrome::DEFAULT,
        screen: Rect::new(0, 0, 1920, 1080),
        font_w: 8,
        apps: 2,
    };

    fn stack_of(contents: &[Rect]) -> Stack<usize> {
        let mut s = Stack::default();
        for (i, c) in contents.iter().enumerate() {
            s.insert(Window::new(i, *c, "w".to_string(), false, CursorStyle::Default));
        }
        s
    }

    #[test]
    fn an_empty_desktop_is_wallpaper_and_the_bar() {
        let s: Stack<usize> = Stack::default();
        let region = Rect::new(0, 0, 1920, 1080);
        let plan = compose(&DESK, &s, region, false);
        assert_eq!(plan[0], Layer::Wallpaper(region.above(DESK.work_area().y1)));
        assert!(matches!(plan[1], Layer::Taskbar { .. }));
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn an_opaque_window_covering_the_region_costs_no_wallpaper() {
        let s = stack_of(&[Rect::new(100, 100, 800, 600)]);
        let inside = Rect::new(200, 200, 50, 50);
        let plan = compose(&DESK, &s, inside, false);
        assert!(!plan.iter().any(|l| matches!(l, Layer::Wallpaper(_))), "{plan:?}");
        assert_eq!(plan, [Layer::Window { index: 0, clip: inside }]);
    }

    #[test]
    fn a_window_mid_resize_does_not_cover_the_wallpaper_it_cannot_paint() {
        let mut s = stack_of(&[Rect::new(100, 100, 800, 600)]);
        s[0].content = Rect::new(100, 100, 900, 700);
        let inside = Rect::new(200, 200, 50, 50);
        let plan = compose(&DESK, &s, inside, false);
        assert!(matches!(plan[0], Layer::Wallpaper(_)), "{plan:?}");
    }

    #[test]
    fn a_minimized_window_is_not_drawn_and_does_not_hide_the_wallpaper() {
        let mut s = stack_of(&[Rect::new(100, 100, 800, 600)]);
        s[0].minimized = true;
        let inside = Rect::new(200, 200, 50, 50);
        let plan = compose(&DESK, &s, inside, false);
        assert_eq!(plan, [Layer::Wallpaper(inside)]);
    }

    #[test]
    fn windows_are_planned_bottom_to_top_and_only_where_they_reach() {
        let s = stack_of(&[
            Rect::new(100, 100, 200, 200),
            Rect::new(150, 150, 200, 200),
            Rect::new(1500, 800, 100, 100),
        ]);
        let region = Rect::new(160, 160, 20, 20);
        let plan = compose(&DESK, &s, region, false);
        assert_eq!(
            plan,
            [Layer::Window { index: 0, clip: region }, Layer::Window { index: 1, clip: region }]
        );
    }

    #[test]
    fn the_launcher_is_planned_over_every_window() {
        let s = stack_of(&[Rect::new(0, 0, 1900, 1000)]);
        let bar = DESK.taskbar(s.len());
        let region = bar.launcher();
        let plan = compose(&DESK, &s, region, true);
        assert_eq!(*plan.last().unwrap(), Layer::Launcher(region));
        assert!(plan.iter().any(|l| matches!(l, Layer::Window { .. })));
    }

    #[test]
    fn the_wallpaper_never_reaches_under_the_taskbar() {
        let s: Stack<usize> = Stack::default();
        let bar = DESK.taskbar(0);
        let plan = compose(&DESK, &s, Rect::new(0, 0, 1920, 1080), false);
        let Layer::Wallpaper(w) = plan[0] else { panic!("{plan:?}") };
        assert!(!w.overlaps(bar.strip()));
    }

    #[test]
    fn a_region_wholly_inside_the_taskbar_plans_only_the_bar() {
        let s: Stack<usize> = Stack::default();
        let bar = DESK.taskbar(0);
        let region = Rect::new(10, bar.strip().y0 + 2, 40, 10);
        assert_eq!(compose(&DESK, &s, region, false), [Layer::Taskbar { clip: region }]);
    }

    #[test]
    fn a_blit_never_reads_outside_the_buffer() {
        let mut w: Window<()> =
            Window::new((), Rect::new(100, 100, 400, 300), "w".to_string(), false, CursorStyle::Default);
        // Dragged 200 pixels wider than the memory behind it.
        w.content = Rect::new(100, 100, 600, 300);
        let b = content_blit(&w, Rect::new(0, 0, 1920, 1080)).unwrap();
        assert_eq!(b.dst, Rect::new(100, 100, 400, 300));
        assert_eq!((b.src_x, b.src_y), (0, 0));

        let clipped = content_blit(&w, Rect::new(300, 200, 50, 50)).unwrap();
        assert_eq!(clipped.dst, Rect::new(300, 200, 50, 50));
        assert_eq!((clipped.src_x, clipped.src_y), (200, 100));
        assert!(clipped.src_x + clipped.dst.w() <= w.buf_w);
        assert!(clipped.src_y + clipped.dst.h() <= w.buf_h);
    }

    #[test]
    fn a_clip_that_misses_the_window_blits_nothing() {
        let w: Window<()> =
            Window::new((), Rect::new(100, 100, 400, 300), "w".to_string(), false, CursorStyle::Default);
        assert!(content_blit(&w, Rect::new(0, 0, 50, 50)).is_none());
        assert!(content_blit(&w, Rect::EMPTY).is_none());
    }

    /// Every pixel of a damaged region is accounted for: either something
    /// opaque claims it, or the wallpaper does.
    #[test]
    fn no_region_is_left_with_nothing_to_draw_in_it() {
        let mut s = stack_of(&[Rect::new(100, 100, 200, 200)]);
        s[0].mode = WindowMode::Normal;
        for region in [
            Rect::new(0, 0, 10, 10),
            Rect::new(150, 150, 10, 10),
            Rect::new(90, 90, 400, 400),
            Rect::new(1900, 1060, 20, 20),
        ] {
            let plan = compose(&DESK, &s, region, false);
            assert!(!plan.is_empty(), "{region:?}");
        }
    }
}
