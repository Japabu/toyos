---
status: open
kind: defect
opened: 2026-08-06
---

# The compositor holds a window *index* across passes, and every removal invalidates it

Found while reading the close path, not reproduced. `Interaction`
(`userland/compositor/src/main.rs:825`) carries `window_idx: usize` in its
`DragPending`, `Dragging` and `Resizing` variants, it survives between event
loop passes, and the drag and resize arms index `windows[window_idx]` with no
revalidation (`:1901`, `:1912`). Every path that shortens or reorders `windows`
invalidates it: `remove` for the close button, GUI+Q and `MSG_DESTROY_WINDOW`,
`retain` for the dead-client sweep, and `bring_to_front`'s remove-and-insert.

Two consequences and a client can drive both, which is what makes it the same
class as the grant that killed the desktop: a window removed *below* the
dragged one moves the wrong window, and a window removed *at or above* the last
index makes `windows[window_idx]` an out-of-bounds panic. A client that exits
while the user is dragging is the whole of the reproduction.

The fix is the one the same file already uses one line away —
`last_title_click_fd: Option<Fd>` identifies a window by its `Fd` rather than
its position — and it wants a gate that drops a window mid-drag.
