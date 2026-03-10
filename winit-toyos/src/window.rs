use std::collections::VecDeque;
use std::iter;
use std::sync::{Arc, Mutex};

use dpi::{PhysicalInsets, PhysicalPosition, PhysicalSize, Position, Size};
use window as toyos_window;
use winit_core::cursor::Cursor;
use winit_core::error::{NotSupportedError, RequestError};
use winit_core::monitor::{Fullscreen, MonitorHandle as CoreMonitorHandle};
use winit_core::window::{
    self as winit_window, CursorGrabMode, ImeCapabilities, ImeRequest, ImeRequestError,
    ResizeDirection, UserAttentionType, Window as CoreWindow, WindowAttributes, WindowButtons,
    WindowId, WindowLevel,
};

use crate::event_loop::ActiveEventLoop;

/// A unique ID derived from the pointer to the underlying ToyOS window.
fn window_id_from_ptr(win: &toyos_window::Window) -> WindowId {
    WindowId::from_raw(win as *const toyos_window::Window as usize)
}

pub struct Window {
    toyos_window: Arc<Mutex<toyos_window::Window>>,
    redraws: Arc<Mutex<VecDeque<WindowId>>>,
    destroys: Arc<Mutex<VecDeque<WindowId>>>,
    window_id: WindowId,
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window").field("window_id", &self.window_id).finish_non_exhaustive()
    }
}

impl Window {
    pub(crate) fn new(
        el: &ActiveEventLoop,
        attrs: WindowAttributes,
    ) -> Result<Self, RequestError> {
        let (w, h): (u32, u32) = if let Some(size) = attrs.surface_size {
            size.to_physical::<u32>(1.0).into()
        } else {
            (1024, 768)
        };

        let toyos_win = toyos_window::Window::create_with_title(w, h, &attrs.title);
        let window_id = window_id_from_ptr(&toyos_win);
        let toyos_window = Arc::new(Mutex::new(toyos_win));

        // Notify event loop that this window was created.
        {
            let mut creates = el.creates.lock().unwrap();
            creates.push_back((toyos_window.clone(), window_id));
        }

        Ok(Self {
            toyos_window,
            redraws: el.redraws.clone(),
            destroys: el.destroys.clone(),
            window_id,
        })
    }

    /// Get a reference to the underlying ToyOS window (for raw-window-handle).
    pub fn toyos_window(&self) -> &Arc<Mutex<toyos_window::Window>> {
        &self.toyos_window
    }
}

impl CoreWindow for Window {
    fn id(&self) -> WindowId {
        self.window_id
    }

    fn ime_capabilities(&self) -> Option<ImeCapabilities> {
        None
    }

    #[inline]
    fn primary_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    #[inline]
    fn available_monitors(&self) -> Box<dyn Iterator<Item = CoreMonitorHandle>> {
        Box::new(iter::empty())
    }

    #[inline]
    fn current_monitor(&self) -> Option<CoreMonitorHandle> {
        None
    }

    #[inline]
    fn scale_factor(&self) -> f64 {
        1.0
    }

    #[inline]
    fn request_redraw(&self) {
        let window_id = self.id();
        let mut redraws = self.redraws.lock().unwrap();
        if !redraws.contains(&window_id) {
            redraws.push_back(window_id);
        }
    }

    #[inline]
    fn pre_present_notify(&self) {
        let win = self.toyos_window.lock().unwrap();
        win.present();
    }

    #[inline]
    fn reset_dead_keys(&self) {}

    #[inline]
    fn surface_position(&self) -> PhysicalPosition<i32> {
        (0, 0).into()
    }

    #[inline]
    fn outer_position(&self) -> Result<PhysicalPosition<i32>, RequestError> {
        Ok((0, 0).into())
    }

    #[inline]
    fn set_outer_position(&self, _position: Position) {}

    #[inline]
    fn surface_size(&self) -> PhysicalSize<u32> {
        let win = self.toyos_window.lock().unwrap();
        (win.width(), win.height()).into()
    }

    #[inline]
    fn request_surface_size(&self, _size: Size) -> Option<PhysicalSize<u32>> {
        None
    }

    #[inline]
    fn outer_size(&self) -> PhysicalSize<u32> {
        self.surface_size()
    }

    fn safe_area(&self) -> PhysicalInsets<u32> {
        PhysicalInsets::new(0, 0, 0, 0)
    }

    #[inline]
    fn set_min_surface_size(&self, _: Option<Size>) {}

    #[inline]
    fn set_max_surface_size(&self, _: Option<Size>) {}

    #[inline]
    fn title(&self) -> String {
        String::new()
    }

    #[inline]
    fn set_title(&self, _title: &str) {}

    #[inline]
    fn set_transparent(&self, _transparent: bool) {}

    #[inline]
    fn set_blur(&self, _blur: bool) {}

    #[inline]
    fn set_visible(&self, _visible: bool) {}

    #[inline]
    fn is_visible(&self) -> Option<bool> {
        Some(true)
    }

    #[inline]
    fn surface_resize_increments(&self) -> Option<PhysicalSize<u32>> {
        None
    }

    #[inline]
    fn set_surface_resize_increments(&self, _increments: Option<Size>) {}

    #[inline]
    fn set_resizable(&self, _resizable: bool) {}

    #[inline]
    fn is_resizable(&self) -> bool {
        true
    }

    #[inline]
    fn set_minimized(&self, _minimized: bool) {}

    #[inline]
    fn is_minimized(&self) -> Option<bool> {
        None
    }

    #[inline]
    fn set_maximized(&self, _maximized: bool) {}

    #[inline]
    fn is_maximized(&self) -> bool {
        false
    }

    fn set_fullscreen(&self, _fullscreen: Option<Fullscreen>) {}

    fn fullscreen(&self) -> Option<Fullscreen> {
        None
    }

    #[inline]
    fn set_decorations(&self, _decorations: bool) {}

    #[inline]
    fn is_decorated(&self) -> bool {
        true
    }

    #[inline]
    fn set_window_level(&self, _level: WindowLevel) {}

    #[inline]
    fn set_window_icon(&self, _window_icon: Option<winit_core::icon::Icon>) {}

    fn request_ime_update(&self, _: ImeRequest) -> Result<(), ImeRequestError> {
        Err(ImeRequestError::NotSupported)
    }

    #[inline]
    fn focus_window(&self) {}

    #[inline]
    fn request_user_attention(&self, _request_type: Option<UserAttentionType>) {}

    #[inline]
    fn set_cursor(&self, _: Cursor) {}

    #[inline]
    fn set_cursor_position(&self, _: Position) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_position is not supported").into())
    }

    #[inline]
    fn set_cursor_grab(&self, _mode: CursorGrabMode) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_grab is not supported").into())
    }

    #[inline]
    fn set_cursor_visible(&self, _visible: bool) {}

    #[inline]
    fn drag_window(&self) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_window is not supported").into())
    }

    #[inline]
    fn drag_resize_window(&self, _direction: ResizeDirection) -> Result<(), RequestError> {
        Err(NotSupportedError::new("drag_resize_window is not supported").into())
    }

    #[inline]
    fn show_window_menu(&self, _position: Position) {}

    #[inline]
    fn set_cursor_hittest(&self, _hittest: bool) -> Result<(), RequestError> {
        Err(NotSupportedError::new("set_cursor_hittest is not supported").into())
    }

    #[inline]
    fn set_enabled_buttons(&self, _buttons: WindowButtons) {}

    #[inline]
    fn enabled_buttons(&self) -> WindowButtons {
        WindowButtons::all()
    }

    #[inline]
    fn theme(&self) -> Option<winit_window::Theme> {
        None
    }

    #[inline]
    fn has_focus(&self) -> bool {
        false
    }

    #[inline]
    fn set_theme(&self, _theme: Option<winit_window::Theme>) {}

    fn set_content_protected(&self, _protected: bool) {}

    fn rwh_06_window_handle(&self) -> &dyn rwh_06::HasWindowHandle {
        self
    }

    fn rwh_06_display_handle(&self) -> &dyn rwh_06::HasDisplayHandle {
        self
    }
}

impl rwh_06::HasWindowHandle for Window {
    fn window_handle(&self) -> Result<rwh_06::WindowHandle<'_>, rwh_06::HandleError> {
        let win = self.toyos_window.lock().unwrap();
        let ptr = &*win as *const toyos_window::Window as *mut std::ffi::c_void;
        let handle = rwh_06::ToyOsWindowHandle::new(
            std::ptr::NonNull::new(ptr).expect("window pointer should never be null"),
        );
        let raw = rwh_06::RawWindowHandle::ToyOs(handle);
        unsafe { Ok(rwh_06::WindowHandle::borrow_raw(raw)) }
    }
}

impl rwh_06::HasDisplayHandle for Window {
    fn display_handle(&self) -> Result<rwh_06::DisplayHandle<'_>, rwh_06::HandleError> {
        let raw = rwh_06::RawDisplayHandle::ToyOs(rwh_06::ToyOsDisplayHandle::new());
        unsafe { Ok(rwh_06::DisplayHandle::borrow_raw(raw)) }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        {
            let mut destroys = self.destroys.lock().unwrap();
            destroys.push_back(self.id());
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlatformSpecificWindowAttributes;
