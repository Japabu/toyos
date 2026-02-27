use crate::as_filename::AsFilename;
use crate::as_symbol_name::AsSymbolName;
use crate::util::ensure_compatible_types;
use core::ffi::CStr;
use core::ptr::null;
use core::{fmt, marker, mem, ptr};

// ToyOS doesn't use these flags (the kernel ignores them), but libloading's
// safe wrapper passes `RTLD_LAZY | RTLD_LOCAL` by default.
pub const RTLD_LAZY: core::ffi::c_int = 1;
pub const RTLD_NOW: core::ffi::c_int = 2;
pub const RTLD_GLOBAL: core::ffi::c_int = 0x100;
pub const RTLD_LOCAL: core::ffi::c_int = 0;

fn with_dlerror<T, F, Error>(closure: F, error: fn(&CStr) -> Error) -> Result<T, Option<Error>>
where
    F: FnOnce() -> Option<T>,
{
    closure().ok_or_else(|| unsafe {
        let dlerror_str = dlerror();
        if dlerror_str.is_null() {
            None
        } else {
            Some(error(CStr::from_ptr(dlerror_str)))
        }
    })
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
        unsafe {
            Library::open_char_ptr(null(), RTLD_LAZY | RTLD_LOCAL).expect("this should never fail")
        }
    }

    pub unsafe fn open<P>(
        filename: Option<P>,
        flags: core::ffi::c_int,
    ) -> Result<Library, crate::Error>
    where
        P: AsFilename,
    {
        let Some(filename) = filename else {
            return Self::open_char_ptr(null(), flags);
        };
        filename.toyos_filename(|filename| Library::open_char_ptr(filename, flags))
    }

    unsafe fn open_char_ptr(
        filename: *const core::ffi::c_char,
        flags: core::ffi::c_int,
    ) -> Result<Library, crate::Error> {
        with_dlerror(
            move || {
                let result = dlopen(filename, flags);
                if result.is_null() {
                    None
                } else {
                    Some(Library { handle: result })
                }
            },
            |desc| crate::Error::DlOpen {
                source: desc.into(),
            },
        )
        .map_err(|e| e.unwrap_or(crate::Error::DlOpenUnknown))
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
        symbol.symbol_name(|posix_symbol| {
            let result = with_dlerror(
                || {
                    dlerror();
                    let symbol = dlsym(self.handle, posix_symbol);
                    if symbol.is_null() {
                        None
                    } else {
                        Some(Symbol {
                            pointer: symbol,
                            pd: marker::PhantomData,
                        })
                    }
                },
                |desc| crate::Error::DlSym {
                    source: desc.into(),
                },
            );
            match result {
                Err(None) => on_null(),
                Err(Some(e)) => Err(e),
                Ok(x) => Ok(x),
            }
        })
    }

    #[inline(always)]
    pub unsafe fn get<T>(&self, symbol: impl AsSymbolName) -> Result<Symbol<T>, crate::Error> {
        // ToyOS dlerror is trivially MT-safe (always returns null)
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
        let result = with_dlerror(
            || {
                if unsafe { dlclose(self.handle) } == 0 {
                    Some(())
                } else {
                    None
                }
            },
            |desc| crate::Error::DlClose {
                source: desc.into(),
            },
        )
        .map_err(|e| e.unwrap_or(crate::Error::DlCloseUnknown));
        mem::forget(self);
        result
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            dlclose(self.handle);
        }
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

// Symbols provided by ToyOS std (sys/pal/toyos/dl.rs)
extern "C" {
    fn dlopen(
        filename: *const core::ffi::c_char,
        flags: core::ffi::c_int,
    ) -> *mut core::ffi::c_void;
    fn dlclose(handle: *mut core::ffi::c_void) -> core::ffi::c_int;
    fn dlsym(
        handle: *mut core::ffi::c_void,
        symbol: *const core::ffi::c_char,
    ) -> *mut core::ffi::c_void;
    fn dlerror() -> *mut core::ffi::c_char;
}
