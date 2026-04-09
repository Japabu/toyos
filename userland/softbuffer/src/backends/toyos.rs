use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::slice;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawWindowHandle, ToyOsWindowHandle};

use crate::backend_interface::*;
use crate::{AlphaMode, Pixel, Rect, SoftBufferError};
use crate::error::InitError;

#[derive(Debug)]
struct ThreadSafeWindowHandle(ToyOsWindowHandle);
unsafe impl Send for ThreadSafeWindowHandle {}
unsafe impl Sync for ThreadSafeWindowHandle {}

#[derive(Debug)]
pub struct ToyOsImpl<D, W> {
    handle: ThreadSafeWindowHandle,
    width: u32,
    height: u32,
    back_buffer: Vec<Pixel>,
    window_handle: W,
    _display: PhantomData<D>,
}

impl<D: HasDisplayHandle, W: HasWindowHandle> ToyOsImpl<D, W> {
    fn window_ref(&self) -> &window::Window {
        unsafe { &*(self.handle.0.window.as_ptr() as *const window::Window) }
    }
}

impl<D: HasDisplayHandle, W: HasWindowHandle> SurfaceInterface<D, W> for ToyOsImpl<D, W> {
    type Context = D;
    type Buffer<'surface>
        = BufferImpl<'surface>
    where
        Self: 'surface;

    fn new(window: W, _display: &D) -> Result<Self, InitError<W>> {
        let raw = window.window_handle()?.as_raw();
        let RawWindowHandle::ToyOs(handle) = raw else {
            return Err(InitError::Unsupported(window));
        };

        Ok(Self {
            handle: ThreadSafeWindowHandle(handle),
            width: 0,
            height: 0,
            back_buffer: Vec::new(),
            window_handle: window,
            _display: PhantomData,
        })
    }

    #[inline]
    fn window(&self) -> &W {
        &self.window_handle
    }

    #[inline]
    fn supports_alpha_mode(&self, alpha_mode: AlphaMode) -> bool {
        matches!(alpha_mode, AlphaMode::Opaque | AlphaMode::Ignored)
    }

    fn configure(
        &mut self,
        width: NonZeroU32,
        height: NonZeroU32,
        _alpha_mode: AlphaMode,
    ) -> Result<(), SoftBufferError> {
        self.width = width.get();
        self.height = height.get();
        let window = self.window_ref();
        let fb = window.framebuffer();
        self.back_buffer.resize(fb.stride() * fb.height(), Pixel::new_rgb(0, 0, 0));
        Ok(())
    }

    fn next_buffer(&mut self, _alpha_mode: AlphaMode) -> Result<BufferImpl<'_>, SoftBufferError> {
        let window_ptr = self.handle.0.window.as_ptr() as *const window::Window;
        let fb = unsafe { &*window_ptr }.framebuffer();
        let stride = fb.stride() as u32;

        Ok(BufferImpl {
            window: unsafe { &*window_ptr },
            width: self.width,
            height: self.height,
            stride,
            pixels: &mut self.back_buffer,
        })
    }
}

#[derive(Debug)]
pub struct BufferImpl<'surface> {
    window: &'surface window::Window,
    width: u32,
    height: u32,
    stride: u32,
    pixels: &'surface mut [Pixel],
}

impl BufferInterface for BufferImpl<'_> {
    fn byte_stride(&self) -> NonZeroU32 {
        NonZeroU32::new(self.stride * 4).unwrap()
    }

    fn width(&self) -> NonZeroU32 {
        NonZeroU32::new(self.width).unwrap()
    }

    fn height(&self) -> NonZeroU32 {
        NonZeroU32::new(self.height).unwrap()
    }

    #[inline]
    fn pixels_mut(&mut self) -> &mut [Pixel] {
        self.pixels
    }

    fn age(&self) -> u8 {
        1
    }

    fn present_with_damage(self, _damage: &[Rect]) -> Result<(), SoftBufferError> {
        let fb = self.window.framebuffer();
        let pixel_count = fb.stride() * fb.height();
        let dst = unsafe {
            slice::from_raw_parts_mut(fb.ptr() as *mut Pixel, pixel_count)
        };
        let len = dst.len().min(self.pixels.len());
        dst[..len].copy_from_slice(&self.pixels[..len]);
        self.window.present();
        Ok(())
    }
}
