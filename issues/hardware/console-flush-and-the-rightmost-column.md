---
status: open
kind: defect
opened: 2026-08-03
---

# `Console::flush` records the rightmost column as painted when the scrollbar clamped it away

The blit is clamped to the bar's left edge and the bookkeeping is not:

```rust
for col in first..=last {
    ...
    self.painted[row * self.cols + col] = Some(cell);   // every col
}
let w = (span * fw).min(paint_width.saturating_sub(x));  // clamped
if w > 0 { self.screen.blit(x, row * fh, w, fh, span * fw, &strip); }
```

With the bar up, `paint_width` is `1920 - 6 = 1914`, so glyph column 239 starts
at x=1912 and receives 2 of its 8 pixel columns — while `painted[239]` claims
the whole cell was delivered. That is exactly the class `screen_console_scroll`
exists to catch, written into the emulator.

It self-heals, which is why no test sees it: `flush` fills `painted[.., 239]`
with `None` on every transition of `bar != self.painted_scrollbar`, and the bar
appears and disappears around any paging. The window is between two such
transitions — successive `scroll_view_up` calls with the bar already up, where
column 239's content changes and the panel keeps the old glyph. One column, only
while paging, gone as soon as the view returns to the bottom.

The fix is to clamp the loop rather than the blit, so a cell that was not
delivered is not recorded as delivered. Left open because it is a one-column
window under an in-progress gesture, and because the honest test for it has to
assert the panel *while* the view is off the bottom, which nothing does yet.
