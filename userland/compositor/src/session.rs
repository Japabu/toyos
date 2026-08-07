//! One run of the compositor: the devices it claimed, the clients it holds and
//! the screen it owns.
//!
//! Every method here is an effect. The decisions they act on come from
//! `toyos_desktop` and are host-tested there; what is left is claiming the
//! devices, draining the fds without ever blocking on a client, allocating and
//! granting the shared buffers, and handing finished rectangles to the panel.

use std::process::Command;
use std::time::{Duration, Instant};

use toyos::ipc::RxStep;
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::shm::SharedMemory;
use toyos::{gpu, ipc, services, system, FramebufferDev, Keyboard, Listener, Mouse};
use toyos_abi::Fd;
use toyos_desktop::{
    cursor_from_abs, cursor_style, fold_mouse, hit_test, key_action, set_mode, tab_action, Chrome,
    CursorStyle, Damage, Desk, Grab, Held, Hit, KeyAction, Point, Rect, Released, Stack, TabAction,
    Verdict, Window, WindowMode,
};
use window::Screen;

use crate::client::{
    announce, deliver, deliver_signal, mark_dead, note_closed, Client, ClientFrame, ClientRx, Dead,
    DropReason, PendingConn, Win, HANDSHAKE_TIMEOUT, MAX_CLIPBOARD_BYTES, MAX_PENDING_CONNS,
};
use crate::render::{self, Assets, BackBuffer, SystemStats, TitleBarIcons};
use crate::stats::FrameStats;
use crate::{
    CURSOR_PX, DOUBLE_CLICK_TIME, DRAIN_BUDGET, FIXED_POLL_FDS, FLAG_HARDWARE_CURSOR,
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
    listener: Listener,
    kb: Keyboard,
    mouse: Mouse,
    poller: Poller,

    /// Held because closing it gives the framebuffer back to the kernel, and
    /// every `gpu::` call afterwards is made by a process that no longer owns
    /// the panel it is drawing on.
    _fb_dev: FramebufferDev,
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
    last_click_fd: Option<Fd>,
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
        let listener = services::listen("compositor").expect("compositor already running");
        let kb = Keyboard::open().expect("failed to claim keyboard");
        let mouse = Mouse::open().expect("failed to claim mouse");
        let fb_dev = FramebufferDev::open().expect("failed to claim framebuffer");

        let fb_info = fb_dev.info().expect("failed to read framebuffer info");
        let fb_size = fb_info.stride as usize * fb_info.height as usize * 4;
        // The framebuffer's tokens come from the device this process has just
        // claimed, so a refusal here is the kernel contradicting itself and not
        // a client doing anything.
        let fb_shm = SharedMemory::map(fb_info.token[0], fb_size)
            .expect("the scanout token the framebuffer device just reported");
        let screen = Screen::new(
            fb_shm.as_ptr(),
            fb_info.width as usize,
            fb_info.height as usize,
            fb_info.stride as usize,
            fb_info.pixel_format,
        );
        let back = BackBuffer::new(screen.width(), screen.height(), screen.pixel_format_raw());

        let hw_cursor = fb_info.flags & FLAG_HARDWARE_CURSOR != 0;
        let cursor_shm = SharedMemory::map(fb_info.cursor_token, 64 * 64 * 4)
            .expect("the cursor token the framebuffer device just reported");
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
        render::upload_cursor(cursor_buf, &cursors.default, hw_cursor);

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
        // between two `wait` calls is the three fixed fds, one per live window
        // and one per pending connection, and `MSG_SET_RESOLUTION` can raise
        // `max_windows` mid-run.
        let poller = Poller::new(FIXED_POLL_FDS + MAX_WINDOW_SLOTS + MAX_PENDING_CONNS);
        poller.poll_add(&kb, IORING_POLL_IN, kb.fd().0 as u64);
        poller.poll_add(&mouse, IORING_POLL_IN, mouse.fd().0 as u64);
        poller.poll_add(&listener, IORING_POLL_IN, listener.fd().0 as u64);

        let cursor = Point { x: desk.screen.w() / 2, y: desk.screen.h() / 2 };
        if hw_cursor {
            gpu::move_cursor(cursor.x as u32, cursor.y as u32)
                .expect("compositor owns the framebuffer");
        }
        let mut damage = Damage::default();
        damage.add(desk.screen);

        eprintln!("compositor: ready");
        Command::new("/bin/filepicker").spawn().ok();

        let now = Instant::now();
        Self {
            reported_traffic: screen.traffic(),
            reported_composed: back.surface.traffic(),
            listener,
            kb,
            mouse,
            poller,
            _fb_dev: fb_dev,
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
            last_click_fd: None,
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
            let timeout = if waited { Duration::from_nanos(1) } else { FRAME_INTERVAL };
            if !self.drain(timeout) {
                break;
            }
            waited = true;
            // The clause that keeps one client from owning the loop: a peer
            // with something to send on every pass keeps its fd ready forever,
            // and a drain that only ended when nothing was ready would never
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

        let kb_ready = self.is_ready(self.kb.fd());
        let mouse_ready = self.is_ready(self.mouse.fd());
        let listener_ready = self.is_ready(self.listener.fd());
        let client_ready = self.stack.iter().any(|w| self.is_ready(w.client.conn.fd()))
            || self.pending.iter().any(|p| self.is_ready(p.conn.fd()));

        // A handshake that never completes is the reason this deadline exists,
        // and the sweep has to happen on a pass that found nothing ready too —
        // otherwise a silent client is only ever timed out by some *other*
        // client's traffic.
        let now = Instant::now();
        for p in self.pending.iter().filter(|p| now.duration_since(p.since) >= HANDSHAKE_TIMEOUT) {
            eprintln!(
                "compositor: dropping pid {} — {}",
                p.pid,
                DropReason::HandshakeTimeout.why()
            );
        }
        self.pending.retain(|p| now.duration_since(p.since) < HANDSHAKE_TIMEOUT);

        if !kb_ready && !mouse_ready && !listener_ready && !client_ready {
            return false;
        }

        self.dead.clear();
        if kb_ready {
            self.keys();
        }
        if mouse_ready {
            self.pointer();
        }
        if listener_ready {
            self.accept();
        }
        let frames = self.take_frames();
        self.dispatch(frames);
        self.reap();
        self.rearm(kb_ready, mouse_ready, listener_ready);
        true
    }

    fn is_ready(&self, fd: Fd) -> bool {
        self.ready.contains(&(fd.0 as u64))
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
                    note_closed("GUI+Q", win.client.pid, self.stack.len());
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
            gpu::move_cursor(self.cursor.x as u32, self.cursor.y as u32)
                .expect("compositor owns the framebuffer");
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
            render::upload_cursor(self.cursor_buf, self.cursors.get(wanted), self.hw_cursor);
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
                note_closed("its close button", win.client.pid, self.stack.len());
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
                let fd = self.stack[i].client.conn.fd();
                let double = Some(fd) == self.last_click_fd
                    && now.duration_since(self.last_click_at) < DOUBLE_CLICK_TIME;
                if double {
                    self.retarget(i, toggled(self.stack[i].mode));
                    self.last_click_fd = None;
                    self.last_click_at = now - DOUBLE_CLICK_TIME;
                } else {
                    self.last_click_fd = Some(fd);
                    self.last_click_at = now;
                    self.grab = Grab::on_title(i, self.stack[i].mode, at);
                }
                self.damage_all();
            }
            Hit::ResizeCorner(idx) => {
                let i = self.stack.raise(idx);
                self.grab = Grab::Resizing { window: i };
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
        match self.grab.release(&self.desk, self.cursor) {
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
        // a full fd table — and clients drive that table, one fd per
        // connection. The connection is lost either way; the desktop is not.
        match services::accept(&self.listener) {
            Err(e) => eprintln!("compositor: a connection could not be accepted ({e:?})"),
            Ok(result) if self.pending.len() >= MAX_PENDING_CONNS as usize => {
                eprintln!(
                    "compositor: refusing pid {} — {MAX_PENDING_CONNS} connections are already \
                     waiting to say what they want",
                    result.client_pid
                );
            }
            Ok(result) => {
                self.poller.poll_add(&result.conn, IORING_POLL_IN, result.conn.fd().0 as u64);
                self.pending.push(PendingConn {
                    conn: result.conn,
                    pid: result.client_pid,
                    rx: ClientRx::new(),
                    since: Instant::now(),
                });
            }
        }
    }

    /// Every whole frame that arrived, off the fds and in memory.
    ///
    /// Collected before anything acts on one: the read side is finished by the
    /// time a message is dispatched, so no branch of [`dispatch`](Self::dispatch)
    /// can park on a peer.
    fn take_frames(&mut self) -> Vec<ClientFrame> {
        let mut out: Vec<ClientFrame> = Vec::new();

        for i in 0..self.pending.len() {
            if !self.is_ready(self.pending[i].conn.fd()) {
                continue;
            }
            let step = {
                let p = &mut self.pending[i];
                p.rx.pump(&p.conn)
            };
            let fd = self.pending[i].conn.fd();
            let pid = self.pending[i].pid;
            match step {
                RxStep::Idle => {}
                RxStep::Eof => mark_dead(&mut self.dead, fd, pid, DropReason::Gone),
                RxStep::Malformed => {
                    mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol)
                }
                RxStep::Frame { msg_type, payload_len } => {
                    let mut frame = ClientFrame::new(fd, pid, msg_type);
                    frame.set_payload(self.pending[i].rx.payload(payload_len));
                    // A connection is identified by its first frame and by
                    // nothing else. `MSG_CREATE_WINDOW` promotes it to a
                    // window; anything else is a one-shot request, answered and
                    // closed — which is what a `services::connect` caller like
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
            if !self.is_ready(self.stack[i].client.conn.fd()) {
                continue;
            }
            let win = &mut self.stack[i];
            let step = win.client.rx.pump(&win.client.conn);
            let (fd, pid) = (win.client.conn.fd(), win.client.pid);
            match step {
                RxStep::Idle => {}
                RxStep::Eof => mark_dead(&mut self.dead, fd, pid, DropReason::Gone),
                RxStep::Malformed => {
                    mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol)
                }
                RxStep::Frame { msg_type, payload_len } => {
                    let mut frame = ClientFrame::new(fd, pid, msg_type);
                    frame.set_payload(win.client.rx.payload(payload_len));
                    out.push(frame);
                }
            }
        }
        out
    }

    fn dispatch(&mut self, frames: Vec<ClientFrame>) {
        for frame in frames {
            let fd = frame.fd;
            let pid = frame.pid;
            match frame.msg_type {
                window::MSG_CREATE_WINDOW => self.create_window(frame),
                window::MSG_PRESENT => {
                    let Ok(rect) = ipc::decode_payload::<window::Rect>(frame.payload()) else {
                        mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    if let Some(i) = self.stack.find(|w| w.client.conn.fd() == fd) {
                        self.stack[i].presented = true;
                        let claim = Rect::from_wire(rect.x, rect.y, rect.w, rect.h);
                        self.damage.add(self.stack[i].present_damage(claim));
                    }
                }
                window::MSG_DESTROY_WINDOW => {
                    if let Some(i) = self.stack.find(|w| w.client.conn.fd() == fd) {
                        let gone = self.stack.remove(i);
                        note_closed("the client itself", gone.client.pid, self.stack.len());
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
                        mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    // Two numbers off the wire, and both decide what this
                    // process reads. The token names a region the client says
                    // it granted — a token it never granted, or never
                    // allocated, is a refusal the compositor has to survive —
                    // and the length is its claim about how much of it is text,
                    // which past the region is a read of somebody else's memory
                    // rather than a clipboard.
                    if info.len as usize > MAX_CLIPBOARD_BYTES {
                        eprintln!(
                            "compositor: refusing {} bytes of clipboard from pid {pid}, max \
                             {MAX_CLIPBOARD_BYTES}",
                            info.len
                        );
                        continue;
                    }
                    let Ok(shm) = SharedMemory::map(info.token, info.len as usize) else {
                        mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    self.clipboard = String::from_utf8_lossy(shm.as_slice()).into_owned();
                }
                window::MSG_SET_CURSOR => {
                    let Ok(style) = ipc::decode_payload::<u32>(frame.payload()) else {
                        mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    if let Some(i) = self.stack.find(|w| w.client.conn.fd() == fd) {
                        self.stack[i].cursor_style = cursor_from_wire(style);
                    }
                }
                window::MSG_SET_RESOLUTION => {
                    let Ok(req) =
                        ipc::decode_payload::<window::ResolutionRequest>(frame.payload())
                    else {
                        mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol);
                        continue;
                    };
                    self.set_resolution(req.width, req.height);
                    self.answer_resolution(fd, pid);
                }
                // The one message a client can ask for faster than it can read
                // the answer: eight bytes in, sixteen out. Blocking here is a
                // client filling its own pipe and taking the desktop with it.
                window::MSG_GET_RESOLUTION => self.answer_resolution(fd, pid),
                _ => {}
            }
        }
    }

    fn create_window(&mut self, frame: ClientFrame) {
        let (fd, pid) = (frame.fd, frame.pid);
        // `frame.conn` is dropped by every early return here, which closes the
        // fd: there is no window to remove yet.
        let Ok(req) = ipc::decode_payload::<window::CreateWindowRequest>(frame.payload()) else {
            return;
        };
        // A window *is* a connection its first frame promoted, so a second
        // `MSG_CREATE_WINDOW` comes with nothing to promote. `conn` is `None`
        // for every frame off an established window, and reading that as a bug
        // rather than as a protocol error made one message from any client
        // fatal.
        let Some(conn) = frame.conn else {
            mark_dead(&mut self.dead, fd, pid, DropReason::OutOfProtocol);
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
                "compositor: refusing {}x{} window from pid {pid} ({} live, max {}), reason \
                 {reason}",
                req.width,
                req.height,
                self.stack.len(),
                self.max_windows
            );
            let _ =
                ipc::try_send(fd, window::MSG_WINDOW_REFUSED, &window::WindowRefused { reason });
            return;
        }

        let content = self.desk.chrome.initial_content(
            Some((req.width as i32, req.height as i32)),
            self.stack.len(),
            self.desk.screen,
        );
        let shm = match SharedMemory::allocate((content.area() * 4) as usize) {
            Ok(shm) => shm,
            Err(e) => {
                eprintln!(
                    "compositor: refusing {}x{} window from pid {pid} — there is no memory for \
                     it ({e:?})",
                    content.w(),
                    content.h()
                );
                let _ = ipc::try_send(
                    fd,
                    window::MSG_WINDOW_REFUSED,
                    &window::WindowRefused { reason: window::REFUSED_NO_MEMORY },
                );
                return;
            }
        };
        // The client can be gone before its first frame is served: `accept`
        // names the process that connected, and the frame it left in the pipe
        // outlives it. Dropping `conn` here is the whole cleanup — there is no
        // window yet.
        if shm.grant(pid).is_err() {
            eprintln!("compositor: dropping pid {pid} — {}", DropReason::Vanished.why());
            return;
        }
        let token = shm.token();
        let title = if req.title_len > 0 {
            let len = (req.title_len as usize).min(30);
            String::from_utf8_lossy(&req.title[..len]).into_owned()
        } else {
            String::new()
        };
        let at = self.stack.insert(Window::new(
            Client { conn, pid, shm, rx: ClientRx::new() },
            content,
            title,
            req.flags & window::WINDOW_FLAG_TOPMOST != 0,
            CursorStyle::Default,
        ));

        self.poller.poll_add(&self.stack[at].client.conn, IORING_POLL_IN, fd.0 as u64);
        let pixel_format = self.pixel_format();
        deliver(
            &mut self.dead,
            &self.stack[at],
            window::MSG_WINDOW_CREATED,
            &window::WindowInfo {
                token,
                width: content.w() as u32,
                height: content.h() as u32,
                stride: content.w() as u32,
                pixel_format,
            },
        );
        self.damage_all();
    }

    fn answer_resolution(&mut self, fd: Fd, pid: u32) {
        let reply =
            window::ResolutionInfo { width: self.fb_info.width, height: self.fb_info.height };
        if ipc::try_send(fd, window::MSG_RESOLUTION_CHANGED, &reply).is_err() {
            mark_dead(&mut self.dead, fd, pid, DropReason::NotReading);
        }
    }

    fn set_resolution(&mut self, width: u32, height: u32) {
        let Ok(info) = gpu::set_resolution(width, height) else {
            return;
        };
        self.fb_info = info;
        let size = info.stride as usize * info.height as usize * 4;
        self._fb_shm = SharedMemory::map(info.token[0], size)
            .expect("the scanout token the mode set just returned");
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
            .filter(|w| self.dead.iter().any(|(fd, _, _)| *fd == w.client.conn.fd()))
            .map(|w| w.frame(&self.desk.chrome))
            .collect();
        let dead = std::mem::take(&mut self.dead);
        self.stack.retain(|w| !dead.iter().any(|(fd, _, _)| *fd == w.client.conn.fd()));
        self.pending.retain(|p| !dead.iter().any(|(fd, _, _)| *fd == p.conn.fd()));
        self.dead = dead;
        if self.stack.len() != before {
            for rect in vacated {
                self.damage.add(rect);
            }
            self.damage_all();
        }
    }

    /// Re-arm the one-shot poll registrations for every fd that fired.
    fn rearm(&mut self, kb: bool, mouse: bool, listener: bool) {
        if kb {
            self.poller.poll_add(&self.kb, IORING_POLL_IN, self.kb.fd().0 as u64);
        }
        if mouse {
            self.poller.poll_add(&self.mouse, IORING_POLL_IN, self.mouse.fd().0 as u64);
        }
        if listener {
            self.poller.poll_add(&self.listener, IORING_POLL_IN, self.listener.fd().0 as u64);
        }
        for win in self.stack.iter() {
            let fd = win.client.conn.fd();
            if self.is_ready(fd) {
                self.poller.poll_add(&win.client.conn, IORING_POLL_IN, fd.0 as u64);
            }
        }
        for p in self.pending.iter() {
            let fd = p.conn.fd();
            if self.is_ready(fd) {
                self.poller.poll_add(&p.conn, IORING_POLL_IN, fd.0 as u64);
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
            gpu::present(
                region.x0 as u32,
                region.y0 as u32,
                region.w() as u32,
                region.h() as u32,
            )
            .expect("compositor owns the framebuffer");
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
            self.stack.iter().filter(|w| dead.iter().any(|(fd, _, _)| *fd == w.client.conn.fd()))
        {
            self.damage.add(win.frame(&self.desk.chrome));
        }
        self.stack.retain(|w| !dead.iter().any(|(fd, _, _)| *fd == w.client.conn.fd()));
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
    /// Over 4096 bytes it goes through shared memory. The grant is what makes
    /// the token mean anything to the window: without it the client's own
    /// `map_shared` is refused, and this path sent one for every paste over
    /// 4096 bytes.
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
                mark_dead(&mut self.dead, win.client.conn.fd(), win.client.pid, e.into());
            }
            return;
        }
        // Held until the next big paste replaces it: the window maps the token
        // after this returns, and a region dropped here is a region it cannot
        // map.
        static PASTE_SHM: std::sync::Mutex<Option<SharedMemory>> = std::sync::Mutex::new(None);
        match SharedMemory::allocate(self.clipboard.len()) {
            Ok(mut shm) if shm.grant(win.client.pid).is_ok() => {
                shm.as_mut_slice()[..self.clipboard.len()]
                    .copy_from_slice(self.clipboard.as_bytes());
                deliver(
                    &mut self.dead,
                    win,
                    window::MSG_CLIPBOARD_PASTE_SHM,
                    &window::ClipboardShmMsg {
                        token: shm.token(),
                        len: self.clipboard.len() as u32,
                    },
                );
                *PASTE_SHM.lock().unwrap() = Some(shm);
            }
            Ok(_) => mark_dead(
                &mut self.dead,
                win.client.conn.fd(),
                win.client.pid,
                DropReason::Vanished,
            ),
            Err(e) => eprintln!(
                "compositor: pid {} gets no paste — no memory for {} bytes ({e:?})",
                win.client.pid,
                self.clipboard.len()
            ),
        }
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
/// **Neither refusal is fatal, and one of them is how the desktop died.** The
/// grant names a process, and a client whose window is being maximized may have
/// exited since the compositor decided to: `grant_shared` answers
/// `InvalidArgument` for a pid the process table no longer knows, and an
/// infallible `SharedMemory::grant` over that took every other window with it.
/// The allocation is the compositor's own memory rather than the client's
/// doing, so a refusal there keeps the window at a size it can afford.
fn rebuffer(win: &mut Win, pixel_format: u32, dead: &mut Vec<Dead>) -> bool {
    let (w, h) = (win.content.w(), win.content.h());
    let old_token = win.client.shm.token();
    let new_shm = match SharedMemory::allocate(w as usize * h as usize * 4) {
        Ok(shm) => shm,
        Err(e) => {
            eprintln!(
                "compositor: pid {} keeps its {}x{} buffer — no memory for {w}x{h} ({e:?})",
                win.client.pid, win.buf_w, win.buf_h
            );
            return false;
        }
    };
    if new_shm.grant(win.client.pid).is_err() {
        mark_dead(dead, win.client.conn.fd(), win.client.pid, DropReason::Vanished);
        return false;
    }
    let token = new_shm.token();
    win.client.shm = new_shm;
    win.buf_w = w;
    win.buf_h = h;
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
