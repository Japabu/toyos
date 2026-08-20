//! One run of the compositor: the devices it claimed, the clients it holds and
//! the screen it owns.
//!
//! Every method here is an effect. The decisions they act on come from
//! `toyos_desktop` and are host-tested there; what is left is claiming the
//! devices, draining the client connections without ever blocking on one,
//! allocating and granting the shared buffers, and handing finished rectangles
//! to the panel.

use std::process::Command;
use std::time::{Duration, Instant};

use toyos::endow;
use toyos::ipc::RxStep;
use toyos::poller::{Poller, READABLE};
use toyos::port::Acceptor;
use toyos::shm::SharedMemory;
use toyos::{ipc, system, AsHandle, FramebufferDev, Keyboard, Mouse};
use toyos_abi::syscall::DeviceType;
use toyos_abi::RawHandle;
use toyos_desktop::{
    cursor_from_abs, cursor_style, fold_mouse, hit_test, key_action, set_mode, tab_action, Chrome,
    CursorStyle, Damage, Desk, Grab, Held, Hit, KeyAction, Point, Rect, Released, Stack, TabAction,
    Verdict, Window, WindowMode,
};
use window::Screen;

use crate::client::{
    announce, deliver, deliver_signal, deliver_with_handles, mark_dead, note_closed, Client,
    ClientFrame, ClientRx, Dead, DropReason, PendingConn, Win, HANDSHAKE_TIMEOUT,
    MAX_CLIPBOARD_BYTES, MAX_PENDING_CONNS,
};
use crate::render::{self, Assets, BackBuffer, SystemStats, TitleBarIcons};
use crate::stats::FrameStats;
use crate::{
    CURSOR_PX, DOUBLE_CLICK_TIME, DRAIN_BUDGET, FIXED_POLL_HANDLES, FLAG_HARDWARE_CURSOR,
    FRAME_INTERVAL, LAUNCHER_APPS, MAX_WINDOW_SLOTS, STATS_INTERVAL,
};

struct Cursors {
    default: sprite::Sprite,
    crosshair: sprite::Sprite,
    resize: sprite::Sprite,
}

impl Cursors {
    fn get(&self, style: CursorStyle) -> &sprite::Sprite {
        match style {
            CursorStyle::Crosshair => &self.crosshair,
            CursorStyle::Resize => &self.resize,
            CursorStyle::Default => &self.default,
        }
    }
}

/// The wallpaper as it is stored: width, height, then RGB triples.
struct Wallpaper {
    raw: Vec<u8>,
    w: usize,
    h: usize,
    /// Scaled to the current screen and in its pixel format.
    scaled: Vec<u8>,
}

impl Wallpaper {
    fn rescale(&mut self, screen: &Screen) {
        self.scaled = render::scale_wallpaper(
            &self.raw[8..],
            self.w,
            self.h,
            screen.width(),
            screen.height(),
            screen.pixel_format_raw() != 0,
        );
    }
}

pub struct Session {
    acceptor: Acceptor,
    kb: Keyboard,
    mouse: Mouse,
    poller: Poller,

    /// The claim, and the thing every screen call is made *through*: closing it
    /// gives the framebuffer back to the kernel, and there is then no handle to
    /// present with.
    fb_dev: FramebufferDev,
    fb_info: toyos_abi::FramebufferInfo,
    /// Held because [`Session::screen`] points into it.
    _fb_shm: SharedMemory,
    screen: Screen,
    back: BackBuffer,
    hw_cursor: bool,
    /// Held because `cursor_buf` points into it.
    _cursor_shm: SharedMemory,
    cursor_buf: *mut u8,
    cursors: Cursors,
    current_cursor: CursorStyle,

    font: font::Font,
    icons: TitleBarIcons,
    wallpaper: Wallpaper,

    desk: Desk,
    stack: Stack<Client>,
    pending: Vec<PendingConn>,
    damage: Damage,
    cursor: Point,
    prev_buttons: u8,
    grab: Grab,
    last_click_handle: Option<RawHandle>,
    last_click_at: Instant,
    clipboard: String,
    launcher_open: bool,
    total_mem: u64,
    max_windows: usize,

    /// Clients to remove at the end of the pass that condemned them.
    dead: Vec<Dead>,
    /// Tokens the last `wait` reported ready.
    ready: Vec<u64>,

    stats: FrameStats,
    cached_stats: SystemStats,
    prev_busy_ticks: u64,
    prev_total_ticks: u64,
    last_taskbar_update: Instant,
    next_stats_report: Instant,
    reported_traffic: (u64, u64),
    reported_composed: window::Traffic,
}

impl Session {
    pub fn start() -> Self {
        // Every one of these is a statement about the manifest the image was
        // built from, which `src/build.rs` checked before the image was
        // written — so `expect` is right here where `services::listen`'s was
        // not. There is no other process to have taken the name and no name to
        // take.
        let acceptor = endow::acceptor("compositor")
            .expect("the manifest declares this program serves `compositor`");
        let kb: Keyboard = endow::device(DeviceType::Keyboard)
            .expect("the manifest gives this program the keyboard");
        let mouse: Mouse = endow::device(DeviceType::Mouse)
            .expect("the manifest gives this program the mouse");
        let fb_dev: FramebufferDev = endow::device(DeviceType::Framebuffer)
            .expect("the manifest gives this program the framebuffer");

        let fb_info = fb_dev.info().expect("failed to read framebuffer info");
        let fb_size = fb_info.stride as usize * fb_info.height as usize * 4;
        // The scanout and cursor buffers are handles the claim's own read
        // installed, so a refusal here is the kernel contradicting itself and
        // not a client doing anything.
        let fb_shm = SharedMemory::adopt(fb_info.scanout[0], fb_size)
            .expect("the scanout buffer the framebuffer claim just handed over");
        let screen = Screen::new(
            fb_shm.as_ptr(),
            fb_info.width as usize,
            fb_info.height as usize,
            fb_info.stride as usize,
            fb_info.pixel_format,
        );
        let back = BackBuffer::new(screen.width(), screen.height(), screen.pixel_format_raw());

        let hw_cursor = fb_info.flags & FLAG_HARDWARE_CURSOR != 0;
        let cursor_shm = SharedMemory::adopt(fb_info.cursor, 64 * 64 * 4)
            .expect("the cursor buffer the framebuffer claim just handed over");
        let cursor_buf = cursor_shm.as_ptr();
        let cursors = Cursors {
            default: read_sprite("/share/icons/cursor-bold.svg", CURSOR_PX, [255, 255, 255]),
            resize: read_sprite(
                "/share/icons/arrow-down-right-bold.svg",
                CURSOR_PX,
                [255, 255, 255],
            ),
            crosshair: read_sprite("/share/icons/crosshair-simple-bold.svg", CURSOR_PX, [0, 0, 0]),
        };
        render::upload_cursor(&fb_dev, cursor_buf, &cursors.default, hw_cursor);

        let font_data = std::fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font")
            .expect("failed to read font");
        let font = font::Font::from_prebuilt(&font_data);

        let raw = std::fs::read("/share/wallpaper.rgb").expect("failed to read wallpaper");
        let mut wallpaper = Wallpaper {
            w: u32::from_le_bytes(raw[0..4].try_into().unwrap()) as usize,
            h: u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize,
            raw,
            scaled: Vec::new(),
        };
        eprintln!(
            "compositor: wallpaper {}x{}, scaling to {}x{}",
            wallpaper.w,
            wallpaper.h,
            screen.width(),
            screen.height()
        );
        wallpaper.rescale(&screen);

        let icons = TitleBarIcons {
            minimize: read_sprite("/share/icons/minus-bold.svg", 14, [255, 255, 255]),
            maximize: read_sprite("/share/icons/square-bold.svg", 14, [255, 255, 255]),
            close: read_sprite("/share/icons/x-bold.svg", 14, [255, 255, 255]),
        };

        let desk = desk_of(&screen, &font);
        let total_mem = total_memory();
        let max_windows =
            toyos_desktop::max_windows(total_mem, desk.screen, MAX_WINDOW_SLOTS as usize);
        eprintln!(
            "compositor: at most {max_windows} windows ({} MiB each of {} MiB total)",
            toyos_desktop::window_bytes(desk.screen) / (1024 * 1024),
            total_mem / (1024 * 1024),
        );

        // Sized for the slot ceiling rather than for `max_windows`: the batch
        // between two `wait` calls is the three fixed registrations, one per
        // live window
        // and one per pending connection, and `MSG_SET_RESOLUTION` can raise
        // `max_windows` mid-run.
        let poller = Poller::new(FIXED_POLL_HANDLES + MAX_WINDOW_SLOTS + MAX_PENDING_CONNS);
        poller.watch(&kb, READABLE, kb.as_handle().0 as u64);
        poller.watch(&mouse, READABLE, mouse.as_handle().0 as u64);
        poller.watch(&acceptor, READABLE, acceptor.as_handle().0 as u64);

        let cursor = Point { x: desk.screen.w() / 2, y: desk.screen.h() / 2 };
        if hw_cursor {
            fb_dev.move_cursor(cursor.x as u32, cursor.y as u32)
                .expect("compositor holds the framebuffer claim");
        }
        let mut damage = Damage::default();
        damage.add(desk.screen);

        eprintln!("compositor: ready");

        let now = Instant::now();
        Self {
            reported_traffic: screen.traffic(),
            reported_composed: back.surface.traffic(),
            acceptor,
            kb,
            mouse,
            poller,
            fb_dev,
            fb_info,
            _fb_shm: fb_shm,
            screen,
            back,
            hw_cursor,
            _cursor_shm: cursor_shm,
            cursor_buf,
            cursors,
            current_cursor: CursorStyle::Default,
            font,
            icons,
            wallpaper,
            desk,
            stack: Stack::default(),
            pending: Vec::new(),
            damage,
            cursor,
            prev_buttons: 0,
            grab: Grab::None,
            last_click_handle: None,
            last_click_at: now,
            clipboard: String::new(),
            launcher_open: false,
            total_mem,
            max_windows,
            dead: Vec::new(),
            ready: Vec::new(),
            stats: FrameStats::default(),
            cached_stats: SystemStats { used_mb: 0, total_mb: 0, cpu_pct: 0 },
            prev_busy_ticks: 0,
            prev_total_ticks: 0,
            last_taskbar_update: now,
            next_stats_report: now + STATS_INTERVAL,
        }
    }

    /// Drain everything pending, then put one frame on the panel.
    pub fn pass(&mut self) {
        let mut waited = false;
        let drain_until = Instant::now() + DRAIN_BUDGET;
        loop {
            // Zero and not one nanosecond: every turn after the first is
            // consuming what has already arrived, and a timeout of 1 ns is not
            // a spelling of "do not block" — it parks the thread on a deadline
            // that is already past, which is the whole of #156.
            let timeout = if waited { Duration::ZERO } else { FRAME_INTERVAL };
            if !self.drain(timeout) {
                break;
            }
            waited = true;
            // The clause that keeps one client from owning the loop: a peer
            // with something to send on every pass keeps its handle ready
            // forever, and a drain that only ended when nothing was ready would never
            // composite.
            if Instant::now() >= drain_until {
                break;
            }
        }
        self.tick_taskbar();
        self.present();
    }

    /// One turn of the drain, or `false` when nothing was ready.
    fn drain(&mut self, timeout: Duration) -> bool {
        self.ready.clear();
        let mut ready = std::mem::take(&mut self.ready);
        self.poller.wait(1, timeout.as_nanos() as u64, |token| ready.push(token));
        self.ready = ready;

        let kb_ready = self.is_ready(self.kb.as_handle());
        let mouse_ready = self.is_ready(self.mouse.as_handle());
        let accept_ready = self.is_ready(self.acceptor.as_handle());
        let client_ready = self.stack.iter().any(|w| self.is_ready(w.client.conn.as_handle()))
            || self.pending.iter().any(|p| self.is_ready(p.conn.as_handle()));

        // A handshake that never completes is the reason this deadline exists,
        // and the sweep has to happen on a pass that found nothing ready too —
        // otherwise a silent client is only ever timed out by some *other*
        // client's traffic.
        let now = Instant::now();
        for p in self.pending.iter().filter(|p| now.duration_since(p.since) >= HANDSHAKE_TIMEOUT) {
            eprintln!(
                "compositor: dropping client {} — {}",
                p.conn.as_handle().0,
                DropReason::HandshakeTimeout.why()
            );
        }
        self.pending.retain(|p| now.duration_since(p.since) < HANDSHAKE_TIMEOUT);

        if !kb_ready && !mouse_ready && !accept_ready && !client_ready {
            return false;
        }

        self.dead.clear();
        if kb_ready {
            self.keys();
        }
        if mouse_ready {
            self.pointer();
        }
        if accept_ready {
            self.accept();
        }
        let frames = self.take_frames();
        self.dispatch(frames);
        self.reap();
        self.rearm(kb_ready, mouse_ready, accept_ready);
        true
    }

    fn is_ready(&self, handle: RawHandle) -> bool {
        self.ready.contains(&(handle.0 as u64))
    }

    fn pixel_format(&self) -> u32 {
        self.screen.pixel_format_raw()
    }

    fn keys(&mut self) {
        let mut events = [window::KeyEvent::EMPTY; 8];
        let buf = unsafe {
            std::slice::from_raw_parts_mut(
                events.as_mut_ptr() as *mut u8,
                std::mem::size_of_val(&events),
            )
        };
        // Never read blocking here. The kernel wakes only when a report queued
        // an event, so readiness and `has_data` agree — but a blocking read on
        // an empty queue parks the compositor until the next real key, and one
        // spurious wake anywhere would freeze it.
        let n = self.kb.read_nonblock(buf).unwrap_or(0);
        for event in &events[..n / std::mem::size_of::<window::KeyEvent>()] {
            let focused = self.stack.focused();
            let action =
                key_action((*event).into(), focused.map(|i| self.stack[i].mode), self.launcher_open);
            match action {
                KeyAction::Ignore => {}
                KeyAction::Forward => {
                    if let Some(i) = focused {
                        deliver(&mut self.dead, &self.stack[i], window::MSG_KEY_INPUT, event);
                    }
                }
                KeyAction::CloseLauncher => {
                    self.launcher_open = false;
                    self.damage.add(self.launcher_rect());
                }
                KeyAction::CycleFocus => {
                    if self.stack.cycle() {
                        self.damage_all();
                    }
                }
                KeyAction::SpawnTerminal => {
                    Command::new("/bin/terminal").spawn().ok();
                }
                KeyAction::Paste => {
                    if let Some(i) = focused {
                        self.paste(i);
                    }
                }
                // The three that move a window. Each damages the rect it is
                // vacating first, because that is only knowable before it
                // moves.
                KeyAction::SetMode(mode) => {
                    let Some(idx) = focused else { continue };
                    self.damage.add(self.stack[idx].frame(&self.desk.chrome));
                    self.retarget(idx, mode);
                    self.damage_all();
                }
                KeyAction::Minimize => {
                    let Some(idx) = focused else { continue };
                    self.damage.add(self.stack[idx].frame(&self.desk.chrome));
                    self.stack[idx].minimized = true;
                    self.damage_all();
                }
                KeyAction::CloseFocused => {
                    let Some(idx) = focused else { continue };
                    self.damage.add(self.stack[idx].frame(&self.desk.chrome));
                    let win = self.stack.remove(idx);
                    note_closed("GUI+Q", win.client.conn.as_handle(), self.stack.len());
                    let _ = win.client.conn.try_signal(window::MSG_WINDOW_CLOSE);
                    self.damage_all();
                }
            }
        }
    }

    fn pointer(&mut self) {
        let mut buf = [0u8; 512];
        let n = self.mouse.read_nonblock(&mut buf).unwrap_or(0);
        let sample = fold_mouse(self.prev_buttons, &buf[..n]);
        if sample.reports == 0 {
            return;
        }

        let was = self.cursor;
        self.cursor = cursor_from_abs(sample.abs_x, sample.abs_y, self.desk.screen);
        if self.hw_cursor {
            self.fb_dev.move_cursor(self.cursor.x as u32, self.cursor.y as u32)
                .expect("compositor holds the framebuffer claim");
        } else {
            let px = CURSOR_PX as i32;
            self.damage.add(Rect::new(was.x, was.y, px, px));
            self.damage.add(Rect::new(self.cursor.x, self.cursor.y, px, px));
        }
        let delta = Point { x: self.cursor.x - was.x, y: self.cursor.y - was.y };

        let wanted =
            cursor_style(&self.desk, &self.stack, &self.grab, self.cursor, self.launcher_open);
        if wanted != self.current_cursor {
            self.current_cursor = wanted;
            render::upload_cursor(
                &self.fb_dev,
                self.cursor_buf,
                self.cursors.get(wanted),
                self.hw_cursor,
            );
        }

        if sample.pressed {
            self.press(sample.buttons);
        }
        if sample.released {
            self.release(sample.buttons);
        }
        if sample.left_held() {
            self.hold(sample.buttons, delta);
        }
        if sample.scroll != 0 {
            if let Hit::Content(idx) =
                hit_test(&self.desk, &self.stack, self.cursor, self.launcher_open)
            {
                let ev = mouse_event(
                    &self.stack[idx],
                    self.cursor,
                    sample.buttons,
                    window::MOUSE_SCROLL,
                    0,
                    sample.scroll.clamp(-128, 127) as i8,
                );
                deliver(&mut self.dead, &self.stack[idx], window::MSG_MOUSE_INPUT, &ev);
            }
        }
        self.prev_buttons = sample.buttons;
    }

    fn press(&mut self, buttons: u8) {
        let at = self.cursor;
        match hit_test(&self.desk, &self.stack, at, self.launcher_open) {
            Hit::CloseButton(idx) => {
                let win = self.stack.remove(idx);
                note_closed("its close button", win.client.conn.as_handle(), self.stack.len());
                self.damage.add(win.frame(&self.desk.chrome));
                let _ = win.client.conn.try_signal(window::MSG_WINDOW_CLOSE);
                self.damage_all();
            }
            Hit::MinimizeButton(idx) => {
                self.stack[idx].minimized = true;
                self.damage_all();
            }
            Hit::MaximizeButton(idx) => {
                let i = self.stack.raise(idx);
                self.damage.add(self.stack[i].frame(&self.desk.chrome));
                self.retarget(i, toggled(self.stack[i].mode));
                self.damage_all();
            }
            Hit::TitleBar(idx) => {
                let i = self.stack.raise(idx);
                self.damage.add(self.stack[i].frame(&self.desk.chrome));

                let now = Instant::now();
                let handle = self.stack[i].client.conn.as_handle();
                let double = Some(handle) == self.last_click_handle
                    && now.duration_since(self.last_click_at) < DOUBLE_CLICK_TIME;
                if double {
                    self.retarget(i, toggled(self.stack[i].mode));
                    self.last_click_handle = None;
                    self.last_click_at = now - DOUBLE_CLICK_TIME;
                } else {
                    self.last_click_handle = Some(handle);
                    self.last_click_at = now;
                    self.grab = Grab::on_title(self.stack[i].id, self.stack[i].mode, at);
                }
                self.damage_all();
            }
            Hit::ResizeCorner(idx) => {
                let i = self.stack.raise(idx);
                self.grab = Grab::Resizing { window: self.stack[i].id };
                self.damage_all();
            }
            Hit::Content(idx) => {
                if self.launcher_open {
                    self.launcher_open = false;
                    self.damage.add(self.launcher_rect());
                }
                let i = self.stack.raise(idx);
                if i != idx {
                    self.damage_all();
                }
                let ev =
                    mouse_event(&self.stack[i], at, buttons, window::MOUSE_PRESS, 1, 0);
                deliver(&mut self.dead, &self.stack[i], window::MSG_MOUSE_INPUT, &ev);
            }
            Hit::TaskbarItem(idx) => {
                match tab_action(&self.stack, idx) {
                    TabAction::Reveal => {
                        self.stack[idx].minimized = false;
                        self.stack.raise(idx);
                    }
                    TabAction::Minimize => self.stack[idx].minimized = true,
                    TabAction::Raise => {
                        self.stack.raise(idx);
                    }
                }
                self.damage_all();
            }
            Hit::TaskbarNew => {
                self.launcher_open = !self.launcher_open;
                self.damage.add(self.launcher_rect());
                self.damage.add(self.desk.taskbar(self.stack.len()).strip());
            }
            Hit::LauncherItem(idx) => {
                Command::new(LAUNCHER_APPS[idx].1).spawn().ok();
                self.launcher_open = false;
                self.damage.add(self.launcher_rect());
            }
            Hit::Desktop => {
                if self.launcher_open {
                    self.launcher_open = false;
                    self.damage.add(self.launcher_rect());
                }
            }
        }
    }

    fn release(&mut self, buttons: u8) {
        if let Some(i) = self.stack.focused() {
            let ev =
                mouse_event(&self.stack[i], self.cursor, buttons, window::MOUSE_RELEASE, 1, 0);
            deliver(&mut self.dead, &self.stack[i], window::MSG_MOUSE_INPUT, &ev);
        }
        match self.grab.release(&self.desk, &self.stack, self.cursor) {
            Released::Nothing => {}
            Released::Snapped { window: idx, mode } => {
                self.damage.add(self.stack[idx].frame(&self.desk.chrome));
                self.retarget(idx, mode);
                self.damage_all();
            }
            Released::Resized { window: idx } => {
                let pf = self.pixel_format();
                settle(&mut self.stack[idx], pf, &mut self.dead);
                self.damage_all();
            }
        }
    }

    fn hold(&mut self, buttons: u8, delta: Point) {
        match self.grab.hold(&self.desk, &mut self.stack, self.cursor, delta) {
            Held::Idle => {}
            Held::Free => {
                if let Some(i) = self.stack.focused() {
                    let ev =
                        mouse_event(&self.stack[i], self.cursor, buttons, window::MOUSE_MOVE, 0, 0);
                    deliver(&mut self.dead, &self.stack[i], window::MSG_MOUSE_INPUT, &ev);
                }
            }
            Held::Restored { window: idx } => {
                let pf = self.pixel_format();
                settle(&mut self.stack[idx], pf, &mut self.dead);
                self.damage.add(self.desk.screen);
            }
            Held::Moved { from, to, .. } => {
                self.damage.add(from);
                self.damage.add(to);
            }
        }
    }

    fn accept(&mut self) {
        // `accept` installs a descriptor, so it answers `ResourceExhausted` on
        // a full handle table — and clients drive that table, one handle per
        // connection. The connection is lost either way; the desktop is not.
        match self.acceptor.accept() {
            Err(e) => eprintln!("compositor: a connection could not be accepted ({e:?})"),
            Ok(conn) if self.pending.len() >= MAX_PENDING_CONNS as usize => {
                eprintln!(
                    "compositor: refusing client {} — {MAX_PENDING_CONNS} connections are already \
                     waiting to say what they want",
                    conn.as_handle().0
                );
            }
            Ok(conn) => {
                self.poller.watch(&conn, READABLE, conn.as_handle().0 as u64);
                self.pending.push(PendingConn { conn, rx: ClientRx::new(), since: Instant::now() });
            }
        }
    }

    /// Every whole frame that arrived, off the connections and in memory.
    ///
    /// Collected before anything acts on one: the read side is finished by the
    /// time a message is dispatched, so no branch of [`dispatch`](Self::dispatch)
    /// can park on a peer.
    fn take_frames(&mut self) -> Vec<ClientFrame> {
        let mut out: Vec<ClientFrame> = Vec::new();

        for i in 0..self.pending.len() {
            if !self.is_ready(self.pending[i].conn.as_handle()) {
                continue;
            }
            let step = {
                let p = &mut self.pending[i];
                p.rx.pump(&p.conn)
            };
            let handle = self.pending[i].conn.as_handle();
            match step {
                RxStep::Idle => {}
                RxStep::Eof => mark_dead(&mut self.dead, handle, DropReason::Gone),
                RxStep::Malformed => {
                    mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol)
                }
                RxStep::Frame { msg_type, payload_len } => {
                    let mut frame = ClientFrame::new(handle, msg_type);
                    frame.set_payload(self.pending[i].rx.payload(payload_len));
                    // A connection is identified by its first frame and by
                    // nothing else. `MSG_CREATE_WINDOW` promotes it to a
                    // window; anything else is a one-shot request, answered and
                    // closed — which is what an `endow::service` caller like
                    // `window::clipboard_set` expects.
                    //
                    // One promotion per pass keeps `i` meaningful across the
                    // `remove`; the rest are re-armed below and served next
                    // pass.
                    frame.conn = Some(self.pending.remove(i).conn);
                    out.push(frame);
                    break;
                }
            }
        }

        for i in 0..self.stack.len() {
            if !self.is_ready(self.stack[i].client.conn.as_handle()) {
                continue;
            }
            let win = &mut self.stack[i];
            let step = win.client.rx.pump(&win.client.conn);
            let handle = win.client.conn.as_handle();
            match step {
                RxStep::Idle => {}
                RxStep::Eof => mark_dead(&mut self.dead, handle, DropReason::Gone),
                RxStep::Malformed => {
                    mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol)
                }
                RxStep::Frame { msg_type, payload_len } => {
                    let mut frame = ClientFrame::new(handle, msg_type);
                    frame.set_payload(win.client.rx.payload(payload_len));
                    out.push(frame);
                }
            }
        }
        out
    }

    fn dispatch(&mut self, frames: Vec<ClientFrame>) {
        for frame in frames {
            let handle = frame.handle;
            match frame.msg_type {
                window::MSG_CREATE_WINDOW => self.create_window(frame),
                window::MSG_PRESENT => {
                    let Ok(rect) = ipc::decode_payload::<window::Rect>(frame.payload()) else {
                        mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol);
                        continue;
                    };
                    if let Some(i) = self.stack.find(|w| w.client.conn.as_handle() == handle) {
                        self.stack[i].presented = true;
                        let claim = Rect::from_wire(rect.x, rect.y, rect.w, rect.h);
                        self.damage.add(self.stack[i].present_damage(claim));
                    }
                }
                window::MSG_DESTROY_WINDOW => {
                    if let Some(i) = self.stack.find(|w| w.client.conn.as_handle() == handle) {
                        let gone = self.stack.remove(i);
                        note_closed("the client itself", gone.client.conn.as_handle(), self.stack.len());
                        self.damage.add(gone.frame(&self.desk.chrome));
                        self.damage_all();
                    }
                }
                window::MSG_CLIPBOARD_SET => {
                    self.clipboard = String::from_utf8_lossy(frame.payload()).into_owned();
                }
                window::MSG_LAYOUT_CHANGED => {
                    // The compositor is the root of the surface tree and
                    // translates nothing, so it has no layout of its own to
                    // update — it exists here only so that every window gets
                    // the same answer to a question one of them changed.
                    // Delivered to the sender too: the config is the layout,
                    // and re-reading a file one has just written is cheaper
                    // than a rule about who is exempt.
                    for win in self.stack.iter() {
                        deliver_signal(&mut self.dead, win, window::MSG_LAYOUT_CHANGED);
                    }
                }
                window::MSG_CLIPBOARD_SET_SHM => {
                    let Ok(info) =
                        ipc::decode_payload::<window::ClipboardShmMsg>(frame.payload())
                    else {
                        mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol);
                        continue;
                    };
                    // The length is the client's claim about how much of the
                    // region it sent is text, and past the region that is a
                    // read of somebody else's memory rather than a clipboard.
                    // The region itself is no longer a claim: it is a handle
                    // the client moved, so there is nothing left to disbelieve
                    // about which memory this is.
                    if info.len as usize > MAX_CLIPBOARD_BYTES {
                        eprintln!(
                            "compositor: refusing {} bytes of clipboard from client {}, max \
                             {MAX_CLIPBOARD_BYTES}",
                            info.len,
                            handle.0
                        );
                        continue;
                    }
                    let Some([buffer]) = ipc::recv_handles_exact::<1>(handle) else {
                        mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol);
                        continue;
                    };
                    let Ok(shm) = SharedMemory::adopt(buffer, info.len as usize) else {
                        mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol);
                        continue;
                    };
                    self.clipboard = String::from_utf8_lossy(shm.as_slice()).into_owned();
                }
                window::MSG_SET_CURSOR => {
                    let Ok(style) = ipc::decode_payload::<u32>(frame.payload()) else {
                        mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol);
                        continue;
                    };
                    if let Some(i) = self.stack.find(|w| w.client.conn.as_handle() == handle) {
                        self.stack[i].cursor_style = cursor_from_wire(style);
                    }
                }
                window::MSG_SET_RESOLUTION => {
                    let Ok(req) =
                        ipc::decode_payload::<window::ResolutionRequest>(frame.payload())
                    else {
                        mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol);
                        continue;
                    };
                    self.set_resolution(req.width, req.height);
                    self.answer_resolution(handle);
                }
                // The one message a client can ask for faster than it can read
                // the answer: eight bytes in, sixteen out. Blocking here is a
                // client filling its own pipe and taking the desktop with it.
                window::MSG_GET_RESOLUTION => self.answer_resolution(handle),
                _ => {}
            }
        }
    }

    fn create_window(&mut self, frame: ClientFrame) {
        let handle = frame.handle;
        // `frame.conn` is dropped by every early return here, which closes the
        // handle: there is no window to remove yet.
        let Ok(req) = ipc::decode_payload::<window::CreateWindowRequest>(frame.payload()) else {
            return;
        };
        // A window *is* a connection its first frame promoted, so a second
        // `MSG_CREATE_WINDOW` comes with nothing to promote. `conn` is `None`
        // for every frame off an established window, and reading that as a bug
        // rather than as a protocol error made one message from any client
        // fatal.
        let Some(conn) = frame.conn else {
            mark_dead(&mut self.dead, handle, DropReason::OutOfProtocol);
            return;
        };

        // Every refusal below is an answer to untrusted input, so none of them
        // is a panic and none is a silent shrink of what was asked for.
        let refusal = match toyos_desktop::create_verdict(
            (req.width, req.height),
            self.desk.screen,
            self.stack.len(),
            self.max_windows,
        ) {
            Verdict::Allow => None,
            Verdict::AtCapacity => Some(window::REFUSED_AT_CAPACITY),
            Verdict::TooLarge => Some(window::REFUSED_TOO_LARGE),
        };
        if let Some(reason) = refusal {
            eprintln!(
                "compositor: refusing {}x{} window from client {} ({} live, max {}), reason \
                 {reason}",
                req.width,
                req.height,
                handle.0,
                self.stack.len(),
                self.max_windows
            );
            let _ =
                ipc::try_send(handle, window::MSG_WINDOW_REFUSED, &window::WindowRefused { reason });
            return;
        }

        let content = self.desk.chrome.initial_content(
            Some((req.width as i32, req.height as i32)),
            self.stack.len(),
            self.desk.screen,
        );
        let shm = match SharedMemory::create((content.area() * 4) as usize) {
            Ok(shm) => shm,
            Err(e) => {
                eprintln!(
                    "compositor: refusing {}x{} window from client {} — there is no memory \
                     for it ({e:?})",
                    content.w(),
                    content.h(),
                    handle.0
                );
                let _ = ipc::try_send(
                    handle,
                    window::MSG_WINDOW_REFUSED,
                    &window::WindowRefused { reason: window::REFUSED_NO_MEMORY },
                );
                return;
            }
        };
        // A second handle to the same region, for the client. The compositor
        // keeps the first and draws chrome through it, so the two lifetimes are
        // genuinely separate: a client that exits takes its own handle and
        // leaves the compositor's buffer intact until the window goes.
        let client_shm = match shm.share() {
            Ok(h) => h,
            Err(e) => {
                eprintln!(
                    "compositor: refusing a window to client {} — its buffer cannot be shared \
                     ({e:?})",
                    handle.0
                );
                let _ = ipc::try_send(
                    handle,
                    window::MSG_WINDOW_REFUSED,
                    &window::WindowRefused { reason: window::REFUSED_NO_MEMORY },
                );
                return;
            }
        };
        let title = if req.title_len > 0 {
            let len = (req.title_len as usize).min(30);
            String::from_utf8_lossy(&req.title[..len]).into_owned()
        } else {
            String::new()
        };
        let at = self.stack.insert(Window::new(
            Client { conn, shm, rx: ClientRx::new() },
            content,
            title,
            req.flags & window::WINDOW_FLAG_TOPMOST != 0,
            CursorStyle::Default,
        ));

        self.poller.watch(&self.stack[at].client.conn, READABLE, handle.0 as u64);
        let pixel_format = self.pixel_format();
        deliver_with_handles(
            &mut self.dead,
            &self.stack[at],
            &[client_shm],
            window::MSG_WINDOW_CREATED,
            &window::WindowInfo {
                width: content.w() as u32,
                height: content.h() as u32,
                stride: content.w() as u32,
                pixel_format,
            },
        );
        self.damage_all();
    }

    fn answer_resolution(&mut self, handle: RawHandle) {
        let reply =
            window::ResolutionInfo { width: self.fb_info.width, height: self.fb_info.height };
        if ipc::try_send(handle, window::MSG_RESOLUTION_CHANGED, &reply).is_err() {
            mark_dead(&mut self.dead, handle, DropReason::NotReading);
        }
    }

    fn set_resolution(&mut self, width: u32, height: u32) {
        let Ok(info) = self.fb_dev.set_resolution(width, height) else {
            return;
        };
        self.fb_info = info;
        let size = info.stride as usize * info.height as usize * 4;
        self._fb_shm = SharedMemory::adopt(info.scanout[0], size)
            .expect("the scanout buffer the mode set just handed over");
        self.screen = Screen::new(
            self._fb_shm.as_ptr(),
            info.width as usize,
            info.height as usize,
            info.stride as usize,
            info.pixel_format,
        );
        self.back =
            BackBuffer::new(self.screen.width(), self.screen.height(), self.pixel_format());
        // The counters belong to the mapping, and this is a new one starting at
        // zero.
        self.reported_traffic = self.screen.traffic();
        self.reported_composed = self.back.surface.traffic();
        self.desk = desk_of(&self.screen, &self.font);
        // What a window costs moved, so what we can afford moved with it.
        // Windows already open are left alone if the new figure is below their
        // count — the cap gates creation, it does not evict.
        self.max_windows =
            toyos_desktop::max_windows(self.total_mem, self.desk.screen, MAX_WINDOW_SLOTS as usize);
        self.wallpaper.rescale(&self.screen);

        for i in 0..self.stack.len() {
            match self.stack[i].mode {
                WindowMode::Normal => {
                    self.stack[i].content =
                        self.desk.chrome.reflow(self.stack[i].content, self.desk.screen)
                }
                mode => self.retarget(i, mode),
            }
        }
        self.cursor.x = self.cursor.x.min(self.desk.screen.x1 - 1);
        self.cursor.y = self.cursor.y.min(self.desk.screen.y1 - 1);
        self.damage.add(self.desk.screen);
    }

    fn reap(&mut self) {
        announce(&self.dead);
        if self.dead.is_empty() {
            return;
        }
        let before = self.stack.len();
        // The rect a dropped window vacates is only knowable while it is still
        // in the list.
        let vacated: Vec<Rect> = self
            .stack
            .iter()
            .filter(|w| self.dead.iter().any(|(handle, _)| *handle == w.client.conn.as_handle()))
            .map(|w| w.frame(&self.desk.chrome))
            .collect();
        let dead = std::mem::take(&mut self.dead);
        self.stack.retain(|w| !dead.iter().any(|(handle, _)| *handle == w.client.conn.as_handle()));
        self.pending.retain(|p| !dead.iter().any(|(handle, _)| *handle == p.conn.as_handle()));
        self.dead = dead;
        if self.stack.len() != before {
            for rect in vacated {
                self.damage.add(rect);
            }
            self.damage_all();
        }
    }

    /// Re-arm the one-shot poll registrations for every handle that fired.
    fn rearm(&mut self, kb: bool, mouse: bool, acceptor: bool) {
        if kb {
            self.poller.watch(&self.kb, READABLE, self.kb.as_handle().0 as u64);
        }
        if mouse {
            self.poller.watch(&self.mouse, READABLE, self.mouse.as_handle().0 as u64);
        }
        if acceptor {
            let h = self.acceptor.as_handle();
            self.poller.watch(&self.acceptor, READABLE, h.0 as u64);
        }
        for win in self.stack.iter() {
            let handle = win.client.conn.as_handle();
            if self.is_ready(handle) {
                self.poller.watch(&win.client.conn, READABLE, handle.0 as u64);
            }
        }
        for p in self.pending.iter() {
            let handle = p.conn.as_handle();
            if self.is_ready(handle) {
                self.poller.watch(&p.conn, READABLE, handle.0 as u64);
            }
        }
    }

    fn tick_taskbar(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_taskbar_update) < Duration::from_secs(1) {
            return;
        }
        self.last_taskbar_update = now;

        let mut si = [0u8; 48];
        if system::sysinfo(&mut si) >= 48 {
            let total_mem = u64::from_le_bytes(si[0..8].try_into().unwrap());
            let used_mem = u64::from_le_bytes(si[8..16].try_into().unwrap());
            let busy = u64::from_le_bytes(si[32..40].try_into().unwrap());
            let total = u64::from_le_bytes(si[40..48].try_into().unwrap());
            let d_busy = busy.saturating_sub(self.prev_busy_ticks);
            let d_total = total.saturating_sub(self.prev_total_ticks);
            if d_total > 0 {
                self.cached_stats.cpu_pct = d_busy.saturating_mul(100) / d_total;
            }
            self.prev_busy_ticks = busy;
            self.prev_total_ticks = total;
            self.cached_stats.used_mb = used_mem / (1024 * 1024);
            self.cached_stats.total_mb = total_mem / (1024 * 1024);
        }

        // Only the readout, which is the only thing about the bar a second
        // changes. A whole-bar repaint here is what the owner saw as the
        // taskbar flickering once a second.
        self.damage.add(self.desk.taskbar(self.stack.len()).status());
    }

    fn present(&mut self) {
        if self.damage.is_empty() {
            return;
        }
        let regions = self.damage.take(self.desk.screen);
        if regions.is_empty() {
            return;
        }
        // Two clock syscalls per composited frame — 120/s at the frame cap —
        // which is what any measure of a frame costs here.
        let started = Instant::now();
        let assets = Assets {
            font: &self.font,
            icons: &self.icons,
            wallpaper: &self.wallpaper.scaled,
            apps: LAUNCHER_APPS,
        };
        for region in &regions {
            render::paint(
                &self.back.surface,
                &self.desk,
                &self.stack,
                &assets,
                self.launcher_open,
                &self.cached_stats,
                *region,
            );
        }

        // Into the back buffer, so a region containing the cursor carries it
        // over with everything else rather than the panel being touched twice.
        if !self.hw_cursor {
            let sprite = self.cursors.get(self.current_cursor);
            let rect = Rect::new(
                self.cursor.x,
                self.cursor.y,
                sprite.width() as i32,
                sprite.height() as i32,
            );
            if regions.iter().any(|r| r.overlaps(rect)) {
                render::draw_software_cursor(&self.back.surface, sprite, self.cursor);
                self.stats.cursor_draws += 1;
            }
        }

        let mut damage_px = 0;
        for region in &regions {
            self.screen.blit(
                region.x0 as usize,
                region.y0 as usize,
                region.w() as usize,
                region.h() as usize,
                self.back.surface.width(),
                self.back.region(*region),
            );
            damage_px += region.area();
        }
        let composited_at = Instant::now();
        self.stats.record(
            composited_at.duration_since(started).as_nanos() as u64,
            regions.len(),
            damage_px,
        );

        for region in &regions {
            self.fb_dev
                .present(
                    region.x0 as u32,
                    region.y0 as u32,
                    region.w() as u32,
                    region.h() as u32,
                )
                .expect("compositor holds the framebuffer claim");
        }

        self.frame_callbacks(&regions);

        if composited_at >= self.next_stats_report {
            let traffic = self.screen.traffic();
            let composed = self.back.surface.traffic();
            self.stats.report(
                (traffic.0 - self.reported_traffic.0, traffic.1 - self.reported_traffic.1),
                composed.since(self.reported_composed),
                self.stack.len(),
            );
            self.stats = FrameStats::default();
            self.reported_traffic = traffic;
            self.reported_composed = composed;
            self.next_stats_report = composited_at + STATS_INTERVAL;
        }
    }

    /// Tell every window that presented and was composited that its frame is
    /// on the panel.
    fn frame_callbacks(&mut self, regions: &[Rect]) {
        let mut dead: Vec<Dead> = Vec::new();
        for i in 0..self.stack.len() {
            let rect = self.stack[i].frame(&self.desk.chrome);
            if self.stack[i].presented
                && !self.stack[i].minimized
                && regions.iter().any(|r| r.overlaps(rect))
            {
                deliver_signal(&mut dead, &self.stack[i], window::MSG_FRAME);
                self.stack[i].presented = false;
            }
        }
        announce(&dead);
        if dead.is_empty() {
            return;
        }
        for win in
            self.stack.iter().filter(|w| dead.iter().any(|(h, _)| *h == w.client.conn.as_handle()))
        {
            self.damage.add(win.frame(&self.desk.chrome));
        }
        self.stack.retain(|w| !dead.iter().any(|(handle, _)| *handle == w.client.conn.as_handle()));
        self.damage_all();
    }

    /// Damage every window and the taskbar, for a change that reorders or
    /// re-focuses them.
    ///
    /// Bounded by what is on screen rather than by the screen: two small
    /// windows cost those two and the bar, where the full-screen repaint this
    /// replaces cost the wallpaper under everything as well. Minimized windows
    /// are damaged too — one of them may be the window that just stopped being
    /// minimized, and a caller that had to know which is a caller that will one
    /// day be wrong.
    fn damage_all(&mut self) {
        for i in 0..self.stack.len() {
            let rect = self.stack[i].frame(&self.desk.chrome);
            self.damage.add(rect);
        }
        self.damage.add(self.desk.taskbar(self.stack.len()).strip());
    }

    fn launcher_rect(&self) -> Rect {
        self.desk.taskbar(self.stack.len()).launcher()
    }

    fn retarget(&mut self, idx: usize, mode: WindowMode) {
        let pf = self.pixel_format();
        set_mode(&self.desk, &mut self.stack, idx, mode);
        settle(&mut self.stack[idx], pf, &mut self.dead);
    }

    /// GUI+V: hand the window at `idx` the clipboard.
    ///
    /// Over 4096 bytes it goes through shared memory — a region made here and
    /// moved to the window with the message that says how much of it is text.
    fn paste(&mut self, idx: usize) {
        if self.clipboard.is_empty() {
            return;
        }
        let win = &self.stack[idx];
        if self.clipboard.len() <= 4096 {
            if let Err(e) = win
                .client
                .conn
                .try_send_bytes(window::MSG_CLIPBOARD_PASTE, self.clipboard.as_bytes())
            {
                mark_dead(&mut self.dead, win.client.conn.as_handle(), e.into());
            }
            return;
        }
        // Nothing is held here. The window's handle keeps the region alive once
        // the send has moved it, so the compositor's own mapping goes at the end
        // of this function — where the old code kept the last paste mapped until
        // the next one replaced it.
        let mut shm = match SharedMemory::create(self.clipboard.len()) {
            Ok(shm) => shm,
            Err(e) => {
                eprintln!(
                    "compositor: client {} gets no paste — no memory for {} bytes ({e:?})",
                    win.client.conn.as_handle().0,
                    self.clipboard.len()
                );
                return;
            }
        };
        let Ok(handle) = shm.share() else {
            eprintln!(
                "compositor: client {} gets no paste — its region cannot be shared",
                win.client.conn.as_handle().0
            );
            return;
        };
        shm.as_mut_slice()[..self.clipboard.len()].copy_from_slice(self.clipboard.as_bytes());
        deliver_with_handles(
            &mut self.dead,
            win,
            &[handle],
            window::MSG_CLIPBOARD_PASTE_SHM,
            &window::ClipboardShmMsg { len: self.clipboard.len() as u32 },
        );
    }
}

fn desk_of(screen: &Screen, font: &font::Font) -> Desk {
    Desk {
        chrome: Chrome::DEFAULT,
        screen: Rect::new(0, 0, screen.width() as i32, screen.height() as i32),
        font_w: font.width() as i32,
        apps: LAUNCHER_APPS.len(),
    }
}

fn read_sprite(path: &str, size: u32, color: [u8; 3]) -> sprite::Sprite {
    let svg = std::fs::read(path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
    sprite::Sprite::from_svg_colored(&svg, size, color)
}

/// Total physical memory, as the kernel reports it.
fn total_memory() -> u64 {
    let mut buf = [0u8; system::SYSINFO_HEADER_SIZE];
    let n = system::sysinfo(&mut buf);
    assert!(n >= system::SYSINFO_HEADER_SIZE, "sysinfo returned {n} bytes");
    u64::from_le_bytes(buf[0..8].try_into().unwrap())
}

/// The mode a window toggles into when it is maximized by button or chord.
fn toggled(mode: WindowMode) -> WindowMode {
    if mode == WindowMode::Normal {
        WindowMode::Maximized
    } else {
        WindowMode::Normal
    }
}

/// A cursor style off the wire.
///
/// A client can send any `u32`; one nobody implements is the default cursor,
/// not an index into anything.
fn cursor_from_wire(raw: u32) -> CursorStyle {
    match raw as u8 {
        window::CURSOR_CROSSHAIR => CursorStyle::Crosshair,
        window::CURSOR_RESIZE => CursorStyle::Resize,
        _ => CursorStyle::Default,
    }
}

fn mouse_event(
    win: &Win,
    at: Point,
    buttons: u8,
    event_type: u8,
    changed: u8,
    scroll: i8,
) -> window::MouseEvent {
    window::MouseEvent {
        x: (at.x - win.content.x0).max(0) as u16,
        y: (at.y - win.content.y0).max(0) as u16,
        buttons,
        event_type,
        changed,
        scroll,
    }
}

/// Give `win` a buffer the size of its content rect, and say whether it got one.
///
/// **Neither refusal is fatal.** The memory is the compositor's own rather than
/// the client's doing, so a refusal keeps the window at a size it can afford —
/// and a client that has exited is learned about by the send below refusing,
/// never by a grant naming a pid the process table no longer knows. That grant
/// is how the desktop used to die: `SharedMemory::grant` was infallible over an
/// `InvalidArgument` that a maximize could reach, and it took every other
/// window with it.
fn rebuffer(win: &mut Win, pixel_format: u32, dead: &mut Vec<Dead>) -> bool {
    let (w, h) = (win.content.w(), win.content.h());
    let new_shm = match SharedMemory::create(w as usize * h as usize * 4) {
        Ok(shm) => shm,
        Err(e) => {
            eprintln!(
                "compositor: client {} keeps its {}x{} buffer — no memory for {w}x{h} ({e:?})",
                win.client.conn.as_handle().0,
                win.buf_w,
                win.buf_h
            );
            return false;
        }
    };
    let Ok(client_shm) = new_shm.share() else {
        eprintln!(
            "compositor: client {} keeps its {}x{} buffer — the new one cannot be shared",
            win.client.conn.as_handle().0,
            win.buf_w,
            win.buf_h
        );
        return false;
    };
    win.client.shm = new_shm;
    win.buf_w = w;
    win.buf_h = h;
    // The one message a window cannot afford to miss — the old mapping is
    // already gone — so a client that will not take it is dropped rather than
    // left drawing into memory it no longer owns.
    deliver_with_handles(
        dead,
        win,
        &[client_shm],
        window::MSG_WINDOW_RESIZED,
        &window::ResizeInfo {
            width: w as u32,
            height: h as u32,
            stride: w as u32,
            pixel_format,
        },
    );
    true
}

/// Make the window and its buffer agree, whichever way round that has to go.
///
/// A window is allowed to run ahead of its memory only while a resize is being
/// dragged. Everywhere else the two must match, and if the machine will not
/// give the memory then it is the window that gives way.
fn settle(win: &mut Win, pixel_format: u32, dead: &mut Vec<Dead>) {
    if !rebuffer(win, pixel_format, dead) {
        win.content = win.backed();
    }
}
