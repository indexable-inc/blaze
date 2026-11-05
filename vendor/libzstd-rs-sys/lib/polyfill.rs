macro_rules! cfg_select {
    ({ $($tt:tt)* }) => {{
        $crate::cfg_select! { $($tt)* }
    }};
    (_ => { $($output:tt)* }) => {
        $($output)*
    };
    (
        $cfg:meta => $output:tt
        $($( $rest:tt )+)?
    ) => {
        #[cfg($cfg)]
        $crate::lib::polyfill::cfg_select! { _ => $output }
        $(
            #[cfg(not($cfg))]
            $crate::lib::polyfill::cfg_select! { $($rest)+ }
        )?
    }
}
pub(crate) use cfg_select;

/// Keep the branches on either side of this point in source order.
///
/// Once inlined, `a && b` on two cheap, side-effect-free tests becomes two branches, and LLVM
/// may evaluate `b` first. That matters when `a` is the predictable test and `b` the
/// unpredictable one: e.g. in the match finders, the data comparison almost never succeeds,
/// while the match-index validity check is close to random.
macro_rules! branch_barrier {
    () => {
        #[cfg(not(any(target_family = "wasm", miri)))]
        // SAFETY: an empty asm block has no effect.
        unsafe {
            core::arch::asm!("", options(preserves_flags));
        }
    };
}
pub(crate) use branch_barrier;

pub trait PointerExt {
    fn wrapping_offset_from(self, other: Self) -> isize;
}

impl<T> PointerExt for *const T {
    fn wrapping_offset_from(self, base: Self) -> isize {
        ((self as isize) - (base as isize)) / size_of::<T>() as isize
    }
}

impl<T> PointerExt for *mut T {
    /// Like `offset_from`, but without the UB.
    fn wrapping_offset_from(self, base: Self) -> isize {
        ((self as isize) - (base as isize)) / size_of::<T>() as isize
    }
}

cfg_select! {
    feature = "nightly" => {
        pub use core::hint::{cold_path, likely, unlikely};
    }
    _ => {
        #[inline(always)]
        pub fn likely(b: bool) -> bool {
            if !b {
                cold_path()
            }

            b
        }
        #[inline(always)]
        pub fn unlikely(b: bool) -> bool {
            if b {
                cold_path();
            }

            b
        }

        #[cold]
        pub fn cold_path() {}
    }
}
