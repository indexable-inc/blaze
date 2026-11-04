//! Finding the descriptor of the image this code is in, and checking that a range of
//! addresses is in that image: with `dladdr` and `dlsym` on Linux and macOS, with the module's
//! handle and `GetProcAddress` on Windows. blaze-live's platforms check their patches' data with
//! `in_image` and `image_base` too (Unix).

#[cfg(unix)]
pub use unix::{descriptor, image_base, in_image, in_image_of, outside};
#[cfg(windows)]
pub use windows::{descriptor, in_image_of};

#[cfg(not(any(unix, windows)))]
mod none {
    use std::ops::Range;

    const PLATFORMS: &str = "a live image is found on Linux, macOS and Windows";

    pub fn descriptor() -> Result<(u64, &'static [u8]), String> {
        // alloc: error path, on a platform it doesn't run on
        Err(PLATFORMS.to_owned())
    }

    pub fn in_image_of(_: &Range<u64>, _: u64) -> Result<(), String> {
        // alloc: error path, on a platform it doesn't run on
        Err(PLATFORMS.to_owned())
    }
}
#[cfg(not(any(unix, windows)))]
pub use none::{descriptor, in_image_of};

/// What a range outside the image at `base` is called in an error.
#[cfg(any(unix, windows))]
fn outside_text(range: &std::ops::Range<u64>, base: u64) -> String {
    // alloc: error path
    format!("data at {:#x}..{:#x}, outside the image at {base:#x}", range.start, range.end)
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::{CStr, CString, c_void},
        ops::Range,
    };

    use super::super::{DESCRIPTOR, Descriptor, mem};

    /// The image that has `address`: where it starts and its path as the loader has it.
    fn image_of(address: *const c_void) -> Option<(u64, *const libc::c_char)> {
        // SAFETY: a `Dl_info` is plain pointers, for which zero is a value.
        let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a whole `Dl_info` for dladdr to fill.
        (unsafe { libc::dladdr(address, &mut info) } != 0)
            .then_some((info.dli_fbase as u64, info.dli_fname))
    }

    /// Where the image that has `address` starts, for the platforms' check that a patch's data
    /// is in the image it was linked for.
    pub fn image_base(address: u64) -> Option<u64> {
        image_of(address as *const c_void).map(|it| it.0)
    }

    /// Checks that the first and the last byte of `range` are in the image at `base`. The
    /// loader maps an image as one run of addresses; a gap within it is refused by the
    /// platform's walk of the mappings (unmapped, or mapped with no access).
    pub fn in_image(range: &Range<u64>, base: u64) -> Result<(), String> {
        let last = range.end.saturating_sub(1).max(range.start);
        if image_base(range.start) == Some(base) && image_base(last) == Some(base) {
            Ok(())
        } else {
            Err(outside(range, base))
        }
    }

    /// What a range outside the image at `base` is called in an error.
    pub fn outside(range: &Range<u64>, base: u64) -> String {
        super::outside_text(range, base)
    }

    /// The descriptor of the image `base` from `handle`, if that is where `handle` finds it.
    fn lookup(handle: *mut c_void, name: &CStr, base: u64) -> Option<u64> {
        if handle.is_null() {
            return None;
        }
        // SAFETY: the handle is open, the name ends in a zero.
        let found = unsafe { libc::dlsym(handle, name.as_ptr()) };
        (!found.is_null() && image_of(found)?.0 == base).then_some(found as u64)
    }

    /// Where the descriptor of the image this code is in is, in memory, and its bytes.
    pub fn descriptor() -> Result<(u64, &'static [u8]), String> {
        let at = find()?;
        // alloc: error path
        let base = image_base(at).ok_or_else(|| format!("no image has {DESCRIPTOR} at {at:#x}"))?;
        let end =
            at.checked_add(Descriptor::SIZE as u64).ok_or("a descriptor past the end of memory")?;
        in_image(&(at..end), base)?;
        // SAFETY: `at..end` is in the image at `base` (checked above), which stays loaded as
        // long as this code, the image's own, runs; it is the descriptor the linker wrote in
        // read-only data, which no patch writes over.
        Ok((at, unsafe { mem::view(at, Descriptor::SIZE) }))
    }

    fn find() -> Result<u64, String> {
        let name = CString::new(DESCRIPTOR).map_err(|_| "a descriptor name with a zero")?;
        let own = descriptor as fn() -> Result<(u64, &'static [u8]), String> as *const c_void;
        let (base, path) =
            image_of(own).ok_or("the loader knows no image of the dev runtime's own code")?;
        // SAFETY: a null path asks for the program's handle, which is never unloaded.
        let program = unsafe { libc::dlopen(std::ptr::null(), libc::RTLD_LAZY) };
        if let Some(found) = lookup(program, &name, base) {
            return Ok(found);
        }
        // A shared library that links the runtime is found by its own path.
        let path = (!path.is_null())
            .then(|| {
                // SAFETY: the loader's path of a loaded image ends in a zero and lives as long
                // as it does.
                unsafe { CStr::from_ptr(path) }
            })
            .filter(|it| !it.is_empty());
        if let Some(path) = path {
            // SAFETY: NOLOAD opens nothing new; the handle is closed below.
            let handle =
                unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_NOLOAD) };
            let found = lookup(handle, &name, base);
            if !handle.is_null() {
                // SAFETY: opened above; the image stays loaded, as it was before.
                unsafe { libc::dlclose(handle) };
            }
            if let Some(found) = found {
                return Ok(found);
            }
        }
        let path =
            // alloc: error path
            path.map_or_else(|| "the program".to_owned(), |it| it.to_string_lossy().into_owned());
        // alloc: error path
        Err(format!("{path} exports no {DESCRIPTOR}: blaze-link links it for live patches"))
    }

    /// Checks that `range` is in the image that has `near` (the descriptor's address).
    pub fn in_image_of(range: &Range<u64>, near: u64) -> Result<(), String> {
        // alloc: error path
        let base = image_base(near).ok_or_else(|| format!("no image has {near:#x}"))?;
        in_image(range, base)
    }
}

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, ops::Range};

    use super::super::{DESCRIPTOR, Descriptor, mem};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    const FROM_ADDRESS: u32 = 0x4;
    const UNCHANGED_REFCOUNT: u32 = 0x2;

    /// Where the module that has `address` is loaded, if one has it.
    fn module_of(address: *const c_void) -> Option<*mut c_void> {
        let mut module = std::ptr::null_mut();
        // SAFETY: with FROM_ADDRESS the name is an address in the module; the count of
        // references is left alone, and the image stays loaded as long as its code runs.
        let found = unsafe {
            GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, address as _, &mut module)
        };
        (found != 0 && !module.is_null()).then_some(module)
    }

    /// Checks that the first and the last byte of `range` are in the module at `base`, as
    /// the loader has them (one run of addresses from its base): the Windows twin of Unix's
    /// `dladdr` check. blaze-live's switch checks a patch's data page by page itself.
    fn in_module(range: &Range<u64>, base: u64) -> Result<(), String> {
        let last = range.end.saturating_sub(1).max(range.start);
        let of = |at: u64| module_of(at as *const c_void).map(|it| it as u64);
        if of(range.start) == Some(base) && of(last) == Some(base) {
            Ok(())
        } else {
            Err(super::outside_text(range, base))
        }
    }

    /// Where the descriptor of the module this code is in is, in memory, and its bytes.
    pub fn descriptor() -> Result<(u64, &'static [u8]), String> {
        let own = descriptor as fn() -> Result<(u64, &'static [u8]), String> as *const c_void;
        let module = module_of(own).ok_or_else(|| {
            // alloc: error path
            format!("no module has the dev runtime's code: {}", std::io::Error::last_os_error())
        })?;
        // alloc: setup, the descriptor's name, looked up once at start
        let name: Vec<u8> = DESCRIPTOR.bytes().chain([0]).collect();
        // SAFETY: the module is loaded and the name ends in a zero.
        let found = unsafe { GetProcAddress(module, name.as_ptr()) };
        if found.is_null() {
            // alloc: error path
            return Err(format!(
                "the module of the dev runtime's code exports no {DESCRIPTOR}: blaze-link links \
                 it for live patches"
            ));
        }
        let at = found as u64;
        let end =
            at.checked_add(Descriptor::SIZE as u64).ok_or("a descriptor past the end of memory")?;
        in_module(&(at..end), module as u64)?;
        // SAFETY: `at..end` is in the module (checked above), which stays loaded as long as its
        // code, this, runs; it is the descriptor the linker wrote in read-only data, which no
        // patch writes over.
        Ok((at, unsafe { mem::view(at, Descriptor::SIZE) }))
    }

    /// Checks that `range` is in the module that has `near` (the descriptor's address).
    pub fn in_image_of(range: &Range<u64>, near: u64) -> Result<(), String> {
        let module = module_of(near as *const c_void)
            // alloc: error path
            .ok_or_else(|| format!("no module has {near:#x}"))?;
        in_module(range, module as u64)
    }
}
