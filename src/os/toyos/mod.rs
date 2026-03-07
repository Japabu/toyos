use crate::as_filename::AsFilename;
use crate::as_symbol_name::AsSymbolName;
use crate::util::ensure_compatible_types;
use core::{fmt, marker, mem, ptr};

// ToyOS doesn't use these flags (the kernel ignores them), but libloading's
// safe wrapper passes `RTLD_LAZY | RTLD_LOCAL` by default.
pub const RTLD_LAZY: core::ffi::c_int = 1;
pub const RTLD_NOW: core::ffi::c_int = 2;
pub const RTLD_GLOBAL: core::ffi::c_int = 0x100;
pub const RTLD_LOCAL: core::ffi::c_int = 0;

unsafe fn cstr_len(s: *const core::ffi::c_char) -> usize {
    let mut len = 0;
    while unsafe { *s.add(len) } != 0 {
        len += 1;
    }
    len
}

fn toyos_dlopen(path: *const core::ffi::c_char) -> *mut core::ffi::c_void {
    let len = unsafe { cstr_len(path) };
    let bytes = unsafe { core::slice::from_raw_parts(path as *const u8, len) };
    match toyos_abi::syscall::dl_open(bytes) {
        Ok(handle) => core::ptr::without_provenance_mut((handle + 1) as usize),
        Err(_) => core::ptr::null_mut(),
    }
}

fn toyos_dlsym(handle: *mut core::ffi::c_void, name: *const core::ffi::c_char) -> *mut core::ffi::c_void {
    let h = handle as u64 - 1; // Decode handle (undo the +1 from dlopen)
    let len = unsafe { cstr_len(name) };
    let bytes = unsafe { core::slice::from_raw_parts(name as *const u8, len) };
    // SAFETY: h is a valid handle from dl_open, bytes is a valid symbol name
    match unsafe { toyos_abi::syscall::dl_sym(h, bytes) } {
        Ok(addr) => core::ptr::with_exposed_provenance_mut(addr as usize),
        Err(_) => core::ptr::null_mut(),
    }
}

fn toyos_dlclose(handle: *mut core::ffi::c_void) {
    let h = handle as u64 - 1;
    toyos_abi::syscall::dl_close(h);
}

/// A loaded dynamic library.
pub struct Library {
    handle: *mut core::ffi::c_void,
}

unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl Library {
    #[inline]
    pub unsafe fn new(filename: impl AsFilename) -> Result<Library, crate::Error> {
        Library::open(Some(filename), RTLD_LAZY | RTLD_LOCAL)
    }

    #[inline]
    pub fn this() -> Library {
        panic!("Library::this() is not supported on ToyOS")
    }

    pub unsafe fn open<P>(
        filename: Option<P>,
        _flags: core::ffi::c_int,
    ) -> Result<Library, crate::Error>
    where
        P: AsFilename,
    {
        let Some(filename) = filename else {
            return Err(crate::Error::DlOpenUnknown);
        };
        filename.toyos_filename(|cstr| {
            let handle = toyos_dlopen(cstr);
            if handle.is_null() {
                Err(crate::Error::DlOpenUnknown)
            } else {
                Ok(Library { handle })
            }
        })
    }

    unsafe fn get_impl<T, F>(
        &self,
        symbol: impl AsSymbolName,
        on_null: F,
    ) -> Result<Symbol<T>, crate::Error>
    where
        F: FnOnce() -> Result<Symbol<T>, crate::Error>,
    {
        ensure_compatible_types::<T, *mut core::ffi::c_void>()?;
        symbol.symbol_name(|cstr| {
            let pointer = toyos_dlsym(self.handle, cstr);
            if pointer.is_null() {
                on_null()
            } else {
                Ok(Symbol {
                    pointer,
                    pd: marker::PhantomData,
                })
            }
        })
    }

    #[inline(always)]
    pub unsafe fn get<T>(&self, symbol: impl AsSymbolName) -> Result<Symbol<T>, crate::Error> {
        self.get_singlethreaded(symbol)
    }

    #[inline(always)]
    pub unsafe fn get_singlethreaded<T>(
        &self,
        symbol: impl AsSymbolName,
    ) -> Result<Symbol<T>, crate::Error> {
        self.get_impl(symbol, || {
            Ok(Symbol {
                pointer: ptr::null_mut(),
                pd: marker::PhantomData,
            })
        })
    }

    pub fn into_raw(self) -> *mut core::ffi::c_void {
        let handle = self.handle;
        mem::forget(self);
        handle
    }

    pub unsafe fn from_raw(handle: *mut core::ffi::c_void) -> Library {
        Library { handle }
    }

    pub fn close(self) -> Result<(), crate::Error> {
        toyos_dlclose(self.handle);
        mem::forget(self);
        Ok(())
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        toyos_dlclose(self.handle);
    }
}

impl fmt::Debug for Library {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!("Library@{:p}", self.handle))
    }
}

/// Symbol from a library.
pub struct Symbol<T> {
    pointer: *mut core::ffi::c_void,
    pd: marker::PhantomData<T>,
}

impl<T> Symbol<T> {
    pub fn into_raw(self) -> *mut core::ffi::c_void {
        self.pointer
    }

    pub fn as_raw_ptr(self) -> *mut core::ffi::c_void {
        self.pointer
    }
}

impl<T> Symbol<Option<T>> {
    pub fn lift_option(self) -> Option<Symbol<T>> {
        if self.pointer.is_null() {
            None
        } else {
            Some(Symbol {
                pointer: self.pointer,
                pd: marker::PhantomData,
            })
        }
    }
}

unsafe impl<T: Send> Send for Symbol<T> {}
unsafe impl<T: Sync> Sync for Symbol<T> {}

impl<T> Clone for Symbol<T> {
    fn clone(&self) -> Symbol<T> {
        Symbol { ..*self }
    }
}

impl<T> core::ops::Deref for Symbol<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*(&self.pointer as *const *mut _ as *const T) }
    }
}

impl<T> fmt::Debug for Symbol<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!("Symbol@{:p}", self.pointer))
    }
}
