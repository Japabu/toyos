use std::process::Command;
use std::time::{Duration, Instant};

use toyos::shm::SharedMemory;
use toyos::ipc::RxStep;
use toyos::{gpu, ipc, services, system, Connection, Keyboard, Mouse, FramebufferDev};
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos_abi::Fd;
use window::{Color, Framebuffer, Traffic};

const TITLE_BAR_HEIGHT: usize = 28;
const BORDER_WIDTH: usize = 1;
const RESIZE_HANDLE_SIZE: usize = 16;
const BUTTON_WIDTH: usize = 28;
const MIN_CONTENT_WIDTH: usize = 200;
const MIN_CONTENT_HEIGHT: usize = 100;
const INITIAL_MARGIN: usize = 40;
const CASCADE_OFFSET: usize = 30;
const TASKBAR_HEIGHT: usize = 32;
const TASKBAR_ITEM_WIDTH: usize = 160;
const TASKBAR_PADDING: usize = 4;
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(400);
const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667); // ~60fps
const STATS_INTERVAL: Duration = Duration::from_secs(2);

const FOCUSED_TITLE_COLOR: Color = Color { r: 0x3a, g: 0x3a, b: 0x4e };
const UNFOCUSED_TITLE_COLOR: Color = Color { r: 0x28, g: 0x28, b: 0x32 };
const FOCUSED_BORDER_COLOR: Color = Color { r: 0x58, g: 0x58, b: 0x6e };
const UNFOCUSED_BORDER_COLOR: Color = Color { r: 0x38, g: 0x38, b: 0x42 };
const FOCUSED_TITLE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const UNFOCUSED_TITLE_TEXT: Color = Color { r: 0x60, g: 0x60, b: 0x70 };
const CLOSE_BUTTON_BG: Color = Color { r: 0x50, g: 0x28, b: 0x28 };
const TASKBAR_COLOR: Color = Color { r: 0x18, g: 0x18, b: 0x25 };
const TASKBAR_ACTIVE_COLOR: Color = Color { r: 0x30, g: 0x30, b: 0x45 };
const TASKBAR_TEXT_COLOR: Color = Color { r: 0x80, g: 0x80, b: 0x90 };
const TASKBAR_ACTIVE_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };
const TASKBAR_NEW_COLOR: Color = Color { r: 0x40, g: 0x60, b: 0x40 };
const TASKBAR_NEW_TEXT: Color = Color { r: 0x80, g: 0xc0, b: 0x80 };
const TASKBAR_MINIMIZED_COLOR: Color = Color { r: 0x20, g: 0x20, b: 0x30 };
const TASKBAR_MINIMIZED_TEXT: Color = Color { r: 0x50, g: 0x50, b: 0x60 };
const LAUNCHER_WIDTH: usize = 160;
const LAUNCHER_ITEM_HEIGHT: usize = 28;
const LAUNCHER_BG: Color = Color { r: 0x20, g: 0x20, b: 0x30 };
const LAUNCHER_TEXT: Color = Color { r: 0xe0, g: 0xe0, b: 0xe8 };

struct LauncherEntry {
    name: &'static str,
    path: &'static str,
}

const LAUNCHER_APPS: &[LauncherEntry] = &[
    LauncherEntry { name: "Terminal", path: "/bin/terminal" },
    LauncherEntry { name: "Files", path: "/bin/files" },
];

const FLAG_HARDWARE_CURSOR: u32 = 1 << 0;

/// Fds the compositor watches that are not windows: keyboard, mouse, listener.
const FIXED_POLL_FDS: u32 = 3;

/// Connections accepted but not yet identified by a first frame.
///
/// The kernel queues 32 unaccepted connections per listener
/// (`listener::MAX_PENDING_CONNECTIONS`); this is the same allowance one step
/// further along, for a client that has been accepted and has not yet said
/// what it wants. Past it the compositor refuses by name rather than growing,
/// and [`HANDSHAKE_TIMEOUT`] is what guarantees the table drains.
const MAX_PENDING_CONNS: u32 = 32;

/// How long a connection may go without completing its first frame.
///
/// Policy, and generous: every client in the tree sends its first frame in the
/// statement after `connect`. What this bounds is the one that never sends it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// Hard ceiling on live windows, from the poller rather than from memory.
///
/// Every window's fd is registered in the same batch as the three fixed ones
/// and the pending connections, and [`Poller::MAX_HANDLES`] is the widest set
/// one poller can carry. Unlike [`max_windows`] this does not move when the
/// resolution does, which is why the poller is sized from it:
/// `MSG_SET_RESOLUTION` can make windows cheaper mid-run, and a poller sized
/// for the old screen would then be too small. At any resolution this machine
/// can actually scan out, the memory budget is far below this and is what
/// binds.
const MAX_WINDOW_SLOTS: u32 = Poller::MAX_HANDLES - FIXED_POLL_FDS - MAX_PENDING_CONNS;

/// The largest client payload the compositor keeps.
///
/// `MSG_CLIPBOARD_SET`'s 116 bytes is the widest of them; every typed payload
/// it decodes is smaller (`CreateWindowRequest`, 40). A client may declare
/// anything up to `ipc::MAX_FRAME_LEN` — the excess is counted down and
/// discarded, never waited for.
const MAX_KEPT_PAYLOAD: usize = 116;

/// How long one pass may spend draining clients before it must composite.
///
/// Without it a client that never stops sending keeps its fd ready forever and
/// the loop below never reaches `redraw` — a freeze with a different shape and
/// the same result. The drain loop's promise is "everything pending", and this
/// is the clause that makes it "or one frame's worth, whichever is sooner".
const DRAIN_BUDGET: Duration = FRAME_INTERVAL;

/// Share of physical memory the compositor will hold in window buffers.
///
/// Policy, not derivation. Nothing in the kernel says what a process may use —
/// no per-process limit, no pressure signal, no OOM killer — so the quantity
/// that would make this derivable does not exist yet. An eighth leaves seven
/// eighths for the kernel, the other daemons, and the clients' own heaps.
const WINDOW_BUDGET_SHARE: u64 = 8;

/// Most physical memory one window can cost at this screen size.
///
/// A screen-sized content buffer is the largest the compositor ever hands out:
/// `MSG_CREATE_WINDOW` refuses a request bigger than the screen, and every
/// path that grows a window afterwards — maximize, snap, drag-resize — is
/// bounded by the screen too. The kernel rounds every shared region up to a
/// 2 MiB page (`shared_memory::alloc` calls `align_2m`), so that, not the pixel
/// count, is what a window takes out of physical memory: a one-pixel window
/// still costs 2 MiB.
///
/// Deliberately a function of the screen — a bigger screen means bigger windows
/// means fewer of them — which no bare constant expresses.
fn window_bytes(screen_w: usize, screen_h: usize) -> usize {
    const PAGE_2M: usize = 2 * 1024 * 1024;
    (screen_w * screen_h * 4).div_ceil(PAGE_2M).max(1) * PAGE_2M
}

/// How many windows the compositor will hold at this screen size.
///
/// One eighth of physical memory divided by what a window costs there, floored
/// at one — a compositor that can never open a window is worse than one over
/// its budget by a single window — and capped at what one poller can watch.
///
/// **This is a mitigation, and the thing it mitigates is the real defect.** A
/// window buffer is charged to nobody: there is no per-process memory limit, no
/// pressure signal and no OOM killer (`specs/known-issues.md` §1), so without a
/// cap any client can walk the machine into exhaustion by asking for windows,
/// and the compositor cannot tell a desktop from an attack. The 2 MiB rounding
/// amplifies it: at 2048x2048 a window costs exactly 16 MiB, so 64 windows is
/// a gigabyte. Read this as "how much we can afford to hand out while nothing
/// can make us take it back", not as a considered UX limit — the number to
/// delete this in favour of is a kernel memory limit.
fn max_windows(total_mem: u64, screen_w: usize, screen_h: usize) -> usize {
    let budget = total_mem / WINDOW_BUDGET_SHARE;
    let affordable = budget / window_bytes(screen_w, screen_h) as u64;
    affordable.clamp(1, MAX_WINDOW_SLOTS as u64) as usize
}

/// Total physical memory, as the kernel reports it.
fn total_memory() -> u64 {
    let mut buf = [0u8; system::SYSINFO_HEADER_SIZE];
    let n = system::sysinfo(&mut buf);
    assert!(n >= system::SYSINFO_HEADER_SIZE, "sysinfo returned {n} bytes");
    u64::from_le_bytes(buf[0..8].try_into().unwrap())
}

/// One client's inbound framing.
///
/// **The compositor never reads a client with a blocking read.** That is the
/// whole point of [`ipc::FrameRx`]: `ipc::recv_header` and `ipc::recv_payload`
/// park the caller until the peer sends the bytes it promised, which hands a
/// client the decision of when the desktop runs again. Here a peer that stops
/// halfway through a frame costs a buffer and a deadline instead of the event
/// loop.
type ClientRx = ipc::FrameRx<MAX_KEPT_PAYLOAD>;

/// A whole client message, off the fd and in memory.
///
/// `conn` is `Some` only for the first frame on a freshly accepted connection:
/// `MSG_CREATE_WINDOW` keeps it, and every other message type answers on it
/// and lets it close.
struct ClientFrame {
    fd: Fd,
    pid: u32,
    msg_type: u32,
    payload: [u8; MAX_KEPT_PAYLOAD],
    payload_len: usize,
    conn: Option<Connection>,
}

impl ClientFrame {
    fn new(fd: Fd, pid: u32, msg_type: u32) -> Self {
        Self { fd, pid, msg_type, payload: [0; MAX_KEPT_PAYLOAD], payload_len: 0, conn: None }
    }

    fn set_payload(&mut self, bytes: &[u8]) {
        self.payload[..bytes.len()].copy_from_slice(bytes);
        self.payload_len = bytes.len();
    }
}

/// A connection that has been accepted and has not yet said what it is for.
///
/// It exists because `accept` and the first frame are two events, and the
/// compositor used to fuse them with a blocking `recv_header` — so a client
/// that connected and sent four bytes owned the desktop until it disconnected.
struct PendingConn {
    conn: Connection,
    pid: u32,
    rx: ClientRx,
    since: Instant,
}

struct WindowState {
    fd: Connection,
    pid: u32,
    shm: SharedMemory,
    content_x: usize,
    content_y: usize,
    width: usize,
    height: usize,
    buf_width: usize,
    buf_height: usize,
    title: String,
    minimized: bool,
    topmost: bool,
    mode: WindowMode,
    saved_x: usize,
    saved_y: usize,
    saved_w: usize,
    saved_h: usize,
    presented: bool,
    cursor_style: u8,
    rx: ClientRx,
}

#[derive(Clone, Copy, PartialEq)]
enum WindowMode {
    Normal,
    Maximized,
    SnappedLeft,
    SnappedRight,
}

struct TitleBarIcons {
    minimize: sprite::Sprite,
    maximize: sprite::Sprite,
    close: sprite::Sprite,
}

enum HitZone {
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

fn focused_window_idx(windows: &[WindowState]) -> Option<usize> {
    windows
        .iter()
        .enumerate()
        .rev()
        .find(|(_, w)| !w.minimized)
        .map(|(i, _)| i)
}

/// Bring window at `idx` to front, keeping topmost windows always on top.
/// Returns the new index of the moved window.
fn bring_to_front(windows: &mut Vec<WindowState>, idx: usize) -> usize {
    if idx == windows.len() - 1 {
        return idx;
    }
    let win = windows.remove(idx);
    if win.topmost {
        // Topmost windows go to the very end
        windows.push(win);
        windows.len() - 1
    } else {
        // Non-topmost windows go before the first topmost window
        let insert_at = windows.iter().position(|w| w.topmost).unwrap_or(windows.len());
        windows.insert(insert_at, win);
        insert_at
    }
}

fn resize_window(
    win: &mut WindowState,
    new_w: usize,
    new_h: usize,
    pixel_format: u32,
    dead: &mut Vec<Dead>,
) {
    let old_token = win.shm.token();
    let buf_size = new_w * new_h * 4;
    let new_shm = SharedMemory::allocate(buf_size);
    new_shm.grant(win.pid);
    let token = new_shm.token();
    // Replace the old SharedMemory (drops it, releasing the old mapping)
    win.shm = new_shm;
    win.width = new_w;
    win.height = new_h;
    win.buf_width = new_w;
    win.buf_height = new_h;
    // The one message a window cannot afford to miss — the old mapping is
    // already gone — so a client that will not take it is dropped rather than
    // left drawing into memory it no longer owns.
    deliver(
        dead,
        win,
        window::MSG_WINDOW_RESIZED,
        &window::ResizeInfo {
            token,
            old_token,
            width: new_w as u32,
            height: new_h as u32,
            stride: new_w as u32,
            pixel_format,
        },
    );
}

fn save_if_normal(win: &mut WindowState) {
    if win.mode == WindowMode::Normal {
        win.saved_x = win.content_x;
        win.saved_y = win.content_y;
        win.saved_w = win.width;
        win.saved_h = win.height;
    }
}

fn maximize_window(
    win: &mut WindowState,
    screen_w: usize,
    screen_h: usize,
    pixel_format: u32,
    dead: &mut Vec<Dead>,
) {
    save_if_normal(win);
    win.mode = WindowMode::Maximized;
    win.content_x = BORDER_WIDTH;
    win.content_y = BORDER_WIDTH + TITLE_BAR_HEIGHT;
    let new_w = screen_w - BORDER_WIDTH * 2;
    let new_h = screen_h - TASKBAR_HEIGHT - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;
    resize_window(win, new_w, new_h, pixel_format, dead);
}

fn snap_left(
    win: &mut WindowState,
    screen_w: usize,
    screen_h: usize,
    pixel_format: u32,
    dead: &mut Vec<Dead>,
) {
    save_if_normal(win);
    win.mode = WindowMode::SnappedLeft;
    win.content_x = BORDER_WIDTH;
    win.content_y = BORDER_WIDTH + TITLE_BAR_HEIGHT;
    let new_w = screen_w / 2 - BORDER_WIDTH * 2;
    let new_h = screen_h - TASKBAR_HEIGHT - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;
    resize_window(win, new_w, new_h, pixel_format, dead);
}

fn snap_right(
    win: &mut WindowState,
    screen_w: usize,
    screen_h: usize,
    pixel_format: u32,
    dead: &mut Vec<Dead>,
) {
    save_if_normal(win);
    win.mode = WindowMode::SnappedRight;
    win.content_x = screen_w / 2 + BORDER_WIDTH;
    win.content_y = BORDER_WIDTH + TITLE_BAR_HEIGHT;
    let new_w = screen_w / 2 - BORDER_WIDTH * 2;
    let new_h = screen_h - TASKBAR_HEIGHT - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;
    resize_window(win, new_w, new_h, pixel_format, dead);
}

fn restore_window(win: &mut WindowState, pixel_format: u32, dead: &mut Vec<Dead>) {
    win.mode = WindowMode::Normal;
    win.content_x = win.saved_x;
    win.content_y = win.saved_y;
    let w = win.saved_w;
    let h = win.saved_h;
    resize_window(win, w, h, pixel_format, dead);
}

/// Scale RGB image and convert to native framebuffer pixel format (4 bytes/pixel).
fn scale_wallpaper(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    bgr: bool,
) -> Vec<u8> {
    let mut dst = vec![0u8; dst_w * dst_h * 4];
    for y in 0..dst_h {
        let sy = y * src_h / dst_h;
        for x in 0..dst_w {
            let sx = x * src_w / dst_w;
            let si = (sy * src_w + sx) * 3;
            let di = (y * dst_w + x) * 4;
            if bgr {
                dst[di] = src[si + 2];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si];
            } else {
                dst[di] = src[si];
                dst[di + 1] = src[si + 1];
                dst[di + 2] = src[si + 2];
            }
        }
    }
    dst
}

fn draw_icon_centered(
    screen: &Framebuffer,
    icon: &sprite::Sprite,
    area_x: usize,
    area_y: usize,
    area_w: usize,
    area_h: usize,
) {
    let ix = area_x + area_w.saturating_sub(icon.width()) / 2;
    let iy = area_y + area_h.saturating_sub(icon.height()) / 2;
    icon.draw(screen.ptr(), screen.stride(), screen.width(), screen.height(), screen.pixel_format_raw(), ix, iy);
}

fn launcher_rect(windows: &[WindowState], screen_h: i32) -> (i32, i32, i32, i32) {
    let lx = (windows.len() * TASKBAR_ITEM_WIDTH) as i32;
    let lh = (LAUNCHER_APPS.len() * LAUNCHER_ITEM_HEIGHT) as i32;
    let ly = screen_h - TASKBAR_HEIGHT as i32 - lh;
    (lx, ly, LAUNCHER_WIDTH as i32, lh)
}

#[derive(Clone, Copy)]
struct DirtyRect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl DirtyRect {
    fn full(screen_w: usize, screen_h: usize) -> Self {
        Self { x: 0, y: 0, w: screen_w, h: screen_h }
    }

    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Self { x, y, w: right - x, h: bottom - y }
    }

    fn clamp(self, screen_w: usize, screen_h: usize) -> Self {
        let x = self.x.min(screen_w);
        let y = self.y.min(screen_h);
        let w = self.w.min(screen_w - x);
        let h = self.h.min(screen_h - y);
        Self { x, y, w, h }
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.w && self.x + self.w > other.x
            && self.y < other.y + other.h && self.y + self.h > other.y
    }
}

fn window_screen_rect(win: &WindowState) -> DirtyRect {
    let x = win.content_x.saturating_sub(BORDER_WIDTH);
    let y = win.content_y.saturating_sub(BORDER_WIDTH + TITLE_BAR_HEIGHT);
    let w = win.width + BORDER_WIDTH * 2;
    let h = win.height + BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;
    DirtyRect { x, y, w, h }
}

fn mark_dirty(dirty_rect: &mut Option<DirtyRect>, r: DirtyRect) {
    *dirty_rect = Some(match dirty_rect.take() {
        Some(old) => old.union(r),
        None => r,
    });
}

/// Why a client is going.
///
/// **Every one of these is printed with the pid.** A client is not entitled to
/// end the compositor, but the compositor is not entitled to make one vanish
/// without saying so either: the log is the only place the machine this runs
/// on can be asked what happened.
#[derive(Clone, Copy)]
enum DropReason {
    /// A frame no protocol here can produce. The next message boundary is
    /// unlocatable, so there is nothing to resynchronise to.
    OutOfProtocol,
    /// Its pipe would not take a whole frame — an entire pipe of messages it
    /// has not read.
    NotReading,
    /// The connection is gone.
    Gone,
    /// Accepted, and never completed a first frame.
    HandshakeTimeout,
}

impl DropReason {
    fn why(self) -> &'static str {
        match self {
            Self::OutOfProtocol => "it sent a frame this protocol cannot describe",
            Self::NotReading => "its pipe will not take another message and it is not reading",
            Self::Gone => "its connection is gone",
            Self::HandshakeTimeout => "it never finished its first message",
        }
    }
}

impl From<ipc::TrySendError> for DropReason {
    fn from(e: ipc::TrySendError) -> Self {
        match e {
            ipc::TrySendError::Full => Self::NotReading,
            _ => Self::Gone,
        }
    }
}

/// A client the next removal pass will take out.
type Dead = (Fd, u32, DropReason);

fn mark_dead(dead: &mut Vec<Dead>, fd: Fd, pid: u32, reason: DropReason) {
    if !dead.iter().any(|(f, _, _)| *f == fd) {
        dead.push((fd, pid, reason));
    }
}

/// Hand a window a typed frame, or mark it for removal.
///
/// A failure is never retried and never ignored: `TrySendError::Full` can have
/// left part of the frame in the pipe, so the peer's stream is past saving —
/// which is the price of never blocking on it.
fn deliver<T: ipc::IpcPayload>(dead: &mut Vec<Dead>, win: &WindowState, msg_type: u32, payload: &T) {
    if let Err(e) = win.fd.try_send(msg_type, payload) {
        mark_dead(dead, win.fd.fd(), win.pid, e.into());
    }
}

/// [`deliver`] for a message that is only its own header.
fn deliver_signal(dead: &mut Vec<Dead>, win: &WindowState, msg_type: u32) {
    if let Err(e) = win.fd.try_signal(msg_type) {
        mark_dead(dead, win.fd.fd(), win.pid, e.into());
    }
}

fn hit_test(windows: &[WindowState], x: i32, y: i32, screen_h: i32, launcher_open: bool) -> HitZone {
    // Launcher popup
    if launcher_open {
        let (lx, ly, lw, lh) = launcher_rect(windows, screen_h);
        if x >= lx && x < lx + lw && y >= ly && y < ly + lh {
            let item = ((y - ly) / LAUNCHER_ITEM_HEIGHT as i32) as usize;
            if item < LAUNCHER_APPS.len() {
                return HitZone::LauncherItem(item);
            }
        }
    }

    // Taskbar at bottom of screen
    if y >= screen_h - TASKBAR_HEIGHT as i32 {
        let tab_x = x as usize / TASKBAR_ITEM_WIDTH;
        if tab_x < windows.len() {
            return HitZone::TaskbarItem(tab_x);
        }
        let new_x = windows.len() * TASKBAR_ITEM_WIDTH;
        if x >= new_x as i32 && x < (new_x + TASKBAR_HEIGHT) as i32 {
            return HitZone::TaskbarNew;
        }
        return HitZone::Desktop;
    }

    for (idx, win) in windows.iter().enumerate().rev() {
        if win.minimized {
            continue;
        }

        let win_x = win.content_x as i32 - BORDER_WIDTH as i32;
        let win_y = win.content_y as i32 - BORDER_WIDTH as i32 - TITLE_BAR_HEIGHT as i32;
        let win_w = win.width as i32 + BORDER_WIDTH as i32 * 2;
        let win_h = win.height as i32 + BORDER_WIDTH as i32 * 2 + TITLE_BAR_HEIGHT as i32;

        if x >= win_x && x < win_x + win_w && y >= win_y && y < win_y + win_h {
            let title_y_end = win_y + BORDER_WIDTH as i32 + TITLE_BAR_HEIGHT as i32;

            // Buttons from right: close, maximize, minimize
            let close_x = win_x + win_w - BORDER_WIDTH as i32 - BUTTON_WIDTH as i32;
            if x >= close_x && y < title_y_end {
                return HitZone::CloseButton(idx);
            }
            let max_x = close_x - BUTTON_WIDTH as i32;
            if x >= max_x && x < close_x && y < title_y_end {
                return HitZone::MaximizeButton(idx);
            }
            let min_x = max_x - BUTTON_WIDTH as i32;
            if x >= min_x && x < max_x && y < title_y_end {
                return HitZone::MinimizeButton(idx);
            }

            // Resize corner (not for snapped/maximized windows)
            if win.mode == WindowMode::Normal {
                let corner_x = win_x + win_w - RESIZE_HANDLE_SIZE as i32;
                let corner_y = win_y + win_h - RESIZE_HANDLE_SIZE as i32;
                if x >= corner_x && y >= corner_y {
                    return HitZone::ResizeCorner(idx);
                }
            }

            if y < title_y_end {
                return HitZone::TitleBar(idx);
            }
            return HitZone::Content(idx);
        }
    }
    HitZone::Desktop
}

enum Interaction {
    None,
    DragPending {
        window_idx: usize,
        start_x: i32,
        start_y: i32,
        was_maximized: bool,
    },
    Dragging { window_idx: usize },
    Resizing { window_idx: usize },
}

const DRAG_THRESHOLD: i32 = 5;

fn redraw(
    screen: &Framebuffer,
    font: &font::Font,
    windows: &[WindowState],
    icons: &TitleBarIcons,
    wallpaper: &[u8],
    launcher_open: bool,
    stats: &SystemStats,
    region: DirtyRect,
) {
    // Only blit wallpaper within the dirty region
    let wp_offset = (region.y * screen.width() + region.x) * 4;
    screen.blit(region.x, region.y, region.w, region.h, screen.width(), &wallpaper[wp_offset..]);

    let focused_idx = focused_window_idx(windows);

    for (i, win) in windows.iter().enumerate() {
        if win.minimized {
            continue;
        }
        if region.overlaps(window_screen_rect(win)) {
            draw_window(screen, font, win, Some(i) == focused_idx, icons, region);
        }
    }

    let taskbar_rect = DirtyRect { x: 0, y: screen.height() - TASKBAR_HEIGHT, w: screen.width(), h: TASKBAR_HEIGHT };
    if region.overlaps(taskbar_rect) {
        draw_taskbar(screen, font, windows, focused_idx, stats);
    }

    // Draw launcher popup last so it's always on top of windows
    if launcher_open {
        let (lx, ly, lw, lh) = launcher_rect(windows, screen.height() as i32);
        let launcher_dirty = DirtyRect { x: lx as usize, y: ly as usize, w: lw as usize, h: lh as usize };
        if region.overlaps(launcher_dirty) {
            draw_launcher(screen, font, lx as usize, ly as usize, lw as usize, lh as usize);
        }
    }
}

fn draw_window(
    screen: &Framebuffer,
    font: &font::Font,
    win: &WindowState,
    focused: bool,
    icons: &TitleBarIcons,
    clip: DirtyRect,
) {
    let border_color = if focused { FOCUSED_BORDER_COLOR } else { UNFOCUSED_BORDER_COLOR };
    let title_color = if focused { FOCUSED_TITLE_COLOR } else { UNFOCUSED_TITLE_COLOR };
    let text_color = if focused { FOCUSED_TITLE_TEXT } else { UNFOCUSED_TITLE_TEXT };

    let win_x = win.content_x - BORDER_WIDTH;
    let win_y = win.content_y - BORDER_WIDTH - TITLE_BAR_HEIGHT;
    let win_w = win.width + BORDER_WIDTH * 2;

    let title_bar = DirtyRect { x: win_x, y: win_y, w: win_w, h: BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT };
    if clip.overlaps(title_bar) {
        screen.fill_rect(win_x, win_y, win_w, BORDER_WIDTH + TITLE_BAR_HEIGHT, border_color);
        screen.fill_rect(
            win_x + BORDER_WIDTH,
            win_y + BORDER_WIDTH,
            win_w - BORDER_WIDTH * 2,
            TITLE_BAR_HEIGHT,
            title_color,
        );

        let title_x = win_x + BORDER_WIDTH + 8;
        let title_y = win_y + BORDER_WIDTH + (TITLE_BAR_HEIGHT - 16) / 2;
        let title = if win.title.is_empty() { "Window" } else { &win.title };
        font.draw_string(screen, title_x, title_y, title, text_color, title_color);

        let close_x = win_x + win_w - BORDER_WIDTH - BUTTON_WIDTH;
        let close_bg = if focused { CLOSE_BUTTON_BG } else { title_color };
        screen.fill_rect(close_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT, close_bg);
        draw_icon_centered(screen, &icons.close, close_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT);

        let max_x = close_x - BUTTON_WIDTH;
        screen.fill_rect(max_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT, title_color);
        draw_icon_centered(screen, &icons.maximize, max_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT);

        let min_x = max_x - BUTTON_WIDTH;
        screen.fill_rect(min_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT, title_color);
        draw_icon_centered(screen, &icons.minimize, min_x, win_y + BORDER_WIDTH, BUTTON_WIDTH, TITLE_BAR_HEIGHT);
    }

    // Draw side/bottom borders only if clip overlaps them
    let content_bottom = win.content_y + win.height;
    if win.content_y < content_bottom {
        // Left border
        screen.fill_rect(win_x, win.content_y, BORDER_WIDTH, win.height, border_color);
        // Right border
        screen.fill_rect(win.content_x + win.width, win.content_y, BORDER_WIDTH, win.height, border_color);
    }
    // Bottom border
    screen.fill_rect(win_x, content_bottom, win_w, BORDER_WIDTH, border_color);

    // Clip content blit to dirty region
    let blit_w = win.width.min(win.buf_width);
    let blit_h = win.height.min(win.buf_height);
    let cx = win.content_x.max(clip.x);
    let cy = win.content_y.max(clip.y);
    let cr = (win.content_x + blit_w).min(clip.x + clip.w);
    let cb = (win.content_y + blit_h).min(clip.y + clip.h);
    if cx < cr && cy < cb {
        let src_x = cx - win.content_x;
        let src_y = cy - win.content_y;
        let src_offset = (src_y * win.buf_width + src_x) * 4;
        let buffer_slice = unsafe { std::slice::from_raw_parts(win.shm.as_ptr(), win.shm.len()) };
        screen.blit(cx, cy, cr - cx, cb - cy, win.buf_width, &buffer_slice[src_offset..]);
    }
}

struct SystemStats {
    used_mb: u64,
    total_mb: u64,
    cpu_pct: u64,
}

fn draw_taskbar(
    screen: &Framebuffer,
    font: &font::Font,
    windows: &[WindowState],
    focused_idx: Option<usize>,
    stats: &SystemStats,
) {
    let screen_w = screen.width();
    let screen_h = screen.height();
    let taskbar_y = screen_h - TASKBAR_HEIGHT;
    screen.fill_rect(0, taskbar_y, screen_w, TASKBAR_HEIGHT, TASKBAR_COLOR);

    let text_y = taskbar_y + (TASKBAR_HEIGHT - 16) / 2;

    for (i, win) in windows.iter().enumerate() {
        let focused = Some(i) == focused_idx;
        let tab_x = i * TASKBAR_ITEM_WIDTH;
        let (bg, fg) = if win.minimized {
            (TASKBAR_MINIMIZED_COLOR, TASKBAR_MINIMIZED_TEXT)
        } else if focused {
            (TASKBAR_ACTIVE_COLOR, TASKBAR_ACTIVE_TEXT)
        } else {
            (TASKBAR_COLOR, TASKBAR_TEXT_COLOR)
        };
        screen.fill_rect(
            tab_x + 1,
            taskbar_y + TASKBAR_PADDING,
            TASKBAR_ITEM_WIDTH - 2,
            TASKBAR_HEIGHT - TASKBAR_PADDING * 2,
            bg,
        );
        let max_chars = (TASKBAR_ITEM_WIDTH - 16) / font.width();
        let title = if win.title.is_empty() { "Window" } else { &win.title };
        let display: String = title.chars().take(max_chars).collect();
        font.draw_string(screen, tab_x + 8, text_y, &display, fg, bg);
    }

    // "+" button after window tabs
    let new_x = windows.len() * TASKBAR_ITEM_WIDTH;
    screen.fill_rect(
        new_x + 1,
        taskbar_y + TASKBAR_PADDING,
        TASKBAR_HEIGHT - 2,
        TASKBAR_HEIGHT - TASKBAR_PADDING * 2,
        TASKBAR_NEW_COLOR,
    );
    let plus_x = new_x + (TASKBAR_HEIGHT - 8) / 2;
    font.draw_char(screen, plus_x, text_y, '+', TASKBAR_NEW_TEXT, TASKBAR_NEW_COLOR);

    // System stats + clock on the right
    let time = system::clock_realtime();
    let hours = time.hours;
    let minutes = time.minutes;

    let status_str = format!(
        "{}M/{}M  CPU {}%  {:02}:{:02}",
        stats.used_mb, stats.total_mb, stats.cpu_pct, hours, minutes
    );
    let status_w = status_str.len() * font.width();
    let status_x = screen_w - status_w - 12;
    font.draw_string(screen, status_x, text_y, &status_str, TASKBAR_ACTIVE_TEXT, TASKBAR_COLOR);

}

fn draw_launcher(screen: &Framebuffer, font: &font::Font, x: usize, y: usize, w: usize, h: usize) {
    screen.fill_rect(x, y, w, h, LAUNCHER_BG);
    for (i, app) in LAUNCHER_APPS.iter().enumerate() {
        let item_y = y + i * LAUNCHER_ITEM_HEIGHT;
        let text_y = item_y + (LAUNCHER_ITEM_HEIGHT - 16) / 2;
        font.draw_string(screen, x + 12, text_y, app.name, LAUNCHER_TEXT, LAUNCHER_BG);
    }
}

/// Render the cursor sprite (RGBA) into a 64x64 BGRA hardware cursor buffer.
fn upload_cursor(cursor_buf: *mut u8, sprite: &sprite::Sprite, hw_cursor: bool) {
    let data = sprite.data();
    let w = sprite.width();
    let h = sprite.height();
    // Clear the full 64x64 buffer
    unsafe { core::ptr::write_bytes(cursor_buf, 0, 64 * 64 * 4); }
    // Copy sprite pixels, converting RGBA → BGRA
    for y in 0..h.min(64) {
        for x in 0..w.min(64) {
            let si = (y * w + x) * 4;
            let di = (y * 64 + x) * 4;
            unsafe {
                let dst = cursor_buf.add(di);
                *dst = data[si + 2];       // B
                *dst.add(1) = data[si + 1]; // G
                *dst.add(2) = data[si];     // R
                *dst.add(3) = data[si + 3]; // A
            }
        }
    }
    if hw_cursor {
        gpu::set_cursor(0, 0).expect("compositor owns the framebuffer");
    }
}

/// Draw the cursor sprite directly into the framebuffer (software cursor fallback).
fn draw_software_cursor(screen: &Framebuffer, sprite: &sprite::Sprite, cx: i32, cy: i32) {
    let data = sprite.data();
    let sw = sprite.width();
    let sh = sprite.height();
    let screen_w = screen.width();
    let screen_h = screen.height();

    for sy in 0..sh {
        let py = cy as usize + sy;
        if py >= screen_h { break; }
        for sx in 0..sw {
            let px = cx as usize + sx;
            if px >= screen_w { break; }
            let si = (sy * sw + sx) * 4;
            let alpha = data[si + 3] as u32;
            if alpha == 0 { continue; }
            let sr = data[si] as u32;
            let sg = data[si + 1] as u32;
            let sb = data[si + 2] as u32;
            if alpha == 255 {
                screen.put_pixel(px, py, Color { r: sr as u8, g: sg as u8, b: sb as u8 });
            } else {
                let bg = screen.get_pixel(px, py);
                let inv = 255 - alpha;
                let r = ((sr * alpha + bg.r as u32 * inv) / 255) as u8;
                let g = ((sg * alpha + bg.g as u32 * inv) / 255) as u8;
                let b = ((sb * alpha + bg.b as u32 * inv) / 255) as u8;
                screen.put_pixel(px, py, Color { r, g, b });
            }
        }
    }
}

/// Counters for one reporting window, flushed from a composited frame and
/// never otherwise: a compositor with nothing to draw says nothing, as soundd
/// says nothing with no clients.
///
/// Here to be read off `/log/kernel.log` on a machine whose only other
/// instrument is the panel. `scanout_rd_bytes` is what it is for: the
/// compositor composes *against* the scanout, so `fill_rect` reads back every
/// row it replicates and a blended cursor pixel reads the pixel under it, and
/// on hardware — unlike QEMU, where the framebuffer is host RAM — those reads
/// are uncached. Composing in RAM and blitting damage would hold it at zero.
///
/// Both byte figures are lower bounds, and for one reason: they count what
/// went through `Framebuffer`. Glyphs go through `put_pixel`, which is
/// uncounted by design, and the title-bar icons are handed `screen.ptr()` and
/// blend through it — reading the scanout as well as writing it — outside the
/// counters entirely.
#[derive(Default)]
struct FrameStats {
    frames: u32,
    cursor_draws: u32,
    composite_ns_min: u64,
    composite_ns_max: u64,
    composite_ns_total: u64,
}

impl FrameStats {
    /// `composite_ns` covers the redraw and the software cursor, not the
    /// `present` that follows: that one is a syscall, and mixing it in would
    /// hide the cost this is aimed at.
    fn record(&mut self, composite_ns: u64) {
        self.composite_ns_min = if self.frames == 0 {
            composite_ns
        } else {
            self.composite_ns_min.min(composite_ns)
        };
        self.composite_ns_max = self.composite_ns_max.max(composite_ns);
        self.composite_ns_total += composite_ns;
        self.frames += 1;
    }

    /// `moved` is the scanout traffic of this window alone. Totals rather than
    /// a mean: with `frames` beside it the mean is a division, and the total is
    /// the share of the window that compositing cost, which the mean is not.
    fn report(&self, moved: Traffic, windows: usize) {
        eprintln!(
            "compositor: frames={} composite_us_min={} composite_us_max={} composite_us_total={} \
             scanout_wr_bytes={} scanout_rd_bytes={} rd_px={} rd_bulk={} cursor={} windows={}",
            self.frames,
            self.composite_ns_min / 1_000,
            self.composite_ns_max / 1_000,
            self.composite_ns_total / 1_000,
            moved.written,
            moved.read,
            moved.pixel_reads,
            moved.bulk_reads,
            self.cursor_draws,
            windows,
        );
    }
}

fn main() {
    let listener = services::listen("compositor").expect("compositor already running");

    let kb = Keyboard::open().expect("failed to claim keyboard");
    let mouse = Mouse::open().expect("failed to claim mouse");
    let fb_dev = FramebufferDev::open().expect("failed to claim framebuffer");

    let mut fb_info = fb_dev.info().expect("failed to read framebuffer info");
    let fb_size = fb_info.stride as usize * fb_info.height as usize * 4;
    let mut fb_shm = SharedMemory::map(fb_info.token[0], fb_size);
    let mut screen = Framebuffer::new(
        fb_shm.as_ptr(),
        fb_info.width as usize,
        fb_info.height as usize,
        fb_info.stride as usize,
        fb_info.pixel_format,
    );

    // Set up cursor
    let hw_cursor = fb_info.flags & FLAG_HARDWARE_CURSOR != 0;
    let cursor_shm = SharedMemory::map(fb_info.cursor_token, 64 * 64 * 4);
    let cursor_buf = cursor_shm.as_ptr();
    let cursor_svg = std::fs::read("/share/icons/cursor-bold.svg").expect("failed to read cursor");
    let cursor_default = sprite::Sprite::from_svg_colored(&cursor_svg, 20, [255, 255, 255]);
    let resize_svg =
        std::fs::read("/share/icons/arrow-down-right-bold.svg").expect("failed to read resize cursor");
    let cursor_resize = sprite::Sprite::from_svg_colored(&resize_svg, 20, [255, 255, 255]);
    let crosshair_svg =
        std::fs::read("/share/icons/crosshair-simple-bold.svg").expect("failed to read crosshair cursor");
    let cursor_crosshair = sprite::Sprite::from_svg_colored(&crosshair_svg, 20, [0, 0, 0]);
    upload_cursor(cursor_buf, &cursor_default, hw_cursor);
    let mut current_cursor_style: u8 = window::CURSOR_DEFAULT;

    let font_data = std::fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font").expect("failed to read font");
    let font = font::Font::from_prebuilt(&font_data);

    let wallpaper_raw = std::fs::read("/share/wallpaper.rgb").expect("failed to read wallpaper");
    let wallpaper_w = u32::from_le_bytes(wallpaper_raw[0..4].try_into().unwrap()) as usize;
    let wallpaper_h = u32::from_le_bytes(wallpaper_raw[4..8].try_into().unwrap()) as usize;
    let wallpaper_pixels = &wallpaper_raw[8..];
    eprintln!(
        "compositor: wallpaper {}x{}, scaling to {}x{}",
        wallpaper_w, wallpaper_h, screen.width(), screen.height()
    );
    let mut wallpaper = scale_wallpaper(
        wallpaper_pixels,
        wallpaper_w,
        wallpaper_h,
        screen.width(),
        screen.height(),
        screen.pixel_format_raw() != 0,
    );

    let icons = TitleBarIcons {
        minimize: sprite::Sprite::from_svg_colored(
            &std::fs::read("/share/icons/minus-bold.svg").expect("failed to read minimize icon"),
            14,
            [255, 255, 255],
        ),
        maximize: sprite::Sprite::from_svg_colored(
            &std::fs::read("/share/icons/square-bold.svg").expect("failed to read maximize icon"),
            14,
            [255, 255, 255],
        ),
        close: sprite::Sprite::from_svg_colored(
            &std::fs::read("/share/icons/x-bold.svg").expect("failed to read close icon"),
            14,
            [255, 255, 255],
        ),
    };

    eprintln!("compositor: ready");

    let mut windows: Vec<WindowState> = Vec::new();
    let mut screen_w = screen.width() as i32;
    let mut screen_h = screen.height() as i32;
    let mut cursor_x = screen_w / 2;
    let mut cursor_y = screen_h / 2;
    if hw_cursor {
        gpu::move_cursor(cursor_x as u32, cursor_y as u32).expect("compositor owns the framebuffer");
    }
    let mut dirty_rect: Option<DirtyRect> = Some(DirtyRect::full(screen_w as usize, screen_h as usize));
    let mut prev_buttons: u8 = 0;
    let mut interaction = Interaction::None;
    let mut last_title_click_time = Instant::now();
    let mut last_title_click_fd: Option<Fd> = None;
    let mut clipboard = String::new();
    Command::new("/bin/filepicker").spawn().ok();
    let mut launcher_open = false;
    let mut prev_busy_ticks: u64 = 0;
    let mut prev_total_ticks: u64 = 0;
    let mut cpu_pct: u64 = 0;
    let mut last_taskbar_update = Instant::now();
    let mut cached_stats = SystemStats { used_mb: 0, total_mb: 0, cpu_pct: 0 };
    let mut frame_stats = FrameStats::default();
    let mut reported_traffic = screen.traffic();
    let mut next_frame_stats = Instant::now() + STATS_INTERVAL;

    // Every token is its fd number, system and client alike; the dispatch
    // below tells them apart by comparing against the known system fds.
    let token_kb = kb.fd().0 as u64;
    let token_mouse = mouse.fd().0 as u64;
    let token_listener = listener.fd().0 as u64;

    let total_mem = total_memory();
    let mut max_windows = max_windows(total_mem, screen.width(), screen.height());
    eprintln!(
        "compositor: at most {max_windows} windows ({} MiB each of {} MiB total)",
        window_bytes(screen.width(), screen.height()) / (1024 * 1024),
        total_mem / (1024 * 1024),
    );

    // Sized for the slot ceiling rather than for `max_windows`: the batch
    // between two `wait` calls is the three fixed fds, one per live window and
    // one per pending connection, and `MSG_SET_RESOLUTION` can raise
    // `max_windows` mid-run.
    let poller = Poller::new(FIXED_POLL_FDS + MAX_WINDOW_SLOTS + MAX_PENDING_CONNS);

    poller.poll_add(&kb, IORING_POLL_IN, token_kb);
    poller.poll_add(&mouse, IORING_POLL_IN, token_mouse);
    poller.poll_add(&listener, IORING_POLL_IN, token_listener);

    let mut pending: Vec<PendingConn> = Vec::new();

    loop {
        // Drain all pending events before compositing
        let mut waited = false;
        let drain_until = Instant::now() + DRAIN_BUDGET;
        loop {
            let timeout = if waited { Duration::from_nanos(1) } else { FRAME_INTERVAL };

            let mut ready_tokens: Vec<u64> = Vec::new();
            poller.wait(1, timeout.as_nanos() as u64, |token| {
                ready_tokens.push(token);
            });

            let kb_ready = ready_tokens.contains(&token_kb);
            let mouse_ready = ready_tokens.contains(&token_mouse);
            let listener_ready = ready_tokens.contains(&token_listener);
            let any_client_ready = windows.iter().any(|w| ready_tokens.contains(&(w.fd.fd().0 as u64)))
                || pending.iter().any(|p| ready_tokens.contains(&(p.conn.fd().0 as u64)));

            // A handshake that never completes is the reason this deadline
            // exists, and the sweep has to happen on a pass that found nothing
            // ready too — otherwise a silent client is only ever timed out by
            // some *other* client's traffic.
            let now = Instant::now();
            for p in pending.iter().filter(|p| now.duration_since(p.since) >= HANDSHAKE_TIMEOUT) {
                eprintln!(
                    "compositor: dropping pid {} — {}",
                    p.pid,
                    DropReason::HandshakeTimeout.why()
                );
            }
            pending.retain(|p| now.duration_since(p.since) < HANDSHAKE_TIMEOUT);

            if !kb_ready && !mouse_ready && !listener_ready && !any_client_ready {
                break;
            }
            waited = true;
            let mut dead: Vec<Dead> = Vec::new();

        if kb_ready {
            let mut events = [window::KeyEvent::EMPTY; 8];
            let buf = unsafe {
                std::slice::from_raw_parts_mut(
                    events.as_mut_ptr() as *mut u8,
                    std::mem::size_of_val(&events),
                )
            };
            // Never read blocking here. The kernel wakes only when a report
            // queued an event, so readiness and `has_data` agree — but a
            // blocking read on an empty queue parks the compositor until the
            // next real key, and one spurious wake anywhere would freeze it.
            let n = kb.read_nonblock(buf).unwrap_or(0);
            for event in &events[..n / std::mem::size_of::<window::KeyEvent>()] {
                if launcher_open && event.pressed() && event.keycode == 0x29 {
                    // Escape: close launcher
                    launcher_open = false;
                    mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                } else if event.pressed() && event.alt() && event.keycode == 0x2B {
                    // Alt+Tab: rotate focus among non-topmost windows
                    let first_topmost = windows.iter().position(|w| w.topmost).unwrap_or(windows.len());
                    if first_topmost > 1 {
                        let win = windows.remove(first_topmost - 1);
                        windows.insert(0, win);
                        let first_topmost = windows.iter().position(|w| w.topmost).unwrap_or(windows.len());
                        if first_topmost > 0 {
                            windows[first_topmost - 1].minimized = false;
                        }
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                } else if event.pressed() && event.gui() {
                    if let Some(idx) = focused_window_idx(&windows) {
                        let pixel_format = screen.pixel_format_raw();
                        match event.keycode {
                            0x50 => {
                                // Super+Left: snap left or restore
                                if windows[idx].mode == WindowMode::SnappedLeft {
                                    restore_window(&mut windows[idx], pixel_format, &mut dead);
                                } else {
                                    snap_left(
                                        &mut windows[idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                        &mut dead,
                                    );
                                }
                            }
                            0x4F => {
                                // Super+Right: snap right or restore
                                if windows[idx].mode == WindowMode::SnappedRight {
                                    restore_window(&mut windows[idx], pixel_format, &mut dead);
                                } else {
                                    snap_right(
                                        &mut windows[idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                        &mut dead,
                                    );
                                }
                            }
                            0x52 => {
                                // Super+Up: maximize or restore
                                if windows[idx].mode == WindowMode::Maximized {
                                    restore_window(&mut windows[idx], pixel_format, &mut dead);
                                } else {
                                    maximize_window(
                                        &mut windows[idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                        &mut dead,
                                    );
                                }
                            }
                            0x51 => {
                                // Super+Down: restore or minimize
                                if windows[idx].mode != WindowMode::Normal {
                                    restore_window(&mut windows[idx], pixel_format, &mut dead);
                                } else {
                                    windows[idx].minimized = true;
                                }
                            }
                            0x14 => {
                                // GUI+Q: close focused window
                                let win = windows.remove(idx);
                                let _ = win.fd.try_signal(window::MSG_WINDOW_CLOSE);
                            }
                            0x19 => {
                                // GUI+V: paste clipboard
                                if !clipboard.is_empty() {
                                    let win = &windows[idx];
                                    if clipboard.len() <= 4096 {
                                        if let Err(e) = win.fd.try_send_bytes(
                                            window::MSG_CLIPBOARD_PASTE,
                                            clipboard.as_bytes(),
                                        ) {
                                            mark_dead(&mut dead, win.fd.fd(), win.pid, e.into());
                                        }
                                    } else {
                                        static PASTE_SHM: std::sync::Mutex<Option<SharedMemory>> =
                                            std::sync::Mutex::new(None);
                                        let mut shm = SharedMemory::allocate(clipboard.len());
                                        shm.as_mut_slice()[..clipboard.len()]
                                            .copy_from_slice(clipboard.as_bytes());
                                        deliver(
                                            &mut dead,
                                            win,
                                            window::MSG_CLIPBOARD_PASTE_SHM,
                                            &window::ClipboardShmMsg {
                                                token: shm.token(),
                                                len: clipboard.len() as u32,
                                            },
                                        );
                                        *PASTE_SHM.lock().unwrap() = Some(shm);
                                    }
                                }
                            }
                            _ => {
                                // Forward other GUI combos to focused app
                                deliver(&mut dead, &windows[idx], window::MSG_KEY_INPUT, event);
                            }
                        }
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                } else if event.pressed() && event.ctrl() && event.keycode == 0x11 {
                    // Ctrl+N: spawn terminal
                    Command::new("/bin/terminal").spawn().ok();
                } else {
                    if let Some(idx) = focused_window_idx(&windows) {
                        deliver(&mut dead, &windows[idx], window::MSG_KEY_INPUT, event);
                    }
                }
            }
        }

        if mouse_ready {
            // Drain all pending mouse events in one read
            let mut buf = [0u8; 512];
            let n = mouse.read_nonblock(&mut buf).unwrap_or(0);
            let event_size = 6; // MouseEvent: buttons(1) + scroll(1) + abs_x(2) + abs_y(2)
            let event_count = n / event_size;

            // Track button transitions and last absolute position
            let mut total_scroll: i32 = 0;
            let mut last_abs_x: u16 = 0;
            let mut last_abs_y: u16 = 0;
            let mut buttons = prev_buttons;
            let mut press_happened = false;
            let mut release_happened = false;

            for i in 0..event_count {
                let off = i * event_size;
                let new_buttons = buf[off];
                let scroll = buf[off + 1] as i8;
                last_abs_x = u16::from_le_bytes([buf[off + 2], buf[off + 3]]);
                last_abs_y = u16::from_le_bytes([buf[off + 4], buf[off + 5]]);
                total_scroll += scroll as i32;

                let new_left = new_buttons & 1 != 0;
                let old_left = buttons & 1 != 0;
                if new_left && !old_left { press_happened = true; }
                if !new_left && old_left { release_happened = true; }
                buttons = new_buttons;
            }

            if event_count > 0 {
                let left = buttons & 1 != 0;

                // Convert absolute tablet coordinates (0–32767) to screen coordinates
                let old_cursor_x = cursor_x;
                let old_cursor_y = cursor_y;
                cursor_x = (last_abs_x as i32 * screen_w / 32768).clamp(0, screen_w - 1);
                cursor_y = (last_abs_y as i32 * screen_h / 32768).clamp(0, screen_h - 1);
                if hw_cursor {
                    gpu::move_cursor(cursor_x as u32, cursor_y as u32).expect("compositor owns the framebuffer");
                } else {
                    // Software cursor: mark old and new cursor regions dirty
                    let cw = 20usize;
                    let ch = 20usize;
                    mark_dirty(&mut dirty_rect, DirtyRect { x: old_cursor_x as usize, y: old_cursor_y as usize, w: cw, h: ch });
                    mark_dirty(&mut dirty_rect, DirtyRect { x: cursor_x as usize, y: cursor_y as usize, w: cw, h: ch });
                }

                // Cursor deltas for drag/resize operations
                let cursor_dx = cursor_x - old_cursor_x;
                let cursor_dy = cursor_y - old_cursor_y;

                // Update cursor shape
                let want_resize = match interaction {
                    Interaction::Resizing { .. } => true,
                    _ => matches!(
                        hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open),
                        HitZone::ResizeCorner(_)
                    ),
                };
                let wanted_cursor = if want_resize {
                    window::CURSOR_RESIZE
                } else if let Some(idx) = focused_window_idx(&windows) {
                    let hz = hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open);
                    if matches!(hz, HitZone::Content(_)) {
                        windows[idx].cursor_style
                    } else {
                        window::CURSOR_DEFAULT
                    }
                } else {
                    window::CURSOR_DEFAULT
                };
                if wanted_cursor != current_cursor_style {
                    current_cursor_style = wanted_cursor;
                    let sprite = match wanted_cursor {
                        window::CURSOR_CROSSHAIR => &cursor_crosshair,
                        window::CURSOR_RESIZE => &cursor_resize,
                        _ => &cursor_default,
                    };
                    upload_cursor(cursor_buf, sprite, hw_cursor);
                }

                let make_mouse_event =
                    |win: &WindowState, event_type: u8, changed: u8, scroll: i8| {
                        let local_x = (cursor_x - win.content_x as i32).max(0) as u16;
                        let local_y = (cursor_y - win.content_y as i32).max(0) as u16;
                        window::MouseEvent {
                            x: local_x,
                            y: local_y,
                            buttons,
                            event_type,
                            changed,
                            scroll,
                        }
                    };

                // Left button pressed during this batch
                if press_happened {
                    match hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open) {
                        HitZone::CloseButton(idx) => {
                            let win = windows.remove(idx);
                            let _ = win.fd.try_signal(window::MSG_WINDOW_CLOSE);
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::MinimizeButton(idx) => {
                            windows[idx].minimized = true;
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::MaximizeButton(idx) => {
                            let new_idx = bring_to_front(&mut windows, idx);
                            let pixel_format = screen.pixel_format_raw();
                            if windows[new_idx].mode != WindowMode::Normal {
                                restore_window(&mut windows[new_idx], pixel_format, &mut dead);
                            } else {
                                maximize_window(
                                    &mut windows[new_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                    &mut dead,
                                );
                            }
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::TitleBar(idx) => {
                            let new_idx = bring_to_front(&mut windows, idx);

                            // Double-click detection
                            let now = Instant::now();
                            let win_fd = windows[new_idx].fd.fd();
                            if Some(win_fd) == last_title_click_fd
                                && now.duration_since(last_title_click_time) < DOUBLE_CLICK_TIME
                            {
                                let pixel_format = screen.pixel_format_raw();
                                if windows[new_idx].mode != WindowMode::Normal {
                                    restore_window(&mut windows[new_idx], pixel_format, &mut dead);
                                } else {
                                    maximize_window(
                                        &mut windows[new_idx],
                                        screen_w as usize,
                                        screen_h as usize,
                                        pixel_format,
                                        &mut dead,
                                    );
                                }
                                last_title_click_fd = None;
                                last_title_click_time = now - DOUBLE_CLICK_TIME;
                            } else {
                                last_title_click_fd = Some(win_fd);
                                last_title_click_time = now;
                                if windows[new_idx].mode != WindowMode::Normal {
                                    // Defer unmaximize until drag threshold is exceeded
                                    interaction = Interaction::DragPending {
                                        window_idx: new_idx,
                                        start_x: cursor_x,
                                        start_y: cursor_y,
                                        was_maximized: true,
                                    };
                                } else {
                                    interaction = Interaction::Dragging {
                                        window_idx: new_idx,
                                    };
                                }
                            }
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::ResizeCorner(idx) => {
                            let new_idx = bring_to_front(&mut windows, idx);
                            interaction = Interaction::Resizing {
                                window_idx: new_idx,
                            };
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::Content(idx) => {
                            launcher_open = false;
                            let new_idx = bring_to_front(&mut windows, idx);
                            if new_idx != idx {
                                mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                            }
                            let win = &windows[new_idx];
                            let ev = make_mouse_event(win, window::MOUSE_PRESS, 1, 0);
                            deliver(&mut dead, win, window::MSG_MOUSE_INPUT, &ev);
                        }
                        HitZone::TaskbarItem(idx) => {
                            if idx < windows.len() {
                                if windows[idx].minimized {
                                    windows[idx].minimized = false;
                                    bring_to_front(&mut windows, idx);
                                } else if Some(idx) == focused_window_idx(&windows) {
                                    windows[idx].minimized = true;
                                } else {
                                    bring_to_front(&mut windows, idx);
                                }
                                mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                            }
                        }
                        HitZone::TaskbarNew => {
                            launcher_open = !launcher_open;
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::LauncherItem(idx) => {
                            Command::new(LAUNCHER_APPS[idx].path).spawn().ok();
                            launcher_open = false;
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        HitZone::Desktop => {
                            if launcher_open {
                                launcher_open = false;
                                mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                            }
                        }
                    }
                }

                // Left button released during this batch
                if release_happened {
                    if let Some(idx) = focused_window_idx(&windows) {
                        let ev = make_mouse_event(&windows[idx], window::MOUSE_RELEASE, 1, 0);
                        deliver(&mut dead, &windows[idx], window::MSG_MOUSE_INPUT, &ev);
                    }
                    match interaction {
                        Interaction::DragPending { .. } => {
                            // Click without dragging — just focus, don't unmaximize
                        }
                        Interaction::Dragging { window_idx } => {
                            // Snap detection on drag release
                            let pixel_format = screen.pixel_format_raw();
                            if cursor_x <= 2 {
                                snap_left(
                                    &mut windows[window_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                    &mut dead,
                                );
                            } else if cursor_x >= screen_w - 3 {
                                snap_right(
                                    &mut windows[window_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                    &mut dead,
                                );
                            } else if cursor_y <= 2 {
                                maximize_window(
                                    &mut windows[window_idx],
                                    screen_w as usize,
                                    screen_h as usize,
                                    pixel_format,
                                    &mut dead,
                                );
                            }
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        Interaction::Resizing { window_idx } => {
                            let pixel_format = screen.pixel_format_raw();
                            let win = &mut windows[window_idx];
                            let new_w = win.width;
                            let new_h = win.height;
                            resize_window(win, new_w, new_h, pixel_format, &mut dead);
                            mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                        }
                        Interaction::None => {}
                    }
                    interaction = Interaction::None;
                }

                // Drag / resize with accumulated deltas
                if left {
                    match interaction {
                        Interaction::DragPending {
                            window_idx,
                            start_x,
                            start_y,
                            was_maximized,
                        } => {
                            let dx = cursor_x - start_x;
                            let dy = cursor_y - start_y;
                            if dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD {
                                if was_maximized {
                                    let pixel_format = screen.pixel_format_raw();
                                    let win = &mut windows[window_idx];
                                    // Remember old maximized width for proportional cursor placement
                                    let old_width = win.width + 2 * BORDER_WIDTH;
                                    restore_window(win, pixel_format, &mut dead);
                                    let win = &mut windows[window_idx];
                                    let new_width = win.width + 2 * BORDER_WIDTH;
                                    // Place cursor proportionally on the restored title bar
                                    let ratio = (start_x as usize).min(old_width) as f32
                                        / old_width as f32;
                                    win.content_x = (cursor_x as usize)
                                        .saturating_sub((new_width as f32 * ratio) as usize)
                                        .max(BORDER_WIDTH);
                                    win.content_y = (cursor_y as usize)
                                        .max(BORDER_WIDTH + TITLE_BAR_HEIGHT);
                                    mark_dirty(
                                        &mut dirty_rect,
                                        DirtyRect::full(screen_w as usize, screen_h as usize),
                                    );
                                }
                                interaction = Interaction::Dragging { window_idx };
                            }
                        }
                        Interaction::Dragging { window_idx } => {
                            let old_rect = window_screen_rect(&windows[window_idx]);
                            let win = &mut windows[window_idx];
                            let min_x = BORDER_WIDTH as i32;
                            let min_y = (BORDER_WIDTH + TITLE_BAR_HEIGHT) as i32;
                            win.content_x =
                                (win.content_x as i32 + cursor_dx).max(min_x) as usize;
                            win.content_y =
                                (win.content_y as i32 + cursor_dy).max(min_y) as usize;
                            let new_rect = window_screen_rect(&windows[window_idx]);
                            mark_dirty(&mut dirty_rect, old_rect.union(new_rect));
                        }
                        Interaction::Resizing { window_idx } => {
                            let old_rect = window_screen_rect(&windows[window_idx]);
                            let win = &mut windows[window_idx];
                            win.width = (win.width as i32 + cursor_dx)
                                .max(MIN_CONTENT_WIDTH as i32)
                                as usize;
                            win.height = (win.height as i32 + cursor_dy)
                                .max(MIN_CONTENT_HEIGHT as i32)
                                as usize;
                            let new_rect = window_screen_rect(&windows[window_idx]);
                            mark_dirty(&mut dirty_rect, old_rect.union(new_rect));
                        }
                        Interaction::None => {
                            // Forward mouse move to focused app for drag selection
                            if let Some(idx) = focused_window_idx(&windows) {
                                let ev = make_mouse_event(
                                    &windows[idx],
                                    window::MOUSE_MOVE,
                                    0,
                                    0,
                                );
                                deliver(&mut dead, &windows[idx], window::MSG_MOUSE_INPUT, &ev);
                            }
                        }
                    }
                }

                // Scroll with accumulated total
                if total_scroll != 0 {
                    if let Some(idx) = focused_window_idx(&windows) {
                        if let HitZone::Content(_) =
                            hit_test(&windows, cursor_x, cursor_y, screen_h, launcher_open)
                        {
                            let clamped_scroll = total_scroll.clamp(-128, 127) as i8;
                            let ev =
                                make_mouse_event(&windows[idx], window::MOUSE_SCROLL, 0, clamped_scroll);
                            deliver(&mut dead, &windows[idx], window::MSG_MOUSE_INPUT, &ev);
                        }
                    }
                }

                prev_buttons = buttons;
            }
        }

        // Collected first to avoid borrowing `windows` while mutating it. The
        // payload comes with the frame rather than being read off the fd
        // during dispatch: the read side is finished by the time anything acts
        // on a message, so no branch below can park on a peer.
        let mut client_msgs: Vec<ClientFrame> = Vec::new();

        if listener_ready {
            let result = services::accept(&listener).expect("accept failed");
            if pending.len() >= MAX_PENDING_CONNS as usize {
                eprintln!(
                    "compositor: refusing pid {} — {MAX_PENDING_CONNS} connections are already \
                     waiting to say what they want",
                    result.client_pid
                );
            } else {
                poller.poll_add(&result.conn, IORING_POLL_IN, result.conn.fd().0 as u64);
                pending.push(PendingConn {
                    conn: result.conn,
                    pid: result.client_pid,
                    rx: ClientRx::new(),
                    since: Instant::now(),
                });
            }
        }

        for i in 0..pending.len() {
            if !ready_tokens.contains(&(pending[i].conn.fd().0 as u64)) {
                continue;
            }
            let step = {
                let p = &mut pending[i];
                p.rx.pump(&p.conn)
            };
            let fd = pending[i].conn.fd();
            let pid = pending[i].pid;
            match step {
                RxStep::Idle => {}
                RxStep::Eof => mark_dead(&mut dead, fd, pid, DropReason::Gone),
                RxStep::Malformed => mark_dead(&mut dead, fd, pid, DropReason::OutOfProtocol),
                RxStep::Frame { msg_type, payload_len } => {
                    let mut frame = ClientFrame::new(fd, pid, msg_type);
                    frame.set_payload(pending[i].rx.payload(payload_len));
                    // A connection is identified by its first frame and by
                    // nothing else. `MSG_CREATE_WINDOW` promotes it to a
                    // window; anything else is a one-shot request, answered
                    // and closed — which is what a `services::connect` caller
                    // like `window::clipboard_set` expects.
                    //
                    // One promotion per pass keeps `i` meaningful across the
                    // `remove`; the rest are re-armed below and served next
                    // pass.
                    frame.conn = Some(pending.remove(i).conn);
                    client_msgs.push(frame);
                    break;
                }
            }
        }

        for i in 0..windows.len() {
            if !ready_tokens.contains(&(windows[i].fd.fd().0 as u64)) {
                continue;
            }
            let win = &mut windows[i];
            let step = win.rx.pump(&win.fd);
            match step {
                RxStep::Idle => {}
                RxStep::Eof => mark_dead(&mut dead, win.fd.fd(), win.pid, DropReason::Gone),
                RxStep::Malformed => {
                    mark_dead(&mut dead, win.fd.fd(), win.pid, DropReason::OutOfProtocol)
                }
                RxStep::Frame { msg_type, payload_len } => {
                    let mut frame = ClientFrame::new(win.fd.fd(), win.pid, msg_type);
                    frame.set_payload(win.rx.payload(payload_len));
                    client_msgs.push(frame);
                }
            }
        }

        for frame in client_msgs {
            let client_fd = frame.fd;
            let client_pid = frame.pid;
            let new_conn = frame.conn;
            let payload = &frame.payload[..frame.payload_len];
            match frame.msg_type {
                window::MSG_CREATE_WINDOW => {
                    // `new_conn` is dropped by `continue`, which closes the fd:
                    // there is no window to remove yet.
                    let Ok(req) = ipc::decode_payload::<window::CreateWindowRequest>(payload)
                    else {
                        continue;
                    };
                    let title = if req.title_len > 0 {
                        let len = (req.title_len as usize).min(30);
                        String::from_utf8_lossy(&req.title[..len]).into_owned()
                    } else {
                        String::new()
                    };

                    let screen_w = screen.width();
                    let screen_h = screen.height();

                    let req_w = req.width as usize;
                    let req_h = req.height as usize;

                    // Both refusals are answers to untrusted input, so neither
                    // is a panic and neither is a silent shrink of what was
                    // asked for. Without the size check `req` reaches
                    // `SharedMemory::allocate` unbounded, where `alloc_shared`
                    // asserts on a token the kernel refused — one message from
                    // any client, and the compositor is gone.
                    let refusal = if windows.len() >= max_windows {
                        Some(window::REFUSED_AT_CAPACITY)
                    } else if req_w > screen_w || req_h > screen_h {
                        Some(window::REFUSED_TOO_LARGE)
                    } else {
                        None
                    };
                    if let Some(reason) = refusal {
                        eprintln!(
                            "compositor: refusing {req_w}x{req_h} window from pid {client_pid} \
                             ({} live, max {max_windows}), reason {reason}",
                            windows.len()
                        );
                        let _ = ipc::try_send(
                            client_fd,
                            window::MSG_WINDOW_REFUSED,
                            &window::WindowRefused { reason },
                        );
                        continue;
                    }

                    let (win_x, win_y, win_w, win_h);
                    if req_w > 0 && req_h > 0 {
                        let chrome_w = BORDER_WIDTH * 2;
                        let chrome_h = BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;
                        win_w = req_w + chrome_w;
                        win_h = req_h + chrome_h;
                        win_x = (screen_w.saturating_sub(win_w)) / 2;
                        win_y = (screen_h.saturating_sub(win_h + TASKBAR_HEIGHT)) / 2;
                    } else {
                        let offset = CASCADE_OFFSET * (windows.len() % 10);
                        win_x = INITIAL_MARGIN + offset;
                        win_y = INITIAL_MARGIN + offset;
                        win_w = screen_w - INITIAL_MARGIN * 2;
                        win_h = screen_h - INITIAL_MARGIN * 2 - TASKBAR_HEIGHT;
                    }

                    let content_x = win_x + BORDER_WIDTH;
                    let content_y = win_y + BORDER_WIDTH + TITLE_BAR_HEIGHT;
                    let content_w = win_w - BORDER_WIDTH * 2;
                    let content_h = win_h - BORDER_WIDTH * 2 - TITLE_BAR_HEIGHT;

                    let buf_size = content_w * content_h * 4;
                    let shm = SharedMemory::allocate(buf_size);
                    shm.grant(client_pid);
                    let token = shm.token();
                    let pixel_format = screen.pixel_format_raw();

                    let topmost = req.flags & window::WINDOW_FLAG_TOPMOST != 0;
                    let conn = new_conn.expect("MSG_CREATE_WINDOW without Connection");
                    windows.push(WindowState {
                        fd: conn,
                        pid: client_pid,
                        shm,
                        content_x,
                        content_y,
                        width: content_w,
                        height: content_h,
                        buf_width: content_w,
                        buf_height: content_h,
                        title,
                        minimized: false,
                        topmost,
                        mode: WindowMode::Normal,
                        saved_x: 0,
                        saved_y: 0,
                        saved_w: 0,
                        saved_h: 0,
                        presented: false,
                        cursor_style: window::CURSOR_DEFAULT,
                        rx: ClientRx::new(),
                    });

                    // Register new client fd in io_uring (token = fd number)
                    poller.poll_add(&windows.last().unwrap().fd, IORING_POLL_IN, client_fd.0 as u64);

                    deliver(
                        &mut dead,
                        windows.last().unwrap(),
                        window::MSG_WINDOW_CREATED,
                        &window::WindowInfo {
                            token,
                            width: content_w as u32,
                            height: content_h as u32,
                            stride: content_w as u32,
                            pixel_format,
                        },
                    );
                    mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w, screen_h));
                }
                window::MSG_PRESENT => {
                    if let Some(win) = windows.iter_mut().find(|w| w.fd.fd() == client_fd) {
                        win.presented = true;
                        mark_dirty(&mut dirty_rect, window_screen_rect(win));
                    }
                }
                window::MSG_DESTROY_WINDOW => {
                    if let Some(idx) = windows.iter().position(|w| w.fd.fd() == client_fd) {
                        windows.remove(idx);
                        mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                    }
                }
                window::MSG_CLIPBOARD_SET => {
                    clipboard = String::from_utf8_lossy(payload).into_owned();
                }
                window::MSG_LAYOUT_CHANGED => {
                    // The compositor is the root of the surface tree and
                    // translates nothing, so it has no layout of its own to
                    // update — it exists here only so that every window gets
                    // the same answer to a question one of them changed.
                    // Delivered to the sender too: the config is the layout,
                    // and re-reading a file one has just written is cheaper
                    // than a rule about who is exempt.
                    for win in &windows {
                        if let Err(e) = win.fd.try_signal(window::MSG_LAYOUT_CHANGED) {
                            mark_dead(&mut dead, win.fd.fd(), win.pid, e.into());
                        }
                    }
                }
                window::MSG_CLIPBOARD_SET_SHM => {
                    let Ok(info) = ipc::decode_payload::<window::ClipboardShmMsg>(payload) else {
                        mark_dead(&mut dead, client_fd, client_pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    let shm = SharedMemory::map(info.token, info.len as usize);
                    clipboard = String::from_utf8_lossy(&shm.as_slice()[..info.len as usize]).into_owned();
                }
                window::MSG_SET_CURSOR => {
                    let Ok(style) = ipc::decode_payload::<u32>(payload) else {
                        mark_dead(&mut dead, client_fd, client_pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    if let Some(win) = windows.iter_mut().find(|w| w.fd.fd() == client_fd) {
                        win.cursor_style = style as u8;
                    }
                }
                window::MSG_SET_RESOLUTION => {
                    let Ok(req) = ipc::decode_payload::<window::ResolutionRequest>(payload) else {
                        mark_dead(&mut dead, client_fd, client_pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    let reply = match gpu::set_resolution(req.width, req.height) {
                        Ok(new_fb_info) => {
                            fb_info = new_fb_info;
                            let new_fb_size = fb_info.stride as usize * fb_info.height as usize * 4;
                            fb_shm = SharedMemory::map(fb_info.token[0], new_fb_size);
                            screen = Framebuffer::new(
                                fb_shm.as_ptr(),
                                fb_info.width as usize,
                                fb_info.height as usize,
                                fb_info.stride as usize,
                                fb_info.pixel_format,
                            );
                            // The counters belong to the mapping, and this is a
                            // new one starting at zero.
                            reported_traffic = screen.traffic();
                            screen_w = screen.width() as i32;
                            screen_h = screen.height() as i32;
                            // What a window costs moved, so what we can afford
                            // moved with it. Windows already open are left
                            // alone if the new figure is below their count —
                            // the cap gates creation, it does not evict.
                            max_windows =
                                self::max_windows(total_mem, screen.width(), screen.height());
                            wallpaper = scale_wallpaper(
                                wallpaper_pixels,
                                wallpaper_w,
                                wallpaper_h,
                                screen.width(),
                                screen.height(),
                                screen.pixel_format_raw() != 0,
                            );

                            let sw = screen_w as usize;
                            let sh = screen_h as usize;
                            let pf = screen.pixel_format_raw();
                            for win in &mut windows {
                                match win.mode {
                                    WindowMode::Maximized => maximize_window(win, sw, sh, pf, &mut dead),
                                    WindowMode::SnappedLeft => snap_left(win, sw, sh, pf, &mut dead),
                                    WindowMode::SnappedRight => snap_right(win, sw, sh, pf, &mut dead),
                                    WindowMode::Normal => {
                                        let win_w = win.width + BORDER_WIDTH * 2;
                                        let win_h = win.height + BORDER_WIDTH * 2 + TITLE_BAR_HEIGHT;
                                        let max_x = sw.saturating_sub(win_w);
                                        let max_y = sh.saturating_sub(win_h + TASKBAR_HEIGHT);
                                        let cx = win.content_x.saturating_sub(BORDER_WIDTH);
                                        let cy = win.content_y.saturating_sub(BORDER_WIDTH + TITLE_BAR_HEIGHT);
                                        win.content_x = cx.min(max_x) + BORDER_WIDTH;
                                        win.content_y = cy.min(max_y) + BORDER_WIDTH + TITLE_BAR_HEIGHT;
                                    }
                                }
                            }

                            cursor_x = cursor_x.min(screen_w - 1);
                            cursor_y = cursor_y.min(screen_h - 1);

                            mark_dirty(&mut dirty_rect, DirtyRect::full(sw, sh));

                            window::ResolutionInfo { width: fb_info.width, height: fb_info.height }
                        }
                        Err(_) => {
                            window::ResolutionInfo { width: fb_info.width, height: fb_info.height }
                        }
                    };
                    if ipc::try_send(client_fd, window::MSG_RESOLUTION_CHANGED, &reply).is_err() {
                        mark_dead(&mut dead, client_fd, client_pid, DropReason::NotReading);
                    }
                }
                window::MSG_GET_RESOLUTION => {
                    let reply = window::ResolutionInfo {
                        width: fb_info.width,
                        height: fb_info.height,
                    };
                    // The one message a client can ask for faster than it can
                    // read the answer: eight bytes in, sixteen out. Blocking
                    // here is a client filling its own pipe and taking the
                    // desktop with it.
                    if ipc::try_send(client_fd, window::MSG_RESOLUTION_CHANGED, &reply).is_err() {
                        mark_dead(&mut dead, client_fd, client_pid, DropReason::NotReading);
                    }
                }
                _ => {}
            }
        }

        for (_, pid, reason) in &dead {
            eprintln!("compositor: dropping pid {pid} — {}", reason.why());
        }
        if !dead.is_empty() {
            let before = windows.len();
            windows.retain(|w| !dead.iter().any(|(fd, _, _)| *fd == w.fd.fd()));
            pending.retain(|p| !dead.iter().any(|(fd, _, _)| *fd == p.conn.fd()));
            if windows.len() != before {
                mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
            }
        }

        // Re-arm one-shot POLL_ADDs for fds that fired
        if kb_ready { poller.poll_add(&kb, IORING_POLL_IN, token_kb); }
        if mouse_ready { poller.poll_add(&mouse, IORING_POLL_IN, token_mouse); }
        if listener_ready { poller.poll_add(&listener, IORING_POLL_IN, token_listener); }
        for win in windows.iter() {
            let token = win.fd.fd().0 as u64;
            if ready_tokens.contains(&token) {
                poller.poll_add(&win.fd, IORING_POLL_IN, token);
            }
        }
        for p in pending.iter() {
            let token = p.conn.fd().0 as u64;
            if ready_tokens.contains(&token) {
                poller.poll_add(&p.conn, IORING_POLL_IN, token);
            }
        }

        // The clause that keeps one client from owning the loop: a peer with
        // something to send on every pass keeps its fd ready forever, and a
        // drain that only ends when nothing is ready would never composite.
        if Instant::now() >= drain_until {
            break;
        }
        } // end inner drain loop

        // Refresh taskbar once per second for clock + stats
        let now = Instant::now();
        if now.duration_since(last_taskbar_update) >= Duration::from_secs(1) {
            last_taskbar_update = now;

            let mut si = [0u8; 48];
            if system::sysinfo(&mut si) >= 48 {
                let total_mem = u64::from_le_bytes(si[0..8].try_into().unwrap());
                let used_mem = u64::from_le_bytes(si[8..16].try_into().unwrap());
                let busy = u64::from_le_bytes(si[32..40].try_into().unwrap());
                let total = u64::from_le_bytes(si[40..48].try_into().unwrap());
                let d_busy = busy.saturating_sub(prev_busy_ticks);
                let d_total = total.saturating_sub(prev_total_ticks);
                if d_total > 0 {
                    cpu_pct = d_busy.saturating_mul(100) / d_total;
                }
                prev_busy_ticks = busy;
                prev_total_ticks = total;
                cached_stats = SystemStats {
                    used_mb: used_mem / (1024 * 1024),
                    total_mb: total_mem / (1024 * 1024),
                    cpu_pct,
                };
            }

            let taskbar_dirty = DirtyRect {
                x: 0, y: screen_h as usize - TASKBAR_HEIGHT,
                w: screen_w as usize, h: TASKBAR_HEIGHT,
            };
            mark_dirty(&mut dirty_rect, taskbar_dirty);
        }

        // Composite once per frame
        if let Some(rect) = dirty_rect.take() {
            let rect = rect.clamp(screen_w as usize, screen_h as usize);
            if rect.w > 0 && rect.h > 0 {
                // Two clock syscalls per composited frame — 120/s at the frame
                // cap — which is what any measure of a frame costs here.
                let composite_start = Instant::now();
                redraw(&screen, &font, &windows, &icons, &wallpaper, launcher_open, &cached_stats, rect);

                // Draw software cursor if no hardware cursor
                if !hw_cursor {
                    let sprite = match current_cursor_style {
                        window::CURSOR_CROSSHAIR => &cursor_crosshair,
                        window::CURSOR_RESIZE => &cursor_resize,
                        _ => &cursor_default,
                    };
                    draw_software_cursor(&screen, sprite, cursor_x, cursor_y);
                    frame_stats.cursor_draws += 1;
                }
                let composited_at = Instant::now();
                frame_stats.record(
                    composited_at.duration_since(composite_start).as_nanos() as u64,
                );

                gpu::present(rect.x as u32, rect.y as u32, rect.w as u32, rect.h as u32)
                    .expect("compositor owns the framebuffer");

                // Send frame callbacks to windows that presented and were composited
                let mut frame_dead: Vec<Dead> = Vec::new();
                for win in windows.iter_mut() {
                    if win.presented && !win.minimized && rect.overlaps(window_screen_rect(win)) {
                        deliver_signal(&mut frame_dead, win, window::MSG_FRAME);
                        win.presented = false;
                    }
                }
                for (_, pid, reason) in &frame_dead {
                    eprintln!("compositor: dropping pid {pid} — {}", reason.why());
                }
                if !frame_dead.is_empty() {
                    windows.retain(|w| !frame_dead.iter().any(|(fd, _, _)| *fd == w.fd.fd()));
                    mark_dirty(&mut dirty_rect, DirtyRect::full(screen_w as usize, screen_h as usize));
                }

                if composited_at >= next_frame_stats {
                    let traffic = screen.traffic();
                    frame_stats.report(traffic.since(reported_traffic), windows.len());
                    frame_stats = FrameStats::default();
                    reported_traffic = traffic;
                    next_frame_stats = composited_at + STATS_INTERVAL;
                }
            }
        }
    }
}
