mod ffi;
mod input;
mod sound;

use ffi::boundary;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

use softbuffer::Surface;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

extern "C" {
    fn doomgeneric_Create(argc: i32, argv: *const *const u8);
    fn doomgeneric_Tick();
    static mut DG_ScreenBuffer: *mut u32;
}

static START_TIME: OnceLock<Instant> = OnceLock::new();

const SRC_W: usize = 640;
const SRC_H: usize = 400;

struct DoomApp {
    window: Option<Arc<dyn Window>>,
    surface: Option<Surface<winit::event_loop::OwnedDisplayHandle, Arc<dyn Window>>>,
    context: Option<softbuffer::Context<winit::event_loop::OwnedDisplayHandle>>,
}

impl ApplicationHandler for DoomApp {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let attrs = WindowAttributes::default()
            .with_title("DOOM")
            .with_surface_size(winit::dpi::LogicalSize::new(960, 600));
        let window: Arc<dyn Window> = event_loop.create_window(attrs).expect("failed to create window").into();

        let display = event_loop.owned_display_handle();
        let context = softbuffer::Context::new(display).expect("failed to create softbuffer context");
        let surface = Surface::new(&context, window.clone()).expect("failed to create surface");

        self.window = Some(window);
        self.surface = Some(surface);
        self.context = Some(context);

        // Leak: doom stores myargv globally and reads it on every tick.
        let argv: Vec<*const u8> = vec![
            b"doom\0".as_ptr(),
            b"-iwad\0".as_ptr(),
            b"/share/doom1.wad\0".as_ptr(),
        ];
        let argv = argv.leak();
        unsafe {
            doomgeneric_Create(argv.len() as i32, argv.as_ptr());
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &dyn ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => std::process::exit(0),
            WindowEvent::KeyboardInput { event, .. } => {
                input::handle_winit_key(&event);
            }
            WindowEvent::RedrawRequested => {
                self.draw_frame();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.window.is_some() {
            unsafe {
                doomgeneric_Tick();
            }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            // doomgeneric self-paces: TryRunTics sleeps via DG_SleepMs until the
            // next 35Hz tic, so Poll does not busy-loop.
            event_loop.set_control_flow(ControlFlow::Poll);
        }
    }
}

impl DoomApp {
    fn draw_frame(&mut self) {
        let surface = match self.surface.as_mut() {
            Some(s) => s,
            None => return,
        };

        let window = self.window.as_ref().unwrap();
        let size = window.surface_size();
        let dst_w = size.width as usize;
        let dst_h = size.height as usize;
        if dst_w == 0 || dst_h == 0 {
            return;
        }

        surface
            .resize(NonZeroU32::new(size.width).unwrap(), NonZeroU32::new(size.height).unwrap())
            .expect("failed to resize surface");

        let mut buffer = surface.next_buffer().expect("failed to get buffer");
        let stride = buffer.byte_stride().get() as usize / 4;

        unsafe {
            let src = DG_ScreenBuffer;
            if src.is_null() {
                return;
            }

            // Precompute X mapping table for nearest-neighbor scaling
            let mut x_map = [0usize; 2560];
            let map = &mut x_map[..dst_w];
            for dx in 0..dst_w {
                map[dx] = dx * SRC_W / dst_w;
            }

            let dst = buffer.pixels().as_mut_ptr() as *mut u32;
            let mut prev_sy = usize::MAX;
            for dy in 0..dst_h {
                let sy = dy * SRC_H / dst_h;
                let dst_row = dst.add(dy * stride);

                if sy == prev_sy && dy > 0 {
                    core::ptr::copy_nonoverlapping(dst.add((dy - 1) * stride), dst_row, dst_w);
                } else {
                    let src_row = src.add(sy * SRC_W);
                    for dx in 0..dst_w {
                        // DOOM's XRGB (0x00RRGGBB) → softbuffer pixel with alpha=0xFF
                        *dst_row.add(dx) = *src_row.add(*map.get_unchecked(dx)) | 0xFF000000;
                    }
                }
                prev_sy = sy;
            }
        }

        window.pre_present_notify();
        buffer.present().expect("failed to present buffer");
    }
}

// ── DG_* implementations (called by C code) ──

#[no_mangle]
pub extern "C" fn DG_Init() {
    boundary("DG_Init", (), || {
        START_TIME.set(Instant::now()).expect("DG_Init called twice");
    })
}

#[no_mangle]
pub extern "C" fn DG_DrawFrame() {}

#[no_mangle]
pub extern "C" fn DG_SleepMs(ms: u32) {
    boundary("DG_SleepMs", (), || {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    })
}

#[no_mangle]
pub extern "C" fn DG_GetTicksMs() -> u32 {
    boundary("DG_GetTicksMs", 0, || {
        START_TIME.get().expect("DG_Init not called").elapsed().as_millis() as u32
    })
}

#[no_mangle]
pub extern "C" fn DG_GetKey(pressed: *mut i32, doom_key: *mut u8) -> i32 {
    boundary("DG_GetKey", 0, || unsafe {
        let mut p = 0;
        let mut k = 0;
        if input::dequeue_key(&mut p, &mut k) {
            *pressed = p;
            *doom_key = k;
            1
        } else {
            0
        }
    })
}

#[no_mangle]
pub extern "C" fn DG_SetWindowTitle(_title: *const u8) {}

#[no_mangle]
pub extern "C" fn DG_AudioWrite(_buf: *const u8, _len: u32) {}

fn main() {
    // The producer that overflowed is doom's own game thread inside
    // `S_UpdateSounds`, so nothing outside this process can drive it; the
    // actuator lives in the binary that owns it. Driven by
    // `tests/toyos-rust-tests/src/bin/doom_sound_flood.rs`.
    if std::env::args().any(|arg| arg == "--sound-stress") {
        std::process::exit(sound::sound_stress());
    }

    let event_loop = EventLoop::new().expect("failed to create event loop");
    let app = DoomApp {
        window: None,
        surface: None,
        context: None,
    };
    event_loop.run_app(app).expect("event loop error");
}
