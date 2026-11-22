//! The dynamic loader on macOS: `dlopen`, `dlsym`, `dlerror`, `dlclose` and `dladdr`
//! (dlfcn.h, libSystem's libdyld) and dyld's image list (`_dyld_*`, mach-o/dyld.h).
//!
//! macOS only. On Linux the loader is the C library's (`ld.so`), with no kernel form, and
//! blaze-sys references no C-library symbol there. What a Linux caller uses instead:
//!
//! - its own image's program headers (what `dl_iterate_phdr` gave for the executable): the
//!   auxiliary vector's `AT_PHDR` (3) and `AT_PHNUM` (5), from the runtime's
//!   `crate::facts::Aux`, handed in;
//!   the other loaded objects: `/proc/self/maps`;
//! - a name in the user program (the launcher's `dlsym(RTLD_DEFAULT, ..)`, `dlopen` of a
//!   native library): the user program's C library, which is a different question from
//!   blaze-sys's own references.

use core::ffi::{CStr, c_char, c_int, c_void};
use core::ptr::NonNull;

/// `RTLD_DEFAULT` (dlfcn.h): the handle that searches every image in load order. -2 on macOS.
///
/// replaces libc::RTLD_DEFAULT
pub const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

flag_set! {
    /// `dlopen`'s mode (dlfcn.h, macOS): when symbols are bound and who else sees them.
    RtldFlags: c_int {
        /// `RTLD_LAZY`: functions bound at their first call.
        ///
        /// replaces libc::RTLD_LAZY
        LAZY = 0x1;
        /// `RTLD_NOW`: everything bound before `dlopen` returns; a missing symbol fails it.
        ///
        /// replaces libc::RTLD_NOW
        NOW = 0x2;
        /// `RTLD_LOCAL`: the image's symbols are not used to bind later images.
        ///
        /// replaces libc::RTLD_LOCAL
        LOCAL = 0x4;
        /// `RTLD_GLOBAL`: they are.
        ///
        /// replaces libc::RTLD_GLOBAL
        GLOBAL = 0x8;
        /// `RTLD_NOLOAD`: nothing loaded; the handle of an image already loaded, or null
        /// (a test for whether it is).
        ///
        /// replaces libc::RTLD_NOLOAD
        NOLOAD = 0x10;
    }
}

/// `Dl_info` (dlfcn.h): what `dladdr` says of an address: the image's path and base, and the
/// nearest symbol at or below it with its address (null where none is exported).
///
/// replaces libc::Dl_info
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Dl_info {
    pub dli_fname: *const c_char,
    pub dli_fbase: *mut c_void,
    pub dli_sname: *const c_char,
    pub dli_saddr: *mut c_void,
}

impl Dl_info {
    /// All null: the out-parameter `dladdr` fills.
    pub const ZERO: Dl_info = Dl_info {
        dli_fname: core::ptr::null(),
        dli_fbase: core::ptr::null_mut(),
        dli_sname: core::ptr::null(),
        dli_saddr: core::ptr::null_mut(),
    };
}

// dlfcn.h, macOS arm64: four pointers.
const _: () = {
    assert!(size_of::<Dl_info>() == 32);
    assert!(align_of::<Dl_info>() == 8);
    assert!(core::mem::offset_of!(Dl_info, dli_fname) == 0);
    assert!(core::mem::offset_of!(Dl_info, dli_fbase) == 8);
    assert!(core::mem::offset_of!(Dl_info, dli_sname) == 16);
    assert!(core::mem::offset_of!(Dl_info, dli_saddr) == 24);
};

unsafe extern "C" {
    /// `dlopen` (dlfcn.h): the image at `path` loaded (or the one already loaded counted
    /// again) with `mode` ([`RtldFlags::bits`]); null on failure, with [`dlerror`] saying why.
    ///
    /// replaces libc::dlopen
    pub fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;

    /// `dlerror` (dlfcn.h): the calling thread's last loader error as text, valid until its
    /// next loader call; null where none is pending (and reading it clears it).
    ///
    /// replaces libc::dlerror
    pub fn dlerror() -> *mut c_char;

    /// `dlclose` (dlfcn.h): one count of `handle` given back.
    ///
    /// replaces libc::dlclose
    pub fn dlclose(handle: *mut c_void) -> c_int;

    /// `dladdr` (dlfcn.h): what image and symbol `address` is in, into `info`; 0 where it is in
    /// no image.
    ///
    /// replaces libc::dladdr
    pub fn dladdr(address: *const c_void, info: *mut Dl_info) -> c_int;
}

/// `dlsym` (dlfcn.h): the address of `symbol` in `handle` (or [`RTLD_DEFAULT`]); null where
/// there is none. macos.rs's declaration, which its table of calls found at first use binds
/// with.
///
/// # Safety
///
/// `handle` is [`RTLD_DEFAULT`], `RTLD_NEXT` or a live handle from [`dlopen`], and `symbol` a
/// live NUL-terminated string.
///
/// replaces libc::dlsym
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void {
    // SAFETY: the caller's contract.
    unsafe { crate::macos::ffi::dlsym(handle, symbol) }
}

/// The address of `name` in the image `handle` ([`RTLD_DEFAULT`]: any), `None` where it is
/// not there: `dlsym`. Used for symbols a newer system has (`getattrlistbulk`,
/// `__unw_add_find_dynamic_unwind_sections`) and a native library's descriptor.
///
/// # Safety
///
/// `handle` is [`RTLD_DEFAULT`] or a live handle from [`dlopen`].
///
/// replaces libc::dlsym (the typed form)
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn symbol(handle: *mut c_void, name: &CStr) -> Option<NonNull<c_void>> {
    // SAFETY: the caller's contract for `handle`; `name` is a live NUL-terminated string.
    NonNull::new(unsafe { dlsym(handle, name.as_ptr()) })
}

/// What image and exported symbol `address` is in: `dladdr`, `None` where it is in no image.
/// Used by the symbolizers and a native library's own path.
///
/// replaces libc::dladdr (the typed form)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn address_info(address: *const c_void) -> Option<Dl_info> {
    let mut info = Dl_info::ZERO;
    // SAFETY: `dladdr` reads only the loader's tables (never `address` itself) and writes one
    // `Dl_info` into the live local given; the strings it points to are the loader's.
    let found = unsafe { dladdr(address, &raw mut info) };
    if found == 0 { None } else { Some(info) }
}

// ---------------------------------------------------------------------------------------------
// dyld's image list.
// ---------------------------------------------------------------------------------------------

#[allow(clashing_extern_declarations, reason = "blaze-sys's own typed signatures; macos_sample.rs declares `_dyld_get_image_vmaddr_slide` with the same types")]
unsafe extern "C" {
    /// `_dyld_image_count` (mach-o/dyld.h): how many images are loaded now; image 0 is the
    /// executable. Another thread may load or unload one between calls.
    ///
    /// replaces libc::_dyld_image_count
    /// replaces libc::dl_iterate_phdr (macOS: _dyld_image_count with _dyld_get_image_header and _dyld_get_image_vmaddr_slide; Linux has no kernel form: AT_PHDR/AT_PHNUM and /proc/self/maps)
    /// replaces libc::dl_phdr_info (macOS: the image's header and slide from the _dyld_* calls; Linux has no kernel form)
    pub safe fn _dyld_image_count() -> u32;

    /// `_dyld_get_image_name` (mach-o/dyld.h): image `index`'s path; null past the end.
    ///
    /// replaces libc::_dyld_get_image_name
    pub fn _dyld_get_image_name(index: u32) -> *const c_char;

    /// `_dyld_get_image_header` (mach-o/dyld.h): image `index`'s `struct mach_header` (a
    /// `mach_header_64` on a 64-bit system), where it is mapped; null past the end.
    ///
    /// replaces libc::_dyld_get_image_header
    pub fn _dyld_get_image_header(index: u32) -> *const c_void;

    /// `_dyld_get_image_vmaddr_slide` (mach-o/dyld.h): what ASLR added to image `index`'s
    /// addresses; 0 past the end. `blaze_sys::macos_sample::main_image_slide` (arm64) is
    /// image 0's, typed.
    ///
    /// replaces libc::_dyld_get_image_vmaddr_slide
    pub fn _dyld_get_image_vmaddr_slide(index: u32) -> isize;
}
