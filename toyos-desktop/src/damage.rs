use alloc::vec::Vec;

use crate::rect::Rect;

/// How many disjoint regions one frame may carry.
///
/// Policy. The shapes it has to hold without merging are the ones a desktop
/// produces every frame: the cursor's two positions, a window's old and new
/// place while it is dragged, the taskbar's clock, and a client's own damage.
/// That is five; eight leaves room for a second window doing the same.
pub const MAX_DAMAGE_RECTS: usize = 8;

/// Where the desktop changed since the last composited frame.
///
/// A list rather than one bounding box, and that is the whole of why the clock
/// ticking no longer repaints a window in the middle of the screen: two damaged
/// regions far apart used to be unioned into everything between them, so a
/// character typed into a terminal at the same moment as the taskbar's second
/// cost a repaint of both plus the gap.
///
/// Bounded, because damage arrives from clients and a list that grew with it
/// would be a client deciding how much the compositor allocates. Past the bound
/// the two rects whose union wastes the fewest pixels are merged, which is the
/// same trade one bounding box makes and only where the budget runs out.
///
/// Two invariants hold after every [`add`](Self::add), and
/// [`is_disjoint`](Self::is_disjoint) is what asserts the second:
///
/// - **Coverage** — every rectangle added since the last [`take`](Self::take)
///   is contained in the union of the list. Nothing is ever forgotten; the
///   list only ever grows coarser.
/// - **Disjointness** — no two entries overlap, so a frame blits each damaged
///   pixel exactly once.
#[derive(Default, Debug)]
pub struct Damage {
    rects: Vec<Rect>,
}

impl Damage {
    pub fn add(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        self.insert_coalescing(r);
        while self.rects.len() > MAX_DAMAGE_RECTS {
            self.merge_cheapest();
        }
    }

    /// Absorb `r` into everything it touches, then re-check.
    ///
    /// The re-check is not belt-and-braces: a rect can bridge two that were
    /// disjoint, and leaving those two separate would blit their new overlap
    /// twice.
    fn insert_coalescing(&mut self, r: Rect) {
        let mut merged = r;
        let mut i = 0;
        while i < self.rects.len() {
            if self.rects[i].contains(merged) {
                return;
            }
            if self.rects[i].overlaps(merged) || merged.contains(self.rects[i]) {
                merged = merged.union(self.rects.swap_remove(i));
                i = 0;
                continue;
            }
            i += 1;
        }
        self.rects.push(merged);
    }

    /// Union the pair whose combined box wastes the fewest pixels.
    ///
    /// The union goes back in through [`insert_coalescing`](Self::insert_coalescing)
    /// rather than into the slot it came from: a box spanning two rects can
    /// reach a third that neither reached, and dropping it in place would leave
    /// the list overlapping itself.
    fn merge_cheapest(&mut self) {
        let mut best = (0usize, 1usize, u64::MAX);
        for a in 0..self.rects.len() {
            for b in a + 1..self.rects.len() {
                let waste = self.rects[a].union(self.rects[b]).area()
                    - self.rects[a].area()
                    - self.rects[b].area();
                if waste < best.2 {
                    best = (a, b, waste);
                }
            }
        }
        let (a, b, _) = best;
        let second = self.rects.swap_remove(b);
        let first = self.rects.swap_remove(a);
        self.insert_coalescing(first.union(second));
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub fn rects(&self) -> &[Rect] {
        &self.rects
    }

    /// Everything damaged, clipped to `bounds` and emptied out.
    pub fn take(&mut self, bounds: Rect) -> Vec<Rect> {
        let mut out = core::mem::take(&mut self.rects);
        out.retain_mut(|r| {
            *r = r.intersect(bounds);
            !r.is_empty()
        });
        out
    }

    /// Whether no two entries share a pixel — the invariant a caller may
    /// assert but never has to restore.
    pub fn is_disjoint(&self) -> bool {
        self.rects
            .iter()
            .enumerate()
            .all(|(i, a)| self.rects[i + 1..].iter().all(|b| !a.overlaps(*b)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Every pixel of `added` that lies in `bounds` is covered by `got`.
    fn covers(got: &[Rect], added: &[Rect], bounds: Rect) -> bool {
        for r in added {
            let r = r.intersect(bounds);
            for y in r.y0..r.y1 {
                for x in r.x0..r.x1 {
                    let p = crate::rect::Point { x, y };
                    if !got.iter().any(|g| g.contains_point(p)) {
                        return false;
                    }
                }
            }
        }
        true
    }

    #[test]
    fn two_distant_rects_stay_two() {
        let mut d = Damage::default();
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(900, 700, 10, 10));
        assert_eq!(d.rects().len(), 2);
        assert!(d.is_disjoint());
    }

    #[test]
    fn a_bridging_rect_collapses_both_it_joins() {
        let mut d = Damage::default();
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(20, 0, 10, 10));
        assert_eq!(d.rects().len(), 2);
        d.add(Rect::new(5, 0, 20, 10));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 30, 10)]);
    }

    #[test]
    fn a_contained_rect_changes_nothing() {
        let mut d = Damage::default();
        d.add(Rect::new(0, 0, 100, 100));
        d.add(Rect::new(10, 10, 5, 5));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 100, 100)]);
    }

    #[test]
    fn an_empty_rect_is_not_damage() {
        let mut d = Damage::default();
        d.add(Rect::corners(50, 50, 50, 90));
        assert!(d.is_empty());
    }

    #[test]
    fn the_bound_holds_and_the_list_stays_disjoint() {
        let mut d = Damage::default();
        let mut added = vec![];
        // Far apart in both axes so nothing coalesces on its own and the
        // budget is what has to do the work.
        for i in 0..40i32 {
            let r = Rect::new(i * 40, (i % 7) * 90, 12, 12);
            added.push(r);
            d.add(r);
            assert!(d.rects().len() <= MAX_DAMAGE_RECTS, "{} rects", d.rects().len());
            assert!(d.is_disjoint());
        }
        let bounds = Rect::new(0, 0, 1920, 1080);
        assert!(covers(&d.take(bounds), &added, bounds));
    }

    #[test]
    fn take_clips_to_the_screen_and_drops_what_falls_outside() {
        let mut d = Damage::default();
        d.add(Rect::new(-50, -50, 100, 100));
        d.add(Rect::new(5000, 5000, 10, 10));
        let out = d.take(Rect::new(0, 0, 800, 600));
        assert_eq!(out, vec![Rect::new(0, 0, 50, 50)]);
        assert!(d.is_empty());
    }

    /// Coverage and disjointness under an adversarial stream, which is what a
    /// client's own `MSG_PRESENT` damage is.
    #[test]
    fn coverage_survives_a_random_stream() {
        let bounds = Rect::new(0, 0, 96, 96);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..300 {
            let mut d = Damage::default();
            let mut added = vec![];
            for _ in 0..20 {
                let v = next();
                let x = (v % 96) as i32;
                let y = ((v >> 8) % 96) as i32;
                let w = ((v >> 16) % 20) as i32;
                let h = ((v >> 24) % 20) as i32;
                let r = Rect::new(x, y, w, h);
                added.push(r);
                d.add(r);
                assert!(d.rects().len() <= MAX_DAMAGE_RECTS);
                assert!(d.is_disjoint(), "{:?}", d.rects());
            }
            assert!(covers(&d.take(bounds), &added, bounds));
        }
    }
}
