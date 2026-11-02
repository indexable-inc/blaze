//! `InvalidValue`, `EnumIntrinsicsNonEnums`, `MutableTransmutes`, `CheckTransmutes` and
//! `InteriorMutableConsts`: rustc's UI tests of `invalid_value`, `enum_intrinsics_non_enums`,
//! `mutable_transmutes`, `ptr_to_integer_transmute_in_consts`, `unnecessary_transmutes`,
//! `integer_to_ptr_transmutes` and `const_item_interior_mutations`, in `values/ui/` byte for byte
//! as rust-lang/rust `ad3d0bc141a` has them in `tests/ui/` (MIT or Apache-2.0), with the items
//! of std they use written as `library/` writes them.

use super::{Ui, ui};
use crate::tests::{assert_reports, error};

fn check(name: &str, core: &str, std: &str, source: &str, stderr: &str) {
    let actual = ui(Ui { name, core, std: Some(("", std)), source, ..Ui::default() });
    assert_eq!(actual, stderr, "{name}");
}

/// `core::intrinsics::transmute`, as `std::mem` re-exports it.
const TRANSMUTE: &str = r#"
pub mod intrinsics {
    #[rustc_intrinsic]
    #[rustc_diagnostic_item = "transmute"]
    pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
}
pub mod mem {
    pub use crate::intrinsics::transmute;
}
"#;

const DISCRIMINANT: &str = r#"
pub mod mem {
    pub use core::mem::Discriminant;

    #[rustc_diagnostic_item = "mem_discriminant"]
    pub const fn discriminant<T>(v: &T) -> Discriminant<T> {
        loop {}
    }

    #[rustc_diagnostic_item = "mem_variant_count"]
    pub const fn variant_count<T>() -> usize {
        loop {}
    }
}
"#;

const UNINIT: &str = r#"
pub mod intrinsics {
    #[rustc_intrinsic]
    #[rustc_diagnostic_item = "transmute"]
    pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
}
pub mod mem {
    pub use crate::intrinsics::transmute;
    use core::mem::ManuallyDrop;

    #[deprecated(since = "1.39.0", note = "use `mem::MaybeUninit` instead")]
    #[rustc_diagnostic_item = "mem_uninitialized"]
    pub unsafe fn uninitialized<T>() -> T {
        loop {}
    }

    #[rustc_diagnostic_item = "mem_zeroed"]
    pub const unsafe fn zeroed<T>() -> T {
        loop {}
    }

    #[lang = "maybe_uninit"]
    #[rustc_diagnostic_item = "MaybeUninit"]
    #[repr(transparent)]
    pub union MaybeUninit<T> {
        uninit: (),
        value: ManuallyDrop<T>,
    }

    impl<T> MaybeUninit<T> {
        #[rustc_diagnostic_item = "maybe_uninit_uninit"]
        pub const fn uninit() -> MaybeUninit<T> {
            MaybeUninit { uninit: () }
        }

        #[rustc_diagnostic_item = "maybe_uninit_zeroed"]
        pub const fn zeroed() -> MaybeUninit<T> {
            loop {}
        }

        #[rustc_diagnostic_item = "assume_init"]
        pub const unsafe fn assume_init(self) -> T {
            loop {}
        }
    }
}
pub mod ptr {
    pub use core::ptr::NonNull;
}
pub mod num {
    pub use core::num::NonZero;
}
pub mod pat {
    pub use core::pattern_type;
}
"#;

const CELLS: &str = r#"
pub mod ops {
    pub use core::ops::Deref;
}
pub mod cell {
    use core::option::Option::{self, None};
    use core::result::Result;

    #[lang = "unsafe_cell"]
    #[repr(transparent)]
    pub struct UnsafeCell<T: ?Sized> {
        value: T,
    }

    impl<T> UnsafeCell<T> {
        pub const fn new(value: T) -> UnsafeCell<T> {
            UnsafeCell { value }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const unsafe fn replace(&self, value: T) -> T {
            loop {}
        }
    }

    impl<T: ?Sized> UnsafeCell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn get(&self) -> *mut T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const unsafe fn as_ref_unchecked(&self) -> &T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const unsafe fn as_mut_unchecked(&self) -> &mut T {
            loop {}
        }
    }

    #[repr(transparent)]
    #[rustc_diagnostic_item = "SyncUnsafeCell"]
    pub struct SyncUnsafeCell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    impl<T> SyncUnsafeCell<T> {
        pub const fn new(value: T) -> Self {
            SyncUnsafeCell { value: UnsafeCell::new(value) }
        }
    }

    impl<T: ?Sized> SyncUnsafeCell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn get(&self) -> *mut T {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "Cell"]
    #[repr(transparent)]
    pub struct Cell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    impl<T> Cell<T> {
        pub const fn new(value: T) -> Cell<T> {
            Cell { value: UnsafeCell::new(value) }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn set(&self, val: T) {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn swap(&self, other: &Self) {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn replace(&self, val: T) -> T {
            loop {}
        }
    }

    impl<T: Copy> Cell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn get(&self) -> T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn update(&self, f: impl FnOnce(T) -> T) {
            loop {}
        }
    }

    #[rustc_diagnostic_item = "RefCell"]
    pub struct RefCell<T: ?Sized> {
        value: UnsafeCell<T>,
    }

    #[non_exhaustive]
    pub struct BorrowError {}

    #[non_exhaustive]
    pub struct BorrowMutError {}

    #[rustc_diagnostic_item = "RefCellRef"]
    pub struct Ref<'b, T: ?Sized + 'b> {
        value: &'b T,
    }

    #[rustc_diagnostic_item = "RefCellRefMut"]
    pub struct RefMut<'b, T: ?Sized + 'b> {
        value: &'b mut T,
    }

    impl<T> RefCell<T> {
        pub const fn new(value: T) -> RefCell<T> {
            RefCell { value: UnsafeCell::new(value) }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn replace(&self, t: T) -> T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn replace_with<F: FnOnce(&mut T) -> T>(&self, f: F) -> T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn swap(&self, other: &Self) {
            loop {}
        }
    }

    impl<T: ?Sized> RefCell<T> {
        #[rustc_should_not_be_called_on_const_items]
        pub const fn borrow(&self) -> Ref<'_, T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn try_borrow(&self) -> Result<Ref<'_, T>, BorrowError> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn borrow_mut(&self) -> RefMut<'_, T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub const fn try_borrow_mut(&self) -> Result<RefMut<'_, T>, BorrowMutError> {
            loop {}
        }
    }

    pub struct OnceCell<T> {
        inner: UnsafeCell<Option<T>>,
    }

    impl<T> OnceCell<T> {
        pub const fn new() -> OnceCell<T> {
            OnceCell { inner: UnsafeCell::new(None) }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn set(&self, value: T) -> Result<(), T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn try_insert(&self, value: T) -> Result<&T, (&T, T)> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn get_or_init<F>(&self, f: F) -> &T
        where
            F: FnOnce() -> T,
        {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn get_or_try_init<F, E>(&self, f: F) -> Result<&T, E>
        where
            F: FnOnce() -> Result<T, E>,
        {
            loop {}
        }
    }

    pub struct LazyCell<T, F = fn() -> T> {
        state: UnsafeCell<Option<T>>,
        init: F,
    }

    impl<T, F: FnOnce() -> T> LazyCell<T, F> {
        pub const fn new(f: F) -> LazyCell<T, F> {
            LazyCell { state: UnsafeCell::new(None), init: f }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn force(this: &LazyCell<T, F>) -> &T {
            loop {}
        }
    }
}
"#;

/// std's locks of `library/std/src/sync/`, with `nonpoison`'s `Mutex`, `RwLock` and `Condvar`
/// and core's `iter::Once` beside them, which make rustc print `std::sync::Mutex<i32>` and
/// `std::sync::Once` with no trimmed path; core's `time::Duration`, which minicore lacks.
const SYNC: &str = r#"
pub mod iter {
    pub use core::iter::*;

    #[rustc_diagnostic_item = "IterOnce"]
    pub struct Once<T> {
        inner: core::option::Option<T>,
    }

    pub fn once<T>(value: T) -> Once<T> {
        Once { inner: core::option::Option::Some(value) }
    }
}
pub mod time {
    #[rustc_diagnostic_item = "Duration"]
    pub struct Duration {
        secs: u64,
        nanos: u32,
    }

    impl Duration {
        pub const fn from_millis(millis: u64) -> Duration {
            Duration { secs: millis / 1000, nanos: 0 }
        }
    }
}
pub mod sync {
    use core::option::Option;
    use core::result::Result;

    pub use self::poison::{
        Condvar, LockResult, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
        TryLockError, TryLockResult, WaitTimeoutResult,
    };

    pub mod poison {
        use core::fmt;
        use core::result::Result;
        use crate::time::Duration;

        pub struct PoisonError<T> {
            data: T,
        }

        impl<T> fmt::Debug for PoisonError<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                loop {}
            }
        }

        pub enum TryLockError<T> {
            Poisoned(PoisonError<T>),
            WouldBlock,
        }

        pub type LockResult<T> = Result<T, PoisonError<T>>;
        pub type TryLockResult<Guard> = Result<Guard, TryLockError<Guard>>;

        #[rustc_diagnostic_item = "Mutex"]
        pub struct Mutex<T: ?Sized> {
            poison: bool,
            data: T,
        }

        #[rustc_diagnostic_item = "MutexGuard"]
        pub struct MutexGuard<'a, T: ?Sized + 'a> {
            lock: &'a Mutex<T>,
        }

        impl<T> Mutex<T> {
            pub const fn new(t: T) -> Mutex<T> {
                Mutex { poison: false, data: t }
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn set(&self, value: T) -> Result<(), PoisonError<T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn replace(&self, value: T) -> LockResult<T> {
                loop {}
            }
        }

        impl<T: ?Sized> Mutex<T> {
            #[rustc_should_not_be_called_on_const_items]
            pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn clear_poison(&self) {
                loop {}
            }
        }

        #[rustc_diagnostic_item = "RwLock"]
        pub struct RwLock<T: ?Sized> {
            poison: bool,
            data: T,
        }

        #[rustc_diagnostic_item = "RwLockReadGuard"]
        pub struct RwLockReadGuard<'rwlock, T: ?Sized + 'rwlock> {
            data: &'rwlock T,
        }

        #[rustc_diagnostic_item = "RwLockWriteGuard"]
        pub struct RwLockWriteGuard<'rwlock, T: ?Sized + 'rwlock> {
            lock: &'rwlock RwLock<T>,
        }

        impl<T> RwLock<T> {
            pub const fn new(t: T) -> RwLock<T> {
                RwLock { poison: false, data: t }
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn set(&self, value: T) -> Result<(), PoisonError<T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn replace(&self, value: T) -> LockResult<T> {
                loop {}
            }
        }

        impl<T: ?Sized> RwLock<T> {
            #[rustc_should_not_be_called_on_const_items]
            pub fn read(&self) -> LockResult<RwLockReadGuard<'_, T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn try_read(&self) -> TryLockResult<RwLockReadGuard<'_, T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn try_write(&self) -> TryLockResult<RwLockWriteGuard<'_, T>> {
                loop {}
            }
        }

        pub struct WaitTimeoutResult(bool);

        pub struct Condvar {
            inner: u32,
        }

        impl Condvar {
            pub const fn new() -> Condvar {
                Condvar { inner: 0 }
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn wait_while<'a, T, F>(
                &self,
                mut guard: MutexGuard<'a, T>,
                mut condition: F,
            ) -> LockResult<MutexGuard<'a, T>>
            where
                F: FnMut(&mut T) -> bool,
            {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            #[deprecated(since = "1.6.0", note = "replaced by `std::sync::Condvar::wait_timeout`")]
            pub fn wait_timeout_ms<'a, T>(
                &self,
                guard: MutexGuard<'a, T>,
                ms: u32,
            ) -> LockResult<(MutexGuard<'a, T>, bool)> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn wait_timeout<'a, T>(
                &self,
                guard: MutexGuard<'a, T>,
                dur: Duration,
            ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn wait_timeout_while<'a, T, F>(
                &self,
                mut guard: MutexGuard<'a, T>,
                dur: Duration,
                mut condition: F,
            ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)>
            where
                F: FnMut(&mut T) -> bool,
            {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn notify_one(&self) {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn notify_all(&self) {
                loop {}
            }
        }
    }

    pub mod nonpoison {
        pub struct WouldBlock;

        pub type TryLockResult<Guard> = core::result::Result<Guard, WouldBlock>;

        #[rustc_diagnostic_item = "NonPoisonMutex"]
        pub struct Mutex<T: ?Sized> {
            data: T,
        }

        impl<T> Mutex<T> {
            pub const fn new(t: T) -> Mutex<T> {
                Mutex { data: t }
            }
        }

        #[rustc_diagnostic_item = "NonPoisonRwLock"]
        pub struct RwLock<T: ?Sized> {
            data: T,
        }

        impl<T> RwLock<T> {
            pub const fn new(t: T) -> RwLock<T> {
                RwLock { data: t }
            }
        }

        pub struct Condvar {
            inner: u32,
        }

        impl Condvar {
            pub const fn new() -> Condvar {
                Condvar { inner: 0 }
            }
        }
    }

    pub struct Once {
        inner: u32,
    }

    pub struct OnceState {
        poisoned: bool,
    }

    impl Once {
        pub const fn new() -> Once {
            Once { inner: 0 }
        }

        #[track_caller]
        #[rustc_should_not_be_called_on_const_items]
        pub fn call_once<F>(&self, f: F)
        where
            F: FnOnce(),
        {
            loop {}
        }

        #[track_caller]
        #[rustc_should_not_be_called_on_const_items]
        pub fn call_once_force<F>(&self, f: F)
        where
            F: FnOnce(&OnceState),
        {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn wait(&self) {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn wait_force(&self) {
            loop {}
        }
    }

    pub struct LazyLock<T, F = fn() -> T> {
        once: Once,
        data: Option<T>,
        init: F,
    }

    impl<T, F: FnOnce() -> T> LazyLock<T, F> {
        pub const fn new(f: F) -> LazyLock<T, F> {
            LazyLock { once: Once::new(), data: Option::None, init: f }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn force(this: &LazyLock<T, F>) -> &T {
            loop {}
        }
    }

    impl<T, F> LazyLock<T, F> {
        #[rustc_should_not_be_called_on_const_items]
        pub fn get(this: &LazyLock<T, F>) -> Option<&T> {
            loop {}
        }
    }

    pub struct OnceLock<T> {
        once: Once,
        value: Option<T>,
    }

    impl<T> OnceLock<T> {
        pub const fn new() -> OnceLock<T> {
            OnceLock { once: Once::new(), value: Option::None }
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn get(&self) -> Option<&T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn wait(&self) -> &T {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn set(&self, value: T) -> Result<(), T> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn try_insert(&self, value: T) -> Result<&T, (&T, T)> {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn get_or_init<F>(&self, f: F) -> &T
        where
            F: FnOnce() -> T,
        {
            loop {}
        }

        #[rustc_should_not_be_called_on_const_items]
        pub fn get_or_try_init<F, E>(&self, f: F) -> Result<&T, E>
        where
            F: FnOnce() -> Result<T, E>,
        {
            loop {}
        }
    }
}
"#;

/// core's `sync::atomic` (`library/core/src/sync/atomic.rs`), which minicore lacks, as std
/// re-exports it, with core's `ptr::null_mut`.
const ATOMICS: &str = r#"
pub mod ptr {
    pub use core::ptr::*;

    #[rustc_promotable]
    #[rustc_diagnostic_item = "ptr_null_mut"]
    pub const fn null_mut<T>() -> *mut T {
        0 as *mut T
    }
}
pub mod sync {
    pub mod atomic {
        use core::option::Option;
        use core::result::Result;

        mod private {
            #[repr(C, align(1))]
            pub struct Align1<T>(T);
            #[repr(C, align(4))]
            pub struct Align4<T>(T);
            #[repr(C, align(8))]
            pub struct Align8<T>(T);
        }

        pub unsafe trait AtomicPrimitive: Sized + Copy {
            type Storage: Sized;
        }

        unsafe impl AtomicPrimitive for bool {
            type Storage = private::Align1<u8>;
        }

        unsafe impl AtomicPrimitive for u32 {
            type Storage = private::Align4<u32>;
        }

        unsafe impl<T> AtomicPrimitive for *mut T {
            type Storage = private::Align8<*mut T>;
        }

        #[repr(C)]
        #[rustc_diagnostic_item = "Atomic"]
        pub struct Atomic<T: AtomicPrimitive> {
            v: T::Storage,
        }

        pub type AtomicBool = Atomic<bool>;
        pub type AtomicU32 = Atomic<u32>;
        pub type AtomicPtr<T> = Atomic<*mut T>;

        #[rustc_diagnostic_item = "Ordering"]
        pub enum Ordering {
            Relaxed,
            Release,
            Acquire,
            AcqRel,
            SeqCst,
        }

        impl AtomicBool {
            pub const fn new(v: bool) -> AtomicBool {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn store(&self, val: bool, order: Ordering) {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn swap(&self, val: bool, order: Ordering) -> bool {
                loop {}
            }

            #[deprecated(since = "1.50.0", note = "Use `compare_exchange` or `compare_exchange_weak` instead")]
            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_and_swap(&self, current: bool, new: bool, order: Ordering) -> bool {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_exchange(
                &self,
                current: bool,
                new: bool,
                success: Ordering,
                failure: Ordering,
            ) -> Result<bool, bool> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_exchange_weak(
                &self,
                current: bool,
                new: bool,
                success: Ordering,
                failure: Ordering,
            ) -> Result<bool, bool> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_and(&self, val: bool, order: Ordering) -> bool {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_nand(&self, val: bool, order: Ordering) -> bool {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_or(&self, val: bool, order: Ordering) -> bool {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_xor(&self, val: bool, order: Ordering) -> bool {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_not(&self, order: Ordering) -> bool {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_update<F>(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                f: F,
            ) -> Result<bool, bool>
            where
                F: FnMut(bool) -> Option<bool>,
            {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn try_update(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                mut f: impl FnMut(bool) -> Option<bool>,
            ) -> Result<bool, bool> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn update(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                mut f: impl FnMut(bool) -> bool,
            ) -> bool {
                loop {}
            }
        }

        impl<T> AtomicPtr<T> {
            pub const fn new(p: *mut T) -> AtomicPtr<T> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn store(&self, ptr: *mut T, order: Ordering) {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn swap(&self, ptr: *mut T, order: Ordering) -> *mut T {
                loop {}
            }

            #[deprecated(since = "1.50.0", note = "Use `compare_exchange` or `compare_exchange_weak` instead")]
            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_and_swap(&self, current: *mut T, new: *mut T, order: Ordering) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_exchange(
                &self,
                current: *mut T,
                new: *mut T,
                success: Ordering,
                failure: Ordering,
            ) -> Result<*mut T, *mut T> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_exchange_weak(
                &self,
                current: *mut T,
                new: *mut T,
                success: Ordering,
                failure: Ordering,
            ) -> Result<*mut T, *mut T> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_update<F>(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                f: F,
            ) -> Result<*mut T, *mut T>
            where
                F: FnMut(*mut T) -> Option<*mut T>,
            {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn try_update(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                mut f: impl FnMut(*mut T) -> Option<*mut T>,
            ) -> Result<*mut T, *mut T> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn update(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                mut f: impl FnMut(*mut T) -> *mut T,
            ) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_ptr_add(&self, val: usize, order: Ordering) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_ptr_sub(&self, val: usize, order: Ordering) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_byte_add(&self, val: usize, order: Ordering) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_byte_sub(&self, val: usize, order: Ordering) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_or(&self, val: usize, order: Ordering) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_and(&self, val: usize, order: Ordering) -> *mut T {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_xor(&self, val: usize, order: Ordering) -> *mut T {
                loop {}
            }
        }

        impl AtomicU32 {
            pub const fn new(v: u32) -> Self {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn store(&self, val: u32, order: Ordering) {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn swap(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[deprecated(since = "1.50.0", note = "Use `compare_exchange` or `compare_exchange_weak` instead")]
            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_and_swap(&self, current: u32, new: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_exchange(
                &self,
                current: u32,
                new: u32,
                success: Ordering,
                failure: Ordering,
            ) -> Result<u32, u32> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn compare_exchange_weak(
                &self,
                current: u32,
                new: u32,
                success: Ordering,
                failure: Ordering,
            ) -> Result<u32, u32> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_add(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_sub(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_and(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_nand(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_or(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_xor(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_update<F>(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                f: F,
            ) -> Result<u32, u32>
            where
                F: FnMut(u32) -> Option<u32>,
            {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn try_update(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                mut f: impl FnMut(u32) -> Option<u32>,
            ) -> Result<u32, u32> {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn update(
                &self,
                set_order: Ordering,
                fetch_order: Ordering,
                mut f: impl FnMut(u32) -> u32,
            ) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_max(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }

            #[rustc_should_not_be_called_on_const_items]
            pub fn fetch_min(&self, val: u32, order: Ordering) -> u32 {
                loop {}
            }
        }
    }
}
"#;

#[test]
fn transmute_imut_to_mut() {
    check(
        "transmute-imut-to-mut.rs",
        "sized",
        TRANSMUTE,
        include_str!("values/ui/transmute-imut-to-mut.rs"),
        include_str!("values/ui/transmute-imut-to-mut.stderr"),
    );
}

/// A function named `transmute` that is not the intrinsic transmutes nothing to rustc.
#[test]
fn a_transmute_that_is_not_the_intrinsic() {
    let std = r#"
pub mod mem {
    pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst {
        loop {}
    }
}
"#;
    let source = r#"
fn main() {
    let _: &mut u8 = unsafe { std::mem::transmute(&0u8) };
}
"#;
    check("not-the-intrinsic.rs", "sized", std, source, "");
}

/// `tests/ui/lint/force-warn/deny-by-default-lint.rs`.
#[test]
fn force_warn_mutable_transmutes() {
    let actual = ui(Ui {
        name: "deny-by-default-lint.rs",
        core: "sized",
        std: Some(("", TRANSMUTE)),
        flags: &["--force-warn mutable_transmutes"],
        source: include_str!("values/ui/deny-by-default-lint.rs"),
        ..Ui::default()
    });
    assert_eq!(actual, include_str!("values/ui/deny-by-default-lint.stderr"));
}

/// `tests/ui/transmute/transmute-bool-u8.rs`: `#![allow(unnecessary_transmutes)]` on the crate.
#[test]
fn transmute_bool_u8() {
    check("transmute-bool-u8.rs", "sized", TRANSMUTE, include_str!("values/ui/transmute-bool-u8.rs"), "");
}

#[test]
fn lint_enum_intrinsics_non_enums() {
    let source = include_str!("values/ui/lint-enum-intrinsics-non-enums.rs");
    let library = &["variant_count"];
    let actual = ui(Ui { name: "lint-enum-intrinsics-non-enums.rs", core: "sized, discriminant", std: Some(("", DISCRIMINANT)), library, source, ..Ui::default() });
    assert_eq!(actual, include_str!("values/ui/lint-enum-intrinsics-non-enums.stderr"));
}

#[test]
fn ptr_to_int_transmute_in_consts_issue_87525() {
    check(
        "ptr-to-int-transmute-in-consts-issue-87525.rs",
        "sized, add",
        TRANSMUTE,
        include_str!("values/ui/ptr-to-int-transmute-in-consts-issue-87525.rs"),
        include_str!("values/ui/ptr-to-int-transmute-in-consts-issue-87525.stderr"),
    );
}

#[test]
fn int_to_ptr() {
    check(
        "int_to_ptr.rs",
        "sized, copy, add, fn",
        TRANSMUTE,
        include_str!("values/ui/int_to_ptr.rs"),
        include_str!("values/ui/int_to_ptr.stderr"),
    );
}

#[test]
fn unnecessary_transmutation() {
    check(
        "unnecessary-transmutation.rs",
        "sized, copy",
        TRANSMUTE,
        include_str!("values/ui/unnecessary-transmutation.rs"),
        include_str!("values/ui/unnecessary-transmutation.stderr"),
    );
}

/// `NonZero<u32>` is core's, printed untrimmed with its `core::num::niche_types` inner type;
/// it needs minicore's `non_zero` to have them (requested of the owner of `test-utils`).
#[test]
fn invalid_value() {
    let source = include_str!("values/ui/invalid_value.rs");
    let library = &["pattern_type_macro"];
    let actual = ui(Ui { name: "invalid_value.rs", core: "sized, option, result, send, non_null, non_zero, pat, manually_drop, fn", std: Some(("", UNINIT)), library, source, ..Ui::default() });
    assert_reports(
        &actual,
        &[
            error(50, &["`&T`"]),
            error(51, &["`&T`"]),
            error(53, &["`Wrap<&T>`"]),
            error(54, &["`Wrap<&T>`"]),
            error(61, &["`!`"]),
            error(62, &["`!`"]),
            error(64, &["`(i32, !)`"]),
            error(65, &["`(i32, !)`"]),
            error(67, &["`Void`"]),
            error(68, &["`Void`"]),
            error(70, &["`&i32`"]),
            error(71, &["`&i32`"]),
            error(73, &["`Ref`"]),
            error(74, &["`Ref`"]),
            error(76, &["`fn()`"]),
            error(77, &["`fn()`"]),
            error(79, &["`Wrap<fn()>`"]),
            error(80, &["`Wrap<fn()>`"]),
            error(82, &["`WrapEnum<fn()>`"]),
            error(83, &["`WrapEnum<fn()>`"]),
            error(85, &["`Wrap<(RefPair, i32)>`"]),
            error(86, &["`Wrap<(RefPair, i32)>`"]),
            error(88, &["`NonNull<i32>`"]),
            error(89, &["`NonNull<i32>`"]),
            error(91, &["`(NonZero<u32>, i32)`"]),
            error(92, &["`(NonZero<u32>, i32)`"]),
            error(94, &["`*const dyn Send`"]),
            error(95, &["`*const dyn Send`"]),
            error(97, &["`[fn(); 2]`"]),
            error(98, &["`[fn(); 2]`"]),
            error(100, &["`TwoUninhabited`"]),
            error(101, &["`TwoUninhabited`"]),
            error(103, &["`OneFruitNonZero`"]),
            error(104, &["`OneFruitNonZero`"]),
            error(108, &["`bool`"]),
            error(111, &["`Wrap<char>`"]),
            error(114, &["`NonBig`"]),
            error(117, &["`Fruit`"]),
            error(120, &["`[bool; 2]`"]),
            error(123, &["`i32`"]),
            error(126, &["`f32`"]),
            error(129, &["`*const ()`"]),
            error(132, &["`*const [()]`"]),
            error(135, &["`WrapAroundRange`"]),
            error(140, &["`Result<i32, i32>`"]),
            error(148, &["`&i32`"]),
            error(149, &["`&[i32]`"]),
            error(150, &["`NonZero<u32>`"]),
            error(153, &["`NonNull<i32>`"]),
            error(154, &["`NonNull<i32>`"]),
            error(155, &["`bool`"]),
        ],
    );
}

#[test]
fn const_item_interior_mutations_const_cell() {
    let source = include_str!("values/ui/const-item-interior-mutations-const-cell.rs");
    let library = &["once_cell_try", "once_cell_try_insert", "sync_unsafe_cell", "unsafe_cell_access"];
    let actual = ui(Ui { name: "const-item-interior-mutations-const-cell.rs", core: "sized, copy, option, result, fn, add, deref", std: Some(("", CELLS)), library, source, ..Ui::default() });
    assert_eq!(actual, include_str!("values/ui/const-item-interior-mutations-const-cell.stderr"));
}

/// `mutex.lock().unwrap()` needs minicore's `Result::unwrap` (requested of the owner of
/// `crates/test-utils`).
#[test]
fn const_item_interior_mutations_const() {
    let source = include_str!("values/ui/const-item-interior-mutations-const.rs");
    let library = &["lock_value_accessors", "once_cell_try", "once_cell_try_insert"];
    let actual = ui(Ui { name: "const-item-interior-mutations-const.rs", core: "sized, copy, option, result, fn, eq, drop, fmt", std: Some(("", SYNC)), library, source, ..Ui::default() });
    assert_eq!(actual, include_str!("values/ui/const-item-interior-mutations-const.stderr"));
}

#[test]
fn const_item_interior_mutations_const_atomics() {
    check(
        "const-item-interior-mutations-const-atomics.rs",
        "sized, copy, option, result, fn",
        ATOMICS,
        include_str!("values/ui/const-item-interior-mutations-const-atomics.rs"),
        include_str!("values/ui/const-item-interior-mutations-const-atomics.stderr"),
    );
}

#[test]
fn const_item_interior_mutations_const_deref() {
    check(
        "const-item-interior-mutations-const-deref.rs",
        "sized, copy, option, result, fn, deref",
        CELLS,
        include_str!("values/ui/const-item-interior-mutations-const-deref.rs"),
        "",
    );
}
