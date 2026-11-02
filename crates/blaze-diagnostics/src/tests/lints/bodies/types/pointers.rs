//! The pointer passes (`DerefNullPtr`, `InvalidReferenceCasting`, `ImplicitAutorefs`,
//! `PtrNullChecks`, `DanglingPointers`): rustc's UI tests in `ui/` (rust-lang/rust
//! `ad3d0bc141a`, MIT or Apache-2.0). `lint-deref-nullptr.rs`, `ptr_null_checks.rs`,
//! `invalid_null_args.rs`, `dangling-pointers-from-locals.rs` and
//! `dangling-pointers-from-temporaries/types.rs` are whole, byte for byte with their `.stderr`.
//!
//! `reference_casting.rs` and `implicit_autorefs.rs` are not here: the first uses `println!`
//! and `Box<dyn Send>`, the second's notes point into std (`$SRC_DIR/core/src/slice/mod.rs`),
//! which a std written in the test can't give.

use super::{Ui, ui};
use crate::tests::{assert_reports, error, warning};

const FLAGS: &str = "sized, copy, clone, derive, option, result, fn, from, add, default, deref, \
                     manually_drop, phantom_data, index, slice, range, unsize, coerce_unsized, \
                     addr_of, todo";

const PRELUDE: &str = "pub use crate::{boxed::Box, string::String, vec::Vec};
mod ambiguous_macros_only {
    mod vec {}
    pub use crate::*;
}
pub use self::ambiguous_macros_only::vec;";

/// The items of std (alloc's and core's, as std re-exports them) the tests use, as std writes
/// them: their signatures, `#[lang]`s, `#[rustc_diagnostic_item]`s and the attributes the passes
/// read (`#[rustc_as_ptr]`, `#[rustc_never_returns_null_ptr]`). The bodies are not std's. Two
/// signatures are narrower: `Box` and `Vec` take no allocator, and `ptr::null` and
/// `ptr::null_mut` take only sized `T`.
const STD: &str = r##"
pub mod intrinsics {
    #[rustc_intrinsic]
    #[rustc_diagnostic_item = "transmute"]
    pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
}

pub mod marker {
    pub use core::marker::PhantomData;
}

pub mod ops {
    pub use core::ops::Deref;
}

pub mod mem {
    pub use core::mem::ManuallyDrop;
    pub use crate::intrinsics::transmute;

    #[lang = "maybe_uninit"]
    #[repr(transparent)]
    pub union MaybeUninit<T> {
        uninit: (),
        value: ManuallyDrop<T>,
    }

    impl<T> MaybeUninit<T> {
        #[rustc_as_ptr]
        pub const fn as_ptr(&self) -> *const T {
            loop {}
        }
    }
}

pub mod ptr {
    use core::option::Option;

    pub use core::ptr::{addr_of, addr_of_mut};

    #[rustc_diagnostic_item = "ptr_null"]
    pub const fn null<T>() -> *const T {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_null_mut"]
    pub const fn null_mut<T>() -> *mut T {
        loop {}
    }

    #[rustc_never_returns_null_ptr]
    #[rustc_diagnostic_item = "ptr_from_ref"]
    pub const fn from_ref<T: ?Sized>(r: &T) -> *const T {
        r
    }

    #[rustc_never_returns_null_ptr]
    pub const fn from_mut<T: ?Sized>(r: &mut T) -> *mut T {
        r
    }

    #[rustc_diagnostic_item = "ptr_read"]
    pub const unsafe fn read<T>(src: *const T) -> T {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_read_unaligned"]
    pub const unsafe fn read_unaligned<T>(src: *const T) -> T {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_read_volatile"]
    pub const unsafe fn read_volatile<T>(src: *const T) -> T {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_write"]
    pub const unsafe fn write<T>(dst: *mut T, src: T) {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_write_unaligned"]
    pub const unsafe fn write_unaligned<T>(dst: *mut T, src: T) {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_write_volatile"]
    pub const unsafe fn write_volatile<T>(dst: *mut T, src: T) {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_write_bytes"]
    pub const unsafe fn write_bytes<T>(dst: *mut T, val: u8, count: usize) {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_replace"]
    pub const unsafe fn replace<T>(dst: *mut T, src: T) -> T {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_copy"]
    pub const unsafe fn copy<T>(src: *const T, dst: *mut T, count: usize) {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_copy_nonoverlapping"]
    pub const unsafe fn copy_nonoverlapping<T>(src: *const T, dst: *mut T, count: usize) {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_swap"]
    pub const unsafe fn swap<T>(x: *mut T, y: *mut T) {
        loop {}
    }

    #[rustc_diagnostic_item = "ptr_swap_nonoverlapping"]
    pub const unsafe fn swap_nonoverlapping<T>(x: *mut T, y: *mut T, count: usize) {
        loop {}
    }

    #[rustc_diagnostic_item = "NonNull"]
    #[repr(transparent)]
    pub struct NonNull<T: ?Sized> {
        pointer: *const T,
    }

    impl<T> NonNull<T> {
        pub const fn dangling() -> Self {
            loop {}
        }
    }

    impl<T: ?Sized> NonNull<T> {
        pub const fn new(ptr: *mut T) -> Option<Self> {
            loop {}
        }

        #[rustc_never_returns_null_ptr]
        pub const fn as_ptr(self) -> *mut T {
            loop {}
        }
    }

    impl<T: ?Sized> *const T {
        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "ptr_const_is_null"]
        pub const fn is_null(self) -> bool {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "const_ptr_cast"]
        pub const fn cast<U>(self) -> *const U {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "ptr_cast_mut"]
        pub const fn cast_mut(self) -> *mut T {
            loop {}
        }
    }

    impl<T: ?Sized> *mut T {
        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "ptr_is_null"]
        pub const fn is_null(self) -> bool {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "ptr_cast"]
        pub const fn cast<U>(self) -> *mut U {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        #[rustc_diagnostic_item = "ptr_cast_const"]
        pub const fn cast_const(self) -> *const T {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        pub const unsafe fn write(self, val: T)
        where
            T: Sized,
        {
            loop {}
        }
    }
}

pub mod slice {
    use crate::boxed::Box;
    use crate::vec::Vec;

    impl<T> [T] {
        #[rustc_allow_incoherent_impl]
        #[rustc_never_returns_null_ptr]
        #[rustc_as_ptr]
        pub const fn as_ptr(&self) -> *const T {
            loop {}
        }

        #[rustc_allow_incoherent_impl]
        pub fn into_vec(self: Box<Self>) -> Vec<T> {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "slice_from_raw_parts"]
    pub const unsafe fn from_raw_parts<'a, T>(data: *const T, len: usize) -> &'a [T] {
        loop {}
    }

    #[rustc_diagnostic_item = "slice_from_raw_parts_mut"]
    pub const unsafe fn from_raw_parts_mut<'a, T>(data: *mut T, len: usize) -> &'a mut [T] {
        loop {}
    }
}

pub mod str {
    impl str {
        #[rustc_allow_incoherent_impl]
        #[rustc_never_returns_null_ptr]
        #[rustc_as_ptr]
        pub const fn as_ptr(&self) -> *const u8 {
            loop {}
        }
    }
}

pub mod cell {
    #[lang = "unsafe_cell"]
    #[repr(transparent)]
    pub struct UnsafeCell<T: ?Sized> {
        value: T,
    }

    impl<T> UnsafeCell<T> {
        pub const fn new(value: T) -> UnsafeCell<T> {
            UnsafeCell { value }
        }
    }

    impl<T: ?Sized> UnsafeCell<T> {
        #[rustc_as_ptr]
        #[rustc_never_returns_null_ptr]
        pub const fn get(&self) -> *mut T {
            loop {}
        }

        #[rustc_diagnostic_item = "unsafe_cell_raw_get"]
        pub const fn raw_get(this: *const Self) -> *mut T {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "Cell"]
    #[repr(transparent)]
    pub struct Cell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    impl<T: ?Sized> Cell<T> {
        #[rustc_as_ptr]
        #[rustc_never_returns_null_ptr]
        pub const fn as_ptr(&self) -> *mut T {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "SyncUnsafeCell"]
    #[repr(transparent)]
    pub struct SyncUnsafeCell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    impl<T: ?Sized> SyncUnsafeCell<T> {
        #[rustc_as_ptr]
        #[rustc_never_returns_null_ptr]
        pub const fn get(&self) -> *mut T {
            loop {}
        }
    }
}

pub mod boxed {
    use core::marker::Unsize;
    use core::ops::{CoerceUnsized, Deref};

    #[lang = "owned_box"]
    pub struct Box<T: ?Sized>(*const T);

    impl<T: ?Sized> Box<T> {
        #[rustc_never_returns_null_ptr]
        #[rustc_as_ptr]
        pub fn as_ptr(b: &Self) -> *const T {
            loop {}
        }
    }

    impl<T: ?Sized> Deref for Box<T> {
        type Target = T;

        fn deref(&self) -> &T {
            loop {}
        }
    }

    impl<T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Box<U>> for Box<T> {}

    #[rustc_intrinsic]
    pub fn box_new<T>(x: T) -> Box<T>;
}

pub mod vec {
    use core::ops::Deref;

    #[rustc_diagnostic_item = "Vec"]
    pub struct Vec<T> {
        ptr: *mut T,
        cap: usize,
        len: usize,
    }

    impl<T> Vec<T> {
        #[rustc_never_returns_null_ptr]
        #[rustc_as_ptr]
        pub const fn as_ptr(&self) -> *const T {
            loop {}
        }
    }

    impl<T> Deref for Vec<T> {
        type Target = [T];

        fn deref(&self) -> &[T] {
            loop {}
        }
    }
}

#[macro_export]
#[rustc_diagnostic_item = "vec_macro"]
macro_rules! vec {
    ($($x:expr),+ $(,)?) => (
        <[_]>::into_vec(
            $crate::boxed::box_new([$($x),+])
        )
    );
}

pub mod string {
    use core::convert::From;
    use core::ops::{AddAssign, Deref};
    use crate::vec::Vec;

    #[lang = "String"]
    pub struct String {
        vec: Vec<u8>,
    }

    impl Deref for String {
        type Target = str;

        fn deref(&self) -> &str {
            loop {}
        }
    }

    impl From<&str> for String {
        fn from(s: &str) -> String {
            loop {}
        }
    }

    impl AddAssign<&str> for String {
        fn add_assign(&mut self, other: &str) {
            loop {}
        }
    }
}

pub mod ffi {
    use core::ops::Deref;
    use crate::boxed::Box;

    pub type c_char = i8;

    #[lang = "CStr"]
    #[repr(transparent)]
    pub struct CStr {
        inner: [c_char],
    }

    impl CStr {
        #[rustc_as_ptr]
        #[rustc_never_returns_null_ptr]
        pub const fn as_ptr(&self) -> *const c_char {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "cstring_type"]
    pub struct CString {
        inner: Box<[u8]>,
    }

    impl Deref for CString {
        type Target = CStr;

        fn deref(&self) -> &CStr {
            loop {}
        }
    }
}
"##;

fn check(name: &str, source: &str, stderr: &str) {
    let actual = ui(Ui { name, core: FLAGS, std: Some((PRELUDE, STD)), source, ..Ui::default() });
    assert_eq!(actual, stderr, "{name}");
}

#[test]
fn lint_deref_nullptr() {
    check(
        "lint-deref-nullptr.rs",
        include_str!("pointers/ui/lint-deref-nullptr.rs"),
        include_str!("pointers/ui/lint-deref-nullptr.stderr"),
    );
}

#[test]
fn ptr_null_checks() {
    let source = include_str!("pointers/ui/ptr_null_checks.rs");
    let actual = ui(Ui { name: "ptr_null_checks.rs", core: FLAGS, std: Some((PRELUDE, STD)), source, ..Ui::default() });
    // A comparison of a pointer made from a reference or a function with null, each its line;
    // one made through `ptr::from_ref` and the like names that function.
    let plain = NULL_CHECKS.map(|line| warning(line, &[]));
    let named = NULL_CHECKS_THROUGH.map(|(line, through)| warning(line, through));
    assert_reports(&actual, &[&plain[..], &named[..]].concat());
}

/// The lines of `ptr_null_checks.rs` with a null check of a pointer that can't be null.
const NULL_CHECKS: [usize; 21] = [12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 36, 40, 48, 51, 53, 55, 57, 59, 61, 63];

/// The lines of `ptr_null_checks.rs` whose pointer is made by the function named.
const NULL_CHECKS_THROUGH: [(usize, &[&str]); 6] = [
    (38, &["`from_mut`"]),
    (42, &["`from_ref`"]),
    (44, &["`from_ref`"]),
    (46, &["`from_ref`"]),
    (67, &["`as_ptr`"]),
    (69, &["`as_ptr`"]),
];

#[test]
fn invalid_null_args() {
    check(
        "invalid_null_args.rs",
        include_str!("pointers/ui/invalid_null_args.rs"),
        include_str!("pointers/ui/invalid_null_args.stderr"),
    );
}

#[test]
fn dangling_pointers_from_locals() {
    check(
        "dangling-pointers-from-locals.rs",
        include_str!("pointers/ui/dangling-pointers-from-locals.rs"),
        include_str!("pointers/ui/dangling-pointers-from-locals.stderr"),
    );
}

#[test]
fn dangling_pointers_from_temporaries_types() {
    let source = include_str!("pointers/ui/dangling-pointers-from-temporaries/types.rs");
    let library = &["sync_unsafe_cell"];
    let actual = ui(Ui { name: "types.rs", core: FLAGS, std: Some((PRELUDE, STD)), library, source, ..Ui::default() });
    assert_reports(
        &actual,
        &[
            error(21, &["`CString`"]),
            error(23, &["`String`"]),
            error(25, &["`Vec<u8>`"]),
            error(27, &["`Box<CString>`"]),
            error(29, &["`Box<[u8]>`"]),
            error(31, &["`Box<str>`"]),
            error(33, &["`Box<CStr>`"]),
            error(35, &["`[u8; 10]`"]),
            error(37, &["`Box<[u8; 10]>`"]),
            error(39, &["`Box<Vec<u8>>`"]),
            error(41, &["`Box<String>`"]),
            error(43, &["`Box<Box<Box<Box<[u8]>>>>`"]),
            error(45, &["`Cell<u8>`"]),
            error(47, &["`MaybeUninit<u8>`"]),
            error(49, &["`Vec<AsPtrFake>`"]),
            error(51, &["`UnsafeCell<u8>`"]),
            error(53, &["`SyncUnsafeCell<u8>`"]),
        ],
    );
}
