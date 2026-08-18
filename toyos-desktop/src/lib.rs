//! Everything the desktop decides, as pure functions over pure state.
//!
//! Where a window is, what changed since the last frame, what the pointer is
//! over, who has the keyboard, what a key means, what is visible inside a
//! damaged region and which pixels of a client's buffer reach the screen. No
//! I/O, no drawing, no shared memory, no connections — those are the
//! compositor's, and it is the only caller.
//!
//! The split exists because a QEMU boot is a poor way to ask any of these
//! questions. `specs/assessments/code-quality-review-2026-08.md` §1.1 is the doctrine, and
//! the ladder it names ends here for the compositor: the arithmetic that
//! decides what a frame costs is exercised on the host in milliseconds, and the
//! guest tests are left to certify what only a guest can — that the right
//! pixels reached a panel.
//!
//! A window carries its client as an opaque `C` ([`Window`]), so reordering the
//! stack cannot separate a window's geometry from the connection behind it.
//! Nothing in this crate ever looks at that value.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod budget;
pub mod damage;
pub mod hit;
pub mod input;
pub mod layout;
pub mod plan;
pub mod rect;
pub mod stack;
pub mod taskbar;
pub mod window;

pub use budget::{create_verdict, max_windows, window_bytes, Verdict};
pub use damage::{Damage, MAX_DAMAGE_RECTS};
pub use hit::{hit_test, Hit};
pub use input::{
    cursor_from_abs, cursor_style, edge_snap, fold_mouse, key_action, tab_action, CursorStyle, Grab,
    Held, KeyAction, MouseSample, Released, TabAction, DRAG_THRESHOLD, MOUSE_EVENT_LEN,
};
pub use layout::{set_mode, Chrome, Desk};
pub use plan::{compose, content_blit, Blit, Layer};
pub use rect::{Point, Rect};
pub use stack::Stack;
pub use taskbar::{Taskbar, MAX_STATUS_CHARS, STATUS_MARGIN};
pub use window::{Window, WindowId, WindowMode};
