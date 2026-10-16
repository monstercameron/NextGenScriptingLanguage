use crate::ffi_utils::buffer::{tzname_buf, MAX_LEN};

pub(crate) fn get_timezone_inner() -> Result<String, crate::GetTimezoneError> {
    get_timezone().ok_or(crate::GetTimezoneError::OsError)
}

#[inline]
fn get_timezone() -> Option<String> {
    let mut buf = tzname_buf();
    // Get system time zone, and borrow its name.
    let tz = system_time_zone::SystemTimeZone::new()?;
    let name = tz.name()?;

    // If the name is encoded in UTF-8, copy it directly.
    let name = if let Some(name) = name.as_utf8() {
        name
    } else {
        // Otherwise convert the name to UTF-8.
        name.to_utf8(&mut buf)?
    };

    if name.is_empty() || name.len() > MAX_LEN {
        // The name should not be empty, or excessively long.
        None
    } else {
        Some(name.to_owned())
    }
}

mod system_time_zone {
    //! Safe wrappers around the CoreFoundation timezone and string APIs.

    use std::os::raw::{c_char, c_void};
    use std::sync::atomic::{AtomicPtr, Ordering};
    use std::sync::Once;

    #[repr(C)]
    struct CFTimeZoneOpaque {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub(super) struct CFStringOpaque {
        _private: [u8; 0],
    }

    type CFTimeZoneRef = *const CFTimeZoneOpaque;
    pub(super) type CFStringRef = *const CFStringOpaque;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct CFRange {
        pub(super) location: isize,
        pub(super) length: isize,
    }

    type ResetSystemFn = unsafe extern "C" fn();
    type CopySystemFn = unsafe extern "C" fn() -> CFTimeZoneRef;
    type GetNameFn = unsafe extern "C" fn(CFTimeZoneRef) -> CFStringRef;
    type ReleaseFn = unsafe extern "C" fn(*const c_void);
    pub(super) type GetCStringPtrFn = unsafe extern "C" fn(CFStringRef, u32) -> *const c_char;
    pub(super) type GetLengthFn = unsafe extern "C" fn(CFStringRef) -> isize;
    pub(super) type GetBytesFn = unsafe extern "C" fn(
        CFStringRef,
        CFRange,
        u32,
        u8,
        u8,
        *mut u8,
        isize,
        *mut isize,
    ) -> isize;

    pub(super) const CF_UTF8_ENCODING: u32 = 0x0800_0100;
    const CORE_FOUNDATION_PATH: &[u8] =
        b"/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation\0";

    /// Resolved function pointers remain valid because the `dlopen` handle is
    /// intentionally kept for the lifetime of the process.
    pub(super) struct CoreFoundationApi {
        _handle: usize,
        reset_system: ResetSystemFn,
        copy_system: CopySystemFn,
        get_name: GetNameFn,
        release: ReleaseFn,
        pub(super) get_c_string_ptr: GetCStringPtrFn,
        pub(super) get_length: GetLengthFn,
        pub(super) get_bytes: GetBytesFn,
    }

    impl CoreFoundationApi {
        fn load() -> Option<Self> {
            // SAFETY: the path is an absolute, NUL-terminated path to the
            // system CoreFoundation binary; RTLD_NOW validates its imports.
            let handle = unsafe {
                libc::dlopen(
                    CORE_FOUNDATION_PATH.as_ptr() as *const c_char,
                    libc::RTLD_NOW | libc::RTLD_LOCAL,
                )
            };
            if handle.is_null() {
                return None;
            }

            macro_rules! resolve {
                ($name:literal, $function_type:ty) => {{
                    let name = concat!($name, "\0");
                    // SAFETY: `handle` is a successful `dlopen` handle and
                    // `name` is a static NUL-terminated CoreFoundation symbol.
                    let symbol = unsafe { libc::dlsym(handle, name.as_ptr() as *const c_char) };
                    if symbol.is_null() {
                        None
                    } else {
                        // SAFETY: each call supplies the exact C declaration
                        // for the named CoreFoundation function.
                        Some(unsafe { std::mem::transmute::<*mut c_void, $function_type>(symbol) })
                    }
                }};
            }

            let functions = (|| {
                Some((
                    resolve!("CFTimeZoneResetSystem", ResetSystemFn)?,
                    resolve!("CFTimeZoneCopySystem", CopySystemFn)?,
                    resolve!("CFTimeZoneGetName", GetNameFn)?,
                    resolve!("CFRelease", ReleaseFn)?,
                    resolve!("CFStringGetCStringPtr", GetCStringPtrFn)?,
                    resolve!("CFStringGetLength", GetLengthFn)?,
                    resolve!("CFStringGetBytes", GetBytesFn)?,
                ))
            })();

            let (
                reset_system,
                copy_system,
                get_name,
                release,
                get_c_string_ptr,
                get_length,
                get_bytes,
            ) = match functions {
                Some(functions) => functions,
                None => {
                    // SAFETY: no function pointer or CF object escaped; unloading
                    // this incomplete API handle is therefore safe.
                    unsafe { libc::dlclose(handle) };
                    return None;
                }
            };

            Some(Self {
                _handle: handle as usize,
                reset_system,
                copy_system,
                get_name,
                release,
                get_c_string_ptr,
                get_length,
                get_bytes,
            })
        }
    }

    static API_INIT: Once = Once::new();
    static API_POINTER: AtomicPtr<CoreFoundationApi> = AtomicPtr::new(std::ptr::null_mut());

    fn api() -> Option<&'static CoreFoundationApi> {
        API_INIT.call_once(|| {
            if let Some(api) = CoreFoundationApi::load() {
                // Intentionally leak this allocation and its dlopen handle:
                // CF objects and function pointers must outlive all calls.
                API_POINTER.store(Box::into_raw(Box::new(api)), Ordering::Release);
            }
        });

        let pointer = API_POINTER.load(Ordering::Acquire);
        if pointer.is_null() {
            None
        } else {
            // SAFETY: `Once` initializes this pointer at most once, and the
            // pointed-to allocation is never freed or replaced.
            Some(unsafe { &*pointer })
        }
    }

    pub(crate) struct SystemTimeZone {
        timezone: CFTimeZoneRef,
        api: &'static CoreFoundationApi,
    }

    impl Drop for SystemTimeZone {
        fn drop(&mut self) {
            // SAFETY: `timezone` is the owned non-null result of
            // `CFTimeZoneCopySystem`; the API handle remains loaded forever.
            unsafe { (self.api.release)(self.timezone as *const c_void) };
        }
    }

    impl SystemTimeZone {
        /// Creates a timezone by querying the current Darwin system timezone.
        ///
        /// As in the linked implementation, reset the CoreFoundation cache on
        /// every query before requesting an owned system timezone object.
        pub(crate) fn new() -> Option<Self> {
            let api = api()?;
            // SAFETY: both pointers were resolved from CoreFoundation with
            // signatures matching its C declarations. The returned timezone
            // is owned and released by `Drop` below.
            let timezone = unsafe {
                // Keep this sequence identical to the linked backend.
                (api.reset_system)();
                (api.copy_system)()
            };
            if timezone.is_null() {
                None
            } else {
                Some(Self { timezone, api })
            }
        }

        /// Returns a borrowed name whose lifetime is tied to this timezone.
        pub(crate) fn name(&self) -> Option<super::string_ref::StringRef<'_, Self>> {
            // SAFETY: `timezone` is valid for `self`'s lifetime and the API
            // remains loaded for the lifetime of the process.
            let string = unsafe { (self.api.get_name)(self.timezone) };
            if string.is_null() {
                None
            } else {
                // SAFETY: `string` is non-null and CoreFoundation guarantees
                // the returned borrowed name remains valid while timezone is.
                Some(unsafe { super::string_ref::StringRef::new(string, self, self.api) })
            }
        }
    }
}

mod string_ref {
    //! Safe wrapper around a borrowed `CFStringRef`.

    use core::convert::TryInto;

    use super::system_time_zone::{CoreFoundationApi, SystemTimeZone};

    type CFStringRef = *const super::system_time_zone::CFStringOpaque;

    pub(crate) struct StringRef<'a, T> {
        string: CFStringRef,
        // Keep the parent timezone alive for the borrowed CoreFoundation name.
        _parent: &'a T,
        api: &'static CoreFoundationApi,
    }

    impl<'a> StringRef<'a, SystemTimeZone> {
        // SAFETY: `string` must be a valid borrowed CF string for `_parent`'s lifetime.
        pub(crate) unsafe fn new(
            string: CFStringRef,
            _parent: &'a SystemTimeZone,
            api: &'static CoreFoundationApi,
        ) -> Self {
            Self {
                string,
                _parent,
                api,
            }
        }

        pub(crate) fn as_utf8(&self) -> Option<&'a str> {
            // SAFETY: this string was returned by `CFTimeZoneGetName` from a
            // live parent timezone, and the API handle remains loaded.
            let v = unsafe {
                (self.api.get_c_string_ptr)(self.string, super::system_time_zone::CF_UTF8_ENCODING)
            };
            if !v.is_null() {
                // SAFETY: `CFStringGetCStringPtr` returns a NUL-terminated
                // pointer valid as long as the parent timezone is alive.
                let v = unsafe { std::ffi::CStr::from_ptr(v) };
                if let Ok(v) = v.to_str() {
                    return Some(v);
                }
            }
            None
        }

        pub(crate) fn to_utf8<'b>(&self, buf: &'b mut [u8]) -> Option<&'b str> {
            // SAFETY: the borrowed `CFStringRef` remains valid for self.
            let length = unsafe { (self.api.get_length)(self.string) };

            let mut buf_bytes = 0;
            let range = super::system_time_zone::CFRange {
                location: 0,
                length,
            };

            // SAFETY: `string` and range are valid; the output buffer is
            // writable and its size is passed to CoreFoundation.
            let converted_bytes = unsafe {
                (self.api.get_bytes)(
                    self.string,
                    range,
                    super::system_time_zone::CF_UTF8_ENCODING,
                    b'\0',
                    false as u8,
                    buf.as_mut_ptr(),
                    buf.len() as isize,
                    &mut buf_bytes,
                )
            };
            if converted_bytes != length {
                return None;
            }

            let len = buf_bytes.try_into().ok()?;
            let s = buf.get(..len)?;
            std::str::from_utf8(s).ok()
        }
    }
}
