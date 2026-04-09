use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;
use std::iter;

use bitflags::bitflags;
use smol_str::SmolStr;
use window as toyos_window;
use winit_core::application::ApplicationHandler;
use winit_core::cursor::{CustomCursor, CustomCursorSource};
use winit_core::error::{EventLoopError, NotSupportedError, RequestError};
use winit_core::event::{self, Modifiers, StartCause};
use winit_core::event_loop::{
    ActiveEventLoop as RootActiveEventLoop, ControlFlow, DeviceEvents,
    EventLoopProxy as CoreEventLoopProxy, EventLoopProxyProvider,
    OwnedDisplayHandle as CoreOwnedDisplayHandle,
};
use winit_core::keyboard::{
    Key, KeyCode, KeyLocation, ModifiersKeys, ModifiersState, NamedKey, NativeKey, NativeKeyCode,
    PhysicalKey,
};
use winit_core::window::{Theme, Window as CoreWindow, WindowId};

use crate::window::Window;

/// Convert a USB HID keycode to a winit PhysicalKey and optional NamedKey.
fn convert_hid_keycode(keycode: u8) -> (PhysicalKey, Option<NamedKey>) {
    let (key_code, named_key_opt) = match keycode {
        // Letters: HID 0x04-0x1D = A-Z
        0x04 => (KeyCode::KeyA, None),
        0x05 => (KeyCode::KeyB, None),
        0x06 => (KeyCode::KeyC, None),
        0x07 => (KeyCode::KeyD, None),
        0x08 => (KeyCode::KeyE, None),
        0x09 => (KeyCode::KeyF, None),
        0x0A => (KeyCode::KeyG, None),
        0x0B => (KeyCode::KeyH, None),
        0x0C => (KeyCode::KeyI, None),
        0x0D => (KeyCode::KeyJ, None),
        0x0E => (KeyCode::KeyK, None),
        0x0F => (KeyCode::KeyL, None),
        0x10 => (KeyCode::KeyM, None),
        0x11 => (KeyCode::KeyN, None),
        0x12 => (KeyCode::KeyO, None),
        0x13 => (KeyCode::KeyP, None),
        0x14 => (KeyCode::KeyQ, None),
        0x15 => (KeyCode::KeyR, None),
        0x16 => (KeyCode::KeyS, None),
        0x17 => (KeyCode::KeyT, None),
        0x18 => (KeyCode::KeyU, None),
        0x19 => (KeyCode::KeyV, None),
        0x1A => (KeyCode::KeyW, None),
        0x1B => (KeyCode::KeyX, None),
        0x1C => (KeyCode::KeyY, None),
        0x1D => (KeyCode::KeyZ, None),

        // Digits: HID 0x1E-0x27 = 1-9, 0
        0x1E => (KeyCode::Digit1, None),
        0x1F => (KeyCode::Digit2, None),
        0x20 => (KeyCode::Digit3, None),
        0x21 => (KeyCode::Digit4, None),
        0x22 => (KeyCode::Digit5, None),
        0x23 => (KeyCode::Digit6, None),
        0x24 => (KeyCode::Digit7, None),
        0x25 => (KeyCode::Digit8, None),
        0x26 => (KeyCode::Digit9, None),
        0x27 => (KeyCode::Digit0, None),

        // Special keys
        0x28 => (KeyCode::Enter, Some(NamedKey::Enter)),
        0x29 => (KeyCode::Escape, Some(NamedKey::Escape)),
        0x2A => (KeyCode::Backspace, Some(NamedKey::Backspace)),
        0x2B => (KeyCode::Tab, Some(NamedKey::Tab)),
        0x2C => (KeyCode::Space, None),

        // Symbols
        0x2D => (KeyCode::Minus, None),
        0x2E => (KeyCode::Equal, None),
        0x2F => (KeyCode::BracketLeft, None),
        0x30 => (KeyCode::BracketRight, None),
        0x31 => (KeyCode::Backslash, None),
        0x33 => (KeyCode::Semicolon, None),
        0x34 => (KeyCode::Quote, None),
        0x35 => (KeyCode::Backquote, None),
        0x36 => (KeyCode::Comma, None),
        0x37 => (KeyCode::Period, None),
        0x38 => (KeyCode::Slash, None),

        // Caps Lock
        0x39 => (KeyCode::CapsLock, Some(NamedKey::CapsLock)),

        // F1-F12
        0x3A => (KeyCode::F1, Some(NamedKey::F1)),
        0x3B => (KeyCode::F2, Some(NamedKey::F2)),
        0x3C => (KeyCode::F3, Some(NamedKey::F3)),
        0x3D => (KeyCode::F4, Some(NamedKey::F4)),
        0x3E => (KeyCode::F5, Some(NamedKey::F5)),
        0x3F => (KeyCode::F6, Some(NamedKey::F6)),
        0x40 => (KeyCode::F7, Some(NamedKey::F7)),
        0x41 => (KeyCode::F8, Some(NamedKey::F8)),
        0x42 => (KeyCode::F9, Some(NamedKey::F9)),
        0x43 => (KeyCode::F10, Some(NamedKey::F10)),
        0x44 => (KeyCode::F11, Some(NamedKey::F11)),
        0x45 => (KeyCode::F12, Some(NamedKey::F12)),

        // Print Screen, Scroll Lock, Pause
        0x46 => (KeyCode::PrintScreen, Some(NamedKey::PrintScreen)),
        0x47 => (KeyCode::ScrollLock, Some(NamedKey::ScrollLock)),
        0x48 => (KeyCode::Pause, Some(NamedKey::Pause)),

        // Insert, Home, Page Up, Delete, End, Page Down
        0x49 => (KeyCode::Insert, Some(NamedKey::Insert)),
        0x4A => (KeyCode::Home, Some(NamedKey::Home)),
        0x4B => (KeyCode::PageUp, Some(NamedKey::PageUp)),
        0x4C => (KeyCode::Delete, Some(NamedKey::Delete)),
        0x4D => (KeyCode::End, Some(NamedKey::End)),
        0x4E => (KeyCode::PageDown, Some(NamedKey::PageDown)),

        // Arrow keys
        0x4F => (KeyCode::ArrowRight, Some(NamedKey::ArrowRight)),
        0x50 => (KeyCode::ArrowLeft, Some(NamedKey::ArrowLeft)),
        0x51 => (KeyCode::ArrowDown, Some(NamedKey::ArrowDown)),
        0x52 => (KeyCode::ArrowUp, Some(NamedKey::ArrowUp)),

        // Numpad
        0x53 => (KeyCode::NumLock, Some(NamedKey::NumLock)),
        0x54 => (KeyCode::NumpadDivide, None),
        0x55 => (KeyCode::NumpadMultiply, None),
        0x56 => (KeyCode::NumpadSubtract, None),
        0x57 => (KeyCode::NumpadAdd, None),
        0x58 => (KeyCode::NumpadEnter, Some(NamedKey::Enter)),
        0x59 => (KeyCode::Numpad1, None),
        0x5A => (KeyCode::Numpad2, None),
        0x5B => (KeyCode::Numpad3, None),
        0x5C => (KeyCode::Numpad4, None),
        0x5D => (KeyCode::Numpad5, None),
        0x5E => (KeyCode::Numpad6, None),
        0x5F => (KeyCode::Numpad7, None),
        0x60 => (KeyCode::Numpad8, None),
        0x61 => (KeyCode::Numpad9, None),
        0x62 => (KeyCode::Numpad0, None),
        0x63 => (KeyCode::NumpadDecimal, None),

        // Modifier keys
        0xE0 => (KeyCode::ControlLeft, Some(NamedKey::Control)),
        0xE1 => (KeyCode::ShiftLeft, Some(NamedKey::Shift)),
        0xE2 => (KeyCode::AltLeft, Some(NamedKey::Alt)),
        0xE3 => (KeyCode::MetaLeft, Some(NamedKey::Meta)),
        0xE4 => (KeyCode::ControlRight, Some(NamedKey::Control)),
        0xE5 => (KeyCode::ShiftRight, Some(NamedKey::Shift)),
        0xE6 => (KeyCode::AltRight, Some(NamedKey::AltGraph)),
        0xE7 => (KeyCode::MetaRight, Some(NamedKey::Meta)),

        _ => return (PhysicalKey::Unidentified(NativeKeyCode::Unidentified), None),
    };
    (PhysicalKey::Code(key_code), named_key_opt)
}

fn element_state(pressed: bool) -> event::ElementState {
    if pressed { event::ElementState::Pressed } else { event::ElementState::Released }
}

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct KeyboardModifierState: u8 {
        const LSHIFT = 1 << 0;
        const RSHIFT = 1 << 1;
        const LCTRL = 1 << 2;
        const RCTRL = 1 << 3;
        const LALT = 1 << 4;
        const RALT = 1 << 5;
        const LMETA = 1 << 6;
        const RMETA = 1 << 7;
    }
}

bitflags! {
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct MouseButtonState: u8 {
        const LEFT = 1 << 0;
        const MIDDLE = 1 << 1;
        const RIGHT = 1 << 2;
    }
}

#[derive(Default, Debug)]
struct EventState {
    keyboard: KeyboardModifierState,
    mouse: MouseButtonState,
}

impl EventState {
    fn key(&mut self, key: PhysicalKey, pressed: bool) {
        let code = match key {
            PhysicalKey::Code(code) => code,
            _ => return,
        };

        match code {
            KeyCode::ShiftLeft => self.keyboard.set(KeyboardModifierState::LSHIFT, pressed),
            KeyCode::ShiftRight => self.keyboard.set(KeyboardModifierState::RSHIFT, pressed),
            KeyCode::ControlLeft => self.keyboard.set(KeyboardModifierState::LCTRL, pressed),
            KeyCode::ControlRight => self.keyboard.set(KeyboardModifierState::RCTRL, pressed),
            KeyCode::AltLeft => self.keyboard.set(KeyboardModifierState::LALT, pressed),
            KeyCode::AltRight => self.keyboard.set(KeyboardModifierState::RALT, pressed),
            KeyCode::MetaLeft => self.keyboard.set(KeyboardModifierState::LMETA, pressed),
            KeyCode::MetaRight => self.keyboard.set(KeyboardModifierState::RMETA, pressed),
            _ => (),
        }
    }

    fn modifiers(&self) -> Modifiers {
        let mut state = ModifiersState::empty();
        let mut pressed_mods = ModifiersKeys::empty();

        if self.keyboard.intersects(KeyboardModifierState::LSHIFT | KeyboardModifierState::RSHIFT) {
            state |= ModifiersState::SHIFT;
        }

        pressed_mods
            .set(ModifiersKeys::LSHIFT, self.keyboard.contains(KeyboardModifierState::LSHIFT));
        pressed_mods
            .set(ModifiersKeys::RSHIFT, self.keyboard.contains(KeyboardModifierState::RSHIFT));

        if self.keyboard.intersects(KeyboardModifierState::LCTRL | KeyboardModifierState::RCTRL) {
            state |= ModifiersState::CONTROL;
        }

        pressed_mods
            .set(ModifiersKeys::LCONTROL, self.keyboard.contains(KeyboardModifierState::LCTRL));
        pressed_mods
            .set(ModifiersKeys::RCONTROL, self.keyboard.contains(KeyboardModifierState::RCTRL));

        if self.keyboard.intersects(KeyboardModifierState::LALT | KeyboardModifierState::RALT) {
            state |= ModifiersState::ALT;
        }

        pressed_mods.set(ModifiersKeys::LALT, self.keyboard.contains(KeyboardModifierState::LALT));
        pressed_mods.set(ModifiersKeys::RALT, self.keyboard.contains(KeyboardModifierState::RALT));

        if self.keyboard.intersects(KeyboardModifierState::LMETA | KeyboardModifierState::RMETA) {
            state |= ModifiersState::META
        }

        pressed_mods
            .set(ModifiersKeys::LMETA, self.keyboard.contains(KeyboardModifierState::LMETA));
        pressed_mods
            .set(ModifiersKeys::RMETA, self.keyboard.contains(KeyboardModifierState::RMETA));

        Modifiers::new(state, pressed_mods)
    }
}

pub struct EventLoop {
    window_target: ActiveEventLoop,
    user_events_receiver: mpsc::Receiver<()>,
}

impl std::fmt::Debug for EventLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLoop").finish_non_exhaustive()
    }
}

impl EventLoop {
    pub fn new(_: &PlatformSpecificEventLoopAttributes) -> Result<Self, EventLoopError> {
        static EVENT_LOOP_CREATED: AtomicBool = AtomicBool::new(false);
        if EVENT_LOOP_CREATED.swap(true, Ordering::Relaxed) {
            return Err(EventLoopError::RecreationAttempt);
        }

        let (user_events_sender, user_events_receiver) = mpsc::sync_channel(1);

        Ok(Self {
            window_target: ActiveEventLoop {
                control_flow: Cell::new(ControlFlow::default()),
                exit: Cell::new(false),
                creates: Mutex::new(VecDeque::new()),
                redraws: Arc::new(Mutex::new(VecDeque::new())),
                destroys: Arc::new(Mutex::new(VecDeque::new())),
                event_loop_proxy: Arc::new(EventLoopProxy { user_events_sender }),
            },
            user_events_receiver,
        })
    }

    fn process_key_event<A: ApplicationHandler>(
        window_id: WindowId,
        key_event: toyos_window::KeyEvent,
        event_state: &mut EventState,
        window_target: &ActiveEventLoop,
        app: &mut A,
    ) {
        let pressed = key_event.pressed();
        let (physical_key, named_key_opt) = convert_hid_keycode(key_event.keycode);

        let modifiers_before = event_state.keyboard;
        event_state.key(physical_key, pressed);

        // Build logical key from the translated character data.
        let mut logical_key = Key::Unidentified(NativeKey::Unidentified);
        let mut key_without_modifiers = logical_key.clone();
        let mut text = None;
        let mut text_with_all_modifiers = None;

        if key_event.len > 0 {
            let len = key_event.len as usize;
            if let Ok(s) = core::str::from_utf8(&key_event.translated[..len]) {
                logical_key = Key::Character(SmolStr::new(s));
                key_without_modifiers =
                    Key::Character(SmolStr::from_iter(s.chars().flat_map(|c| c.to_lowercase())));
                if pressed {
                    text = Some(SmolStr::new(s));
                    text_with_all_modifiers = Some(SmolStr::new(s));
                }
            }
        }

        if let Some(named_key) = named_key_opt {
            logical_key = Key::Named(named_key);
            key_without_modifiers = logical_key.clone();
        }

        let event = event::WindowEvent::KeyboardInput {
            device_id: None,
            event: event::KeyEvent {
                logical_key,
                physical_key,
                location: KeyLocation::Standard,
                state: element_state(pressed),
                repeat: false,
                text,
                key_without_modifiers,
                text_with_all_modifiers,
            },
            is_synthetic: false,
        };

        app.window_event(window_target, window_id, event);

        if modifiers_before != event_state.keyboard {
            app.window_event(
                window_target,
                window_id,
                event::WindowEvent::ModifiersChanged(event_state.modifiers()),
            );
        }
    }

    fn process_mouse_event<A: ApplicationHandler>(
        window_id: WindowId,
        mouse: toyos_window::MouseEvent,
        event_state: &mut EventState,
        window_target: &ActiveEventLoop,
        app: &mut A,
    ) {
        match mouse.event_type {
            toyos_window::MOUSE_MOVE => {
                app.window_event(window_target, window_id, event::WindowEvent::PointerMoved {
                    device_id: None,
                    primary: true,
                    position: (mouse.x as f64, mouse.y as f64).into(),
                    source: event::PointerSource::Mouse,
                });
            },
            toyos_window::MOUSE_PRESS => {
                let button = match mouse.changed {
                    0x01 => event::MouseButton::Left,
                    0x02 => event::MouseButton::Right,
                    0x04 => event::MouseButton::Middle,
                    _ => event::MouseButton::Left,
                };

                // Track button state.
                if mouse.changed & 0x01 != 0 {
                    event_state.mouse.set(MouseButtonState::LEFT, true);
                }
                if mouse.changed & 0x02 != 0 {
                    event_state.mouse.set(MouseButtonState::RIGHT, true);
                }
                if mouse.changed & 0x04 != 0 {
                    event_state.mouse.set(MouseButtonState::MIDDLE, true);
                }

                app.window_event(window_target, window_id, event::WindowEvent::PointerButton {
                    device_id: None,
                    primary: true,
                    state: event::ElementState::Pressed,
                    position: (mouse.x as f64, mouse.y as f64).into(),
                    button: button.into(),
                });
            },
            toyos_window::MOUSE_RELEASE => {
                let button = match mouse.changed {
                    0x01 => event::MouseButton::Left,
                    0x02 => event::MouseButton::Right,
                    0x04 => event::MouseButton::Middle,
                    _ => event::MouseButton::Left,
                };

                // Track button state.
                if mouse.changed & 0x01 != 0 {
                    event_state.mouse.set(MouseButtonState::LEFT, false);
                }
                if mouse.changed & 0x02 != 0 {
                    event_state.mouse.set(MouseButtonState::RIGHT, false);
                }
                if mouse.changed & 0x04 != 0 {
                    event_state.mouse.set(MouseButtonState::MIDDLE, false);
                }

                app.window_event(window_target, window_id, event::WindowEvent::PointerButton {
                    device_id: None,
                    primary: true,
                    state: event::ElementState::Released,
                    position: (mouse.x as f64, mouse.y as f64).into(),
                    button: button.into(),
                });
            },
            toyos_window::MOUSE_SCROLL => {
                app.window_event(window_target, window_id, event::WindowEvent::MouseWheel {
                    device_id: None,
                    delta: event::MouseScrollDelta::LineDelta(0.0, mouse.scroll as f32),
                    phase: event::TouchPhase::Moved,
                });
            },
            _ => {},
        }
    }

    pub fn run_app_on_demand<A: ApplicationHandler>(
        &mut self,
        mut app: A,
    ) -> Result<(), EventLoopError> {
        let mut start_cause = StartCause::Init;
        let mut event_state = EventState::default();
        // We use a raw pointer as the window ID. The actual pointer value is stable for a window's
        // lifetime, so this is fine. We keep a reference to the Window to poll events.
        let mut toyos_window: Option<Arc<Mutex<toyos_window::Window>>> = None;
        let mut current_window_id: Option<WindowId> = None;

        loop {
            app.new_events(&self.window_target, start_cause);

            if start_cause == StartCause::Init {
                app.can_create_surfaces(&self.window_target);
            }

            // Handle window creates.
            while let Some((win, wid)) = {
                let mut creates = self.window_target.creates.lock().unwrap();
                creates.pop_front()
            } {
                toyos_window = Some(win.clone());
                current_window_id = Some(wid);

                let (w, h) = {
                    let w = win.lock().unwrap();
                    (w.width(), w.height())
                };

                app.window_event(
                    &self.window_target,
                    wid,
                    event::WindowEvent::SurfaceResized((w, h).into()),
                );
            }

            // Handle window destroys.
            while let Some(destroy_id) = {
                let mut destroys = self.window_target.destroys.lock().unwrap();
                destroys.pop_front()
            } {
                app.window_event(&self.window_target, destroy_id, event::WindowEvent::Destroyed);
                if current_window_id == Some(destroy_id) {
                    toyos_window = None;
                    current_window_id = None;
                }
            }

            // Poll events from the ToyOS window.
            if let (Some(win), Some(wid)) = (&toyos_window, current_window_id) {
                loop {
                    let event_opt = {
                        let mut w = win.lock().unwrap();
                        w.poll_event(0)
                    };
                    match event_opt {
                        Some(toyos_window::Event::KeyInput(key_event)) => {
                            Self::process_key_event(
                                wid,
                                key_event,
                                &mut event_state,
                                &self.window_target,
                                &mut app,
                            );
                        },
                        Some(toyos_window::Event::MouseInput(mouse_event)) => {
                            Self::process_mouse_event(
                                wid,
                                mouse_event,
                                &mut event_state,
                                &self.window_target,
                                &mut app,
                            );
                        },
                        Some(toyos_window::Event::Resized) => {
                            let (w, h) = {
                                let w = win.lock().unwrap();
                                (w.width(), w.height())
                            };
                            app.window_event(
                                &self.window_target,
                                wid,
                                event::WindowEvent::SurfaceResized((w, h).into()),
                            );

                            let mut redraws = self.window_target.redraws.lock().unwrap();
                            if !redraws.contains(&wid) {
                                redraws.push_back(wid);
                            }
                        },
                        Some(toyos_window::Event::Close) => {
                            app.window_event(
                                &self.window_target,
                                wid,
                                event::WindowEvent::CloseRequested,
                            );
                        },
                        Some(toyos_window::Event::Frame) => {
                            // Frame events indicate the compositor is ready for a new frame.
                            // Request a redraw.
                            let mut redraws = self.window_target.redraws.lock().unwrap();
                            if !redraws.contains(&wid) {
                                redraws.push_back(wid);
                            }
                        },
                        Some(toyos_window::Event::ClipboardPaste(_)) => {
                            // Clipboard paste events are not directly mapped to winit events.
                        },
                        None => break,
                    }
                }
            }

            while self.user_events_receiver.try_recv().is_ok() {
                app.proxy_wake_up(&self.window_target);
            }

            // Dispatch redraws.
            while let Some(window_id) = {
                let mut redraws = self.window_target.redraws.lock().unwrap();
                redraws.pop_front()
            } {
                app.window_event(
                    &self.window_target,
                    window_id,
                    event::WindowEvent::RedrawRequested,
                );
            }

            app.about_to_wait(&self.window_target);

            if self.window_target.exiting() {
                break;
            }

            match self.window_target.control_flow() {
                ControlFlow::Poll => {
                    start_cause = StartCause::Poll;
                    continue;
                },
                ControlFlow::Wait => {
                    // Block until the next event arrives.
                    if let Some(win) = &toyos_window {
                        let mut w = win.lock().unwrap();
                        let event = w.recv_event();
                        // We got an event; put it back by processing it next iteration.
                        // We need to handle it, so decode and push as needed.
                        drop(w);
                        // Process the event we just received immediately by re-entering the loop.
                        // To avoid complexity, we decode the event here and push a synthetic redraw
                        // or handle it at the top of the next iteration.
                        // Actually, we need to handle the blocking event. Let's process it inline.
                        if let Some(wid) = current_window_id {
                            match event {
                                toyos_window::Event::KeyInput(key_event) => {
                                    Self::process_key_event(
                                        wid,
                                        key_event,
                                        &mut event_state,
                                        &self.window_target,
                                        &mut app,
                                    );
                                },
                                toyos_window::Event::MouseInput(mouse_event) => {
                                    Self::process_mouse_event(
                                        wid,
                                        mouse_event,
                                        &mut event_state,
                                        &self.window_target,
                                        &mut app,
                                    );
                                },
                                toyos_window::Event::Resized => {
                                    let (w, h) = {
                                        let w = win.lock().unwrap();
                                        (w.width(), w.height())
                                    };
                                    app.window_event(
                                        &self.window_target,
                                        wid,
                                        event::WindowEvent::SurfaceResized((w, h).into()),
                                    );
                                    let mut redraws =
                                        self.window_target.redraws.lock().unwrap();
                                    if !redraws.contains(&wid) {
                                        redraws.push_back(wid);
                                    }
                                },
                                toyos_window::Event::Close => {
                                    app.window_event(
                                        &self.window_target,
                                        wid,
                                        event::WindowEvent::CloseRequested,
                                    );
                                },
                                toyos_window::Event::Frame => {
                                    let mut redraws =
                                        self.window_target.redraws.lock().unwrap();
                                    if !redraws.contains(&wid) {
                                        redraws.push_back(wid);
                                    }
                                },
                                toyos_window::Event::ClipboardPaste(_) => {},
                            }
                        }
                    }
                    start_cause = StartCause::WaitCancelled { start: Instant::now(), requested_resume: None };
                },
                ControlFlow::WaitUntil(instant) => {
                    let start = Instant::now();
                    if let Some(duration) = instant.checked_duration_since(start) {
                        let timeout_ns = duration.as_nanos() as u64;
                        if let Some(win) = &toyos_window {
                            let event_opt = {
                                let mut w = win.lock().unwrap();
                                w.poll_event(timeout_ns)
                            };
                            if let Some(event) = event_opt {
                                if let Some(wid) = current_window_id {
                                    match event {
                                        toyos_window::Event::KeyInput(key_event) => {
                                            Self::process_key_event(
                                                wid,
                                                key_event,
                                                &mut event_state,
                                                &self.window_target,
                                                &mut app,
                                            );
                                        },
                                        toyos_window::Event::MouseInput(mouse_event) => {
                                            Self::process_mouse_event(
                                                wid,
                                                mouse_event,
                                                &mut event_state,
                                                &self.window_target,
                                                &mut app,
                                            );
                                        },
                                        toyos_window::Event::Resized => {
                                            let (w, h) = {
                                                let w = win.lock().unwrap();
                                                (w.width(), w.height())
                                            };
                                            app.window_event(
                                                &self.window_target,
                                                wid,
                                                event::WindowEvent::SurfaceResized(
                                                    (w, h).into(),
                                                ),
                                            );
                                            let mut redraws =
                                                self.window_target.redraws.lock().unwrap();
                                            if !redraws.contains(&wid) {
                                                redraws.push_back(wid);
                                            }
                                        },
                                        toyos_window::Event::Close => {
                                            app.window_event(
                                                &self.window_target,
                                                wid,
                                                event::WindowEvent::CloseRequested,
                                            );
                                        },
                                        toyos_window::Event::Frame => {
                                            let mut redraws =
                                                self.window_target.redraws.lock().unwrap();
                                            if !redraws.contains(&wid) {
                                                redraws.push_back(wid);
                                            }
                                        },
                                        toyos_window::Event::ClipboardPaste(_) => {},
                                    }
                                }
                                start_cause = StartCause::WaitCancelled {
                                    start,
                                    requested_resume: Some(instant),
                                };
                            } else {
                                start_cause = StartCause::ResumeTimeReached {
                                    start,
                                    requested_resume: instant,
                                };
                            }
                        } else {
                            start_cause = StartCause::ResumeTimeReached {
                                start,
                                requested_resume: instant,
                            };
                        }
                    } else {
                        start_cause = StartCause::ResumeTimeReached {
                            start,
                            requested_resume: instant,
                        };
                    }
                },
            }
        }

        Ok(())
    }

    pub fn window_target(&self) -> &dyn RootActiveEventLoop {
        &self.window_target
    }
}

#[derive(Debug)]
pub struct EventLoopProxy {
    user_events_sender: mpsc::SyncSender<()>,
}

impl EventLoopProxyProvider for EventLoopProxy {
    fn wake_up(&self) {
        let _ = self.user_events_sender.try_send(());
    }
}

impl Unpin for EventLoopProxy {}

pub struct ActiveEventLoop {
    control_flow: Cell<ControlFlow>,
    exit: Cell<bool>,
    pub(super) creates: Mutex<VecDeque<(Arc<Mutex<toyos_window::Window>>, WindowId)>>,
    pub(super) redraws: Arc<Mutex<VecDeque<WindowId>>>,
    pub(super) destroys: Arc<Mutex<VecDeque<WindowId>>>,
    pub(super) event_loop_proxy: Arc<EventLoopProxy>,
}

impl std::fmt::Debug for ActiveEventLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveEventLoop")
            .field("control_flow", &self.control_flow)
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

impl RootActiveEventLoop for ActiveEventLoop {
    fn create_proxy(&self) -> CoreEventLoopProxy {
        CoreEventLoopProxy::new(self.event_loop_proxy.clone())
    }

    fn create_window(
        &self,
        window_attributes: winit_core::window::WindowAttributes,
    ) -> Result<Box<dyn CoreWindow>, RequestError> {
        Ok(Box::new(Window::new(self, window_attributes)?))
    }

    fn create_custom_cursor(&self, _: CustomCursorSource) -> Result<CustomCursor, RequestError> {
        Err(NotSupportedError::new("create_custom_cursor is not supported").into())
    }

    fn available_monitors(&self) -> Box<dyn Iterator<Item = winit_core::monitor::MonitorHandle>> {
        Box::new(iter::empty())
    }

    fn system_theme(&self) -> Option<Theme> {
        None
    }

    fn primary_monitor(&self) -> Option<winit_core::monitor::MonitorHandle> {
        None
    }

    fn listen_device_events(&self, _allowed: DeviceEvents) {}

    fn set_control_flow(&self, control_flow: ControlFlow) {
        self.control_flow.set(control_flow)
    }

    fn control_flow(&self) -> ControlFlow {
        self.control_flow.get()
    }

    fn exit(&self) {
        self.exit.set(true);
    }

    fn exiting(&self) -> bool {
        self.exit.get()
    }

    fn owned_display_handle(&self) -> CoreOwnedDisplayHandle {
        CoreOwnedDisplayHandle::new(Arc::new(OwnedDisplayHandle))
    }

    fn rwh_06_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }
}

impl rwh_06::HasDisplayHandle for ActiveEventLoop {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::RawDisplayHandle::ToyOs(rwh_06::ToyOsDisplayHandle::new());
        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw)) }
    }
}

#[derive(Clone)]
pub(crate) struct OwnedDisplayHandle;

impl rwh_06::HasDisplayHandle for OwnedDisplayHandle {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::RawDisplayHandle::ToyOs(rwh_06::ToyOsDisplayHandle::new());
        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw)) }
    }
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct PlatformSpecificEventLoopAttributes {}
