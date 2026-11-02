//! A stand-in toolchain over minicore's `core`: an `alloc` and a `std` whose functions the models
//! name have the pinned std's paths and signatures (`library/` of this tree), with bodies that
//! end in `extern "C"` functions as the real ones do, and build scripts on them.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin, all_crates};
use hir_expand::fixture;

/// The minicore regions the stand-ins use, then `alloc` and `std`. A test appends its crates.
pub(super) const TOOLCHAIN: &str = r#"
//- minicore: sized, copy, clone, drop, deref, fmt, result, option, panic, as_ref, from, try, fn, transmute, unsize, coerce_unsized, dispatch_from_dyn, builtin_impls, index, slice, str, borrow, asm, write
//- /alloc.rs crate:alloc
pub mod alloc {
    unsafe extern "Rust" {
        #[rustc_allocator]
        #[rustc_nounwind]
        #[rustc_std_internal_symbol]
        fn __rust_alloc(size: usize, align: usize) -> *mut u8;
        #[rustc_deallocator]
        #[rustc_nounwind]
        #[rustc_std_internal_symbol]
        fn __rust_dealloc(ptr: *mut u8, size: usize, align: usize);
        #[rustc_nounwind]
        #[rustc_std_internal_symbol]
        fn __rust_no_alloc_shim_is_unstable_v2();
        #[rustc_std_internal_symbol]
        fn __rust_alloc_error_handler(size: usize, align: usize) -> !;
    }

    pub unsafe fn alloc(size: usize, align: usize) -> *mut u8 {
        unsafe {
            __rust_no_alloc_shim_is_unstable_v2();
            __rust_alloc(size, align)
        }
    }

    pub unsafe fn dealloc(ptr: *mut u8, size: usize, align: usize) {
        unsafe { __rust_dealloc(ptr, size, align) }
    }

    pub fn handle_alloc_error(size: usize) -> ! {
        unsafe { __rust_alloc_error_handler(size, 1) }
    }
}

// Known by an attribute or a link name alone, not by a table's path.
pub mod shim {
    unsafe extern "Rust" {
        #[rustc_reallocator]
        pub fn grow_by_attribute(ptr: *mut u8, size: usize) -> *mut u8;
        #[link_name = "__rust_dealloc"]
        pub fn free_by_link_name(ptr: *mut u8, size: usize, align: usize);
        #[link_name = "rust_begin_unwind"]
        pub fn unwind_by_link_name() -> !;
    }
    unsafe extern "C" {
        pub fn not_a_shim(size: usize) -> *mut u8;
    }
}

pub mod vec {
    pub struct Vec<T> {
        ptr: *mut T,
        len: usize,
    }

    impl<T> Vec<T> {
        pub fn new() -> Vec<T> {
            Vec { ptr: unsafe { crate::alloc::alloc(0, 1) } as *mut T, len: 0 }
        }
    }

    impl<T> Drop for Vec<T> {
        fn drop(&mut self) {
            unsafe { crate::alloc::dealloc(self.ptr as *mut u8, self.len, 1) }
        }
    }
}

pub mod string {
    use core::fmt;
    use core::ops::Deref;

    pub struct String {
        ptr: *mut u8,
        len: usize,
    }

    impl String {
        pub fn as_str(&self) -> &str {
            self
        }
    }

    impl Deref for String {
        type Target = str;
        fn deref(&self) -> &str {
            ""
        }
    }

    impl From<&str> for String {
        fn from(text: &str) -> String {
            let _ = text;
            String { ptr: unsafe { crate::alloc::alloc(8, 1) }, len: 8 }
        }
    }

    impl Clone for String {
        fn clone(&self) -> String {
            String::from(self.as_str())
        }
    }

    impl Drop for String {
        fn drop(&mut self) {
            unsafe { crate::alloc::dealloc(self.ptr, self.len, 1) }
        }
    }

    impl AsRef<str> for String {
        fn as_ref(&self) -> &str {
            self
        }
    }

    impl AsRef<[u8]> for String {
        fn as_ref(&self) -> &[u8] {
            self.as_str().as_bytes()
        }
    }

    impl fmt::Display for String {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt::Display::fmt(self.as_str(), f)
        }
    }
}

pub mod borrow {
    pub trait ToOwned {
        type Owned;
        fn to_owned(&self) -> Self::Owned;
    }

    impl ToOwned for str {
        type Owned = crate::string::String;
        fn to_owned(&self) -> crate::string::String {
            crate::string::String::from(self)
        }
    }
}

// What core has and minicore leaves out.
pub mod str {
    impl AsRef<str> for str {
        fn as_ref(&self) -> &str {
            self
        }
    }

    impl AsRef<[u8]> for str {
        fn as_ref(&self) -> &[u8] {
            self.as_bytes()
        }
    }

    impl<T: ?Sized + AsRef<U>, U: ?Sized> AsRef<U> for &T {
        fn as_ref(&self) -> &U {
            <T as AsRef<U>>::as_ref(*self)
        }
    }
}

//- /std.rs crate:std deps:alloc
pub use alloc::{borrow, string, vec};
pub use core::format_args_nl;

pub mod prelude {
    pub mod rust_2015 {
        pub use core::prelude::v1::*;
        pub use crate::borrow::ToOwned;
        pub use crate::string::String;
        pub use crate::vec::Vec;
        pub use crate::{eprintln, println};
    }
    pub mod rust_2018 {
        pub use super::rust_2015::*;
    }
    pub mod rust_2021 {
        pub use super::rust_2015::*;
    }
    pub mod rust_2024 {
        pub use super::rust_2015::*;
    }
}

#[macro_export]
macro_rules! println {
    ($($arg:tt)*) => {{
        $crate::io::_print($crate::format_args_nl!($($arg)*));
    }};
}

#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {{
        $crate::io::_eprint($crate::format_args_nl!($($arg)*));
    }};
}

mod sys {
    unsafe extern "C" {
        pub fn getenv(name: *const u8) -> *const u8;
        pub fn open(path: *const u8, flags: i32) -> i32;
        pub fn read(fd: i32, buf: *mut u8, len: usize) -> isize;
        pub fn write(fd: i32, buf: *const u8, len: usize) -> isize;
        pub fn close(fd: i32) -> i32;
        pub fn mkdir(path: *const u8, mode: u32) -> i32;
        pub fn posix_spawnp(pid: *mut i32, file: *const u8) -> i32;
        pub fn exit(code: i32) -> !;
    }

    pub mod io {
        mod error {
            pub fn format_error(errno: i32) -> i32 {
                unsafe extern "C" {
                    #[link_name = "__xpg_strerror_r"]
                    fn strerror_r(errnum: i32, buf: *mut u8, buflen: usize) -> i32;
                }
                let mut buf = [0u8; 128];
                unsafe { strerror_r(errno, &mut buf as *mut [u8; 128] as *mut u8, 128) }
            }
        }
        pub use error::format_error;
    }
}

pub mod ffi {
    use crate::vec::Vec;

    #[repr(transparent)]
    pub struct OsStr {
        inner: [u8],
    }

    pub struct OsString {
        inner: Vec<u8>,
    }

    impl OsStr {
        pub fn to_str(&self) -> Option<&str> {
            None
        }

        pub(crate) fn as_ptr(&self) -> *const u8 {
            &self.inner as *const [u8] as *const u8
        }
    }

    impl OsString {
        pub fn new() -> OsString {
            OsString { inner: Vec::new() }
        }
    }

    impl AsRef<OsStr> for OsStr {
        fn as_ref(&self) -> &OsStr {
            self
        }
    }

    impl AsRef<OsStr> for OsString {
        fn as_ref(&self) -> &OsStr {
            "".as_ref()
        }
    }

    impl AsRef<OsStr> for str {
        fn as_ref(&self) -> &OsStr {
            unsafe { &*(self.as_bytes() as *const [u8] as *const OsStr) }
        }
    }

    impl AsRef<OsStr> for crate::string::String {
        fn as_ref(&self) -> &OsStr {
            self.as_str().as_ref()
        }
    }
}

pub mod path {
    use crate::ffi::{OsStr, OsString};
    use crate::string::String;

    #[repr(transparent)]
    pub struct Path {
        inner: OsStr,
    }

    pub struct PathBuf {
        inner: OsString,
    }

    impl Path {
        pub fn new<S: AsRef<OsStr> + ?Sized>(s: &S) -> &Path {
            unsafe { &*(s.as_ref() as *const OsStr as *const Path) }
        }

        pub fn join<P: AsRef<Path>>(&self, path: P) -> PathBuf {
            let mut buf = self.to_path_buf();
            buf.push(path);
            buf
        }

        pub fn to_path_buf(&self) -> PathBuf {
            PathBuf { inner: OsString::new() }
        }

        pub fn exists(&self) -> bool {
            unsafe { crate::sys::open(self.as_ptr(), 0) >= 0 }
        }

        pub(crate) fn as_ptr(&self) -> *const u8 {
            self.inner.as_ptr()
        }
    }

    impl PathBuf {
        pub fn new() -> PathBuf {
            PathBuf { inner: OsString::new() }
        }

        pub fn push<P: AsRef<Path>>(&mut self, path: P) {
            let _ = path.as_ref();
        }

        pub fn as_path(&self) -> &Path {
            self
        }
    }

    impl core::ops::Deref for PathBuf {
        type Target = Path;
        fn deref(&self) -> &Path {
            Path::new(&self.inner)
        }
    }

    impl AsRef<Path> for Path {
        fn as_ref(&self) -> &Path {
            self
        }
    }

    impl AsRef<Path> for PathBuf {
        fn as_ref(&self) -> &Path {
            self
        }
    }

    impl AsRef<Path> for str {
        fn as_ref(&self) -> &Path {
            Path::new(self)
        }
    }

    impl AsRef<Path> for String {
        fn as_ref(&self) -> &Path {
            Path::new(self.as_str())
        }
    }

    impl From<String> for PathBuf {
        fn from(text: String) -> PathBuf {
            let _ = text;
            PathBuf::new()
        }
    }
}

pub mod env {
    use core::fmt;
    use crate::ffi::{OsStr, OsString};
    use crate::string::String;

    pub enum VarError {
        NotPresent,
    }

    impl fmt::Debug for VarError {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            Ok(())
        }
    }

    pub fn var<K: AsRef<OsStr>>(key: K) -> Result<String, VarError> {
        match var_os(key) {
            Some(_) => Ok(String::from("")),
            None => Err(VarError::NotPresent),
        }
    }

    pub fn var_os<K: AsRef<OsStr>>(key: K) -> Option<OsString> {
        let value = unsafe { crate::sys::getenv(key.as_ref().as_ptr()) };
        let _ = value;
        None
    }
}

pub mod io {
    use core::fmt;

    pub struct Error {
        pub(crate) code: i32,
    }

    impl fmt::Debug for Error {
        fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
            let _ = crate::sys::io::format_error(self.code);
            Ok(())
        }
    }

    pub type Result<T> = core::result::Result<T, Error>;

    pub trait Write {
        fn write(&mut self, buf: &[u8]) -> Result<usize>;

        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            match self.write(buf) {
                Ok(_) => Ok(()),
                Err(error) => Err(error),
            }
        }

        fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<()> {
            let _ = args;
            self.write_all(&[])
        }
    }

    pub trait Read {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
    }

    pub struct Stdout {
        fd: i32,
    }

    pub struct Stderr {
        fd: i32,
    }

    pub fn stdout() -> Stdout {
        Stdout { fd: 1 }
    }

    pub fn stderr() -> Stderr {
        Stderr { fd: 2 }
    }

    fn write_fd(fd: i32, buf: &[u8]) -> Result<usize> {
        let n = unsafe { crate::sys::write(fd, buf as *const [u8] as *const u8, 0) };
        if n < 0 { Err(Error { code: 1 }) } else { Ok(0) }
    }

    impl Write for Stdout {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            write_fd(self.fd, buf)
        }
    }

    impl Write for Stderr {
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            write_fd(self.fd, buf)
        }
    }

    pub fn _print(args: fmt::Arguments<'_>) {
        let _ = stdout().write_fmt(args);
    }

    pub fn _eprint(args: fmt::Arguments<'_>) {
        let _ = stderr().write_fmt(args);
    }
}

pub mod os {
    pub mod fd {
        pub struct OwnedFd {
            pub(crate) fd: i32,
        }

        impl Drop for OwnedFd {
            fn drop(&mut self) {
                unsafe {
                    crate::sys::close(self.fd);
                }
            }
        }

        pub trait FromRawFd {
            unsafe fn from_raw_fd(fd: i32) -> Self;
        }

        impl FromRawFd for OwnedFd {
            unsafe fn from_raw_fd(fd: i32) -> OwnedFd {
                OwnedFd { fd }
            }
        }

        impl FromRawFd for crate::fs::File {
            unsafe fn from_raw_fd(fd: i32) -> crate::fs::File {
                crate::fs::File { inner: unsafe { OwnedFd::from_raw_fd(fd) } }
            }
        }
    }
}

pub mod fs {
    use crate::io;
    use crate::os::fd::OwnedFd;
    use crate::path::Path;
    use crate::string::String;
    use crate::vec::Vec;

    pub struct File {
        pub(crate) inner: OwnedFd,
    }

    fn open(path: &Path, flags: i32) -> io::Result<File> {
        let fd = unsafe { crate::sys::open(path.as_ptr(), flags) };
        if fd < 0 { Err(io::Error { code: fd }) } else { Ok(File { inner: OwnedFd { fd } }) }
    }

    impl File {
        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            open(path.as_ref(), 0)
        }

        pub fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
            open(path.as_ref(), 1)
        }
    }

    impl io::Read for File {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = unsafe { crate::sys::read(self.inner.fd, buf as *mut [u8] as *mut u8, 0) };
            if n < 0 { Err(io::Error { code: 1 }) } else { Ok(0) }
        }
    }

    impl io::Write for File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = unsafe { crate::sys::write(self.inner.fd, buf as *const [u8] as *const u8, 0) };
            if n < 0 { Err(io::Error { code: 1 }) } else { Ok(0) }
        }
    }

    pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        io::Read::read(&mut file, &mut [])?;
        Ok(Vec::new())
    }

    pub fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
        let mut file = File::open(path)?;
        io::Read::read(&mut file, &mut [])?;
        Ok(String::from(""))
    }

    pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
        let mut file = File::create(path)?;
        io::Write::write_all(&mut file, contents.as_ref())
    }

    pub fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
        let made = unsafe { crate::sys::mkdir(path.as_ref().as_ptr(), 0o755) };
        if made < 0 { Err(io::Error { code: made }) } else { Ok(()) }
    }
}

pub mod process {
    use crate::ffi::OsStr;
    use crate::io;

    pub struct Command {
        program: *const u8,
    }

    pub struct Output {
        pub status: i32,
    }

    impl Command {
        pub fn new<S: AsRef<OsStr>>(program: S) -> Command {
            Command { program: program.as_ref().as_ptr() }
        }

        pub fn output(&mut self) -> io::Result<Output> {
            let mut pid = 0;
            let status = unsafe { crate::sys::posix_spawnp(&mut pid, self.program) };
            Ok(Output { status })
        }
    }

    pub fn exit(code: i32) -> ! {
        unsafe { crate::sys::exit(code) }
    }
}
"#;

/// The database of the toolchain with `crates` appended (each starting `//- /path crate:..`).
pub(super) fn db_with(crates: &str) -> Db {
    // alloc: a test's fixture text
    fixture::db(&format!("{TOOLCHAIN}{crates}")).into_db()
}

/// The crate named `name` in `db`.
#[track_caller]
pub(super) fn crate_named(db: &Db, name: &str) -> Crate {
    all_crates(db)
        .iter()
        .copied()
        .find(|krate| {
            krate.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}` in the fixture"))
}

/// The toolchain crate of `lang` in `db`.
#[track_caller]
pub(super) fn lang_crate(db: &Db, lang: LangCrateOrigin) -> Crate {
    all_crates(db)
        .iter()
        .copied()
        .find(|krate| krate.data(db).origin == CrateOrigin::Lang(lang))
        .unwrap_or_else(|| panic!("no {lang:?} crate in the fixture"))
}
