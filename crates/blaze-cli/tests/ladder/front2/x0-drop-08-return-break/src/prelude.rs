//! The prelude of the RUNNABLE TWINS (front2/x0-*): the `no_core` declarations the twins use,
//! each modeled on the pinned library's own (library/core/src: marker.rs, clone.rs, cmp.rs,
//! ops/arith.rs, ops/bit.rs, ops/function.rs, ops/unsize.rs, ops/drop.rs, ptr/mod.rs,
//! panicking.rs, intrinsics/mod.rs): the same lang item, the same trait signature and
//! supertraits, and the same impl bodies for the primitive types (`self + other` and so on,
//! which the compiler lowers to its builtin operation). Left out of core's: stability and
//! diagnostic attributes, `const` traits, `clone_from`, and every method no twin calls.
//! `Option` and `Ordering` are this program's own enums (core's `Option` lang item is not
//! needed: no `for` loop and no `?` in a twin).
//!
//! This file is the ONE source: each twin holds a byte-equal copy as `src/prelude.rs` (the run
//! path reads only a rung's own `src/`), included with `mod prelude; use prelude::*;`. Change
//! it here, then copy it to every `x0-*/src/prelude.rs`.
#![allow(dead_code)]

#[lang = "pointee_sized"]
pub trait PointeeSized {}

#[lang = "meta_sized"]
pub trait MetaSized: PointeeSized {}

#[lang = "sized"]
pub trait Sized: MetaSized {}

#[lang = "legacy_receiver"]
pub trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}
impl<T: PointeeSized> LegacyReceiver for &mut T {}

#[lang = "clone"]
pub trait Clone: Sized {
    fn clone(&self) -> Self;
}

#[lang = "copy"]
pub trait Copy: Clone {}

macro_rules! copy_impls {
    ($($t:ty)*) => ($(
        impl Clone for $t {
            fn clone(&self) -> $t { *self }
        }
        impl Copy for $t {}
    )*)
}

copy_impls! { u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 bool char }

impl<T: PointeeSized> Clone for &T {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: PointeeSized> Copy for &T {}

impl<T: PointeeSized> Clone for *const T {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: PointeeSized> Copy for *const T {}

impl<T: PointeeSized> Clone for *mut T {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: PointeeSized> Copy for *mut T {}

impl<T: Copy, const N: usize> Clone for [T; N] {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Copy, const N: usize> Copy for [T; N] {}

#[lang = "drop"]
pub trait Drop {
    fn drop(&mut self);
}

#[lang = "drop_in_place"]
pub unsafe fn drop_in_place<T: PointeeSized>(to_drop: *mut T) {
    // As core: the compiler replaces this body with the type's drop glue.
    unsafe { drop_in_place(to_drop) }
}

/// core::mem::drop.
pub fn drop<T>(_x: T) {}

#[lang = "tuple_trait"]
pub trait Tuple {}

#[lang = "fn_once"]
#[rustc_paren_sugar]
pub trait FnOnce<Args: Tuple> {
    #[lang = "fn_once_output"]
    type Output;
    extern "rust-call" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = "fn_mut"]
#[rustc_paren_sugar]
pub trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = "fn"]
#[rustc_paren_sugar]
pub trait Fn<Args: Tuple>: FnMut<Args> {
    extern "rust-call" fn call(&self, args: Args) -> Self::Output;
}

#[lang = "unsize"]
pub trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = "coerce_unsized"]
pub trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a mut U> for &'a mut T {}

#[lang = "dispatch_from_dyn"]
pub trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a mut U> for &'a mut T {}

// What the checks of an operation call when they fail (core's panicking.rs); a twin never
// reaches one: each loops, so a reached one is a hang the run's watchdog names.
#[lang = "panic_bounds_check"]
fn panic_bounds_check(index: usize, len: usize) -> ! {
    loop {}
}
#[lang = "panic_const_add_overflow"]
pub fn panic_const_add_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_sub_overflow"]
pub fn panic_const_sub_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_mul_overflow"]
pub fn panic_const_mul_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_div_overflow"]
pub fn panic_const_div_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_rem_overflow"]
pub fn panic_const_rem_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_neg_overflow"]
pub fn panic_const_neg_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_shr_overflow"]
pub fn panic_const_shr_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_shl_overflow"]
pub fn panic_const_shl_overflow() -> ! {
    loop {}
}
#[lang = "panic_const_div_by_zero"]
pub fn panic_const_div_by_zero() -> ! {
    loop {}
}
#[lang = "panic_const_rem_by_zero"]
pub fn panic_const_rem_by_zero() -> ! {
    loop {}
}

#[lang = "add"]
pub trait Add<Rhs = Self> {
    type Output;
    fn add(self, rhs: Rhs) -> Self::Output;
}
#[lang = "sub"]
pub trait Sub<Rhs = Self> {
    type Output;
    fn sub(self, rhs: Rhs) -> Self::Output;
}
#[lang = "mul"]
pub trait Mul<Rhs = Self> {
    type Output;
    fn mul(self, rhs: Rhs) -> Self::Output;
}
#[lang = "div"]
pub trait Div<Rhs = Self> {
    type Output;
    fn div(self, rhs: Rhs) -> Self::Output;
}
#[lang = "rem"]
pub trait Rem<Rhs = Self> {
    type Output;
    fn rem(self, rhs: Rhs) -> Self::Output;
}
#[lang = "neg"]
pub trait Neg {
    type Output;
    fn neg(self) -> Self::Output;
}
#[lang = "not"]
pub trait Not {
    type Output;
    fn not(self) -> Self::Output;
}
#[lang = "bitand"]
pub trait BitAnd<Rhs = Self> {
    type Output;
    fn bitand(self, rhs: Rhs) -> Self::Output;
}
#[lang = "bitor"]
pub trait BitOr<Rhs = Self> {
    type Output;
    fn bitor(self, rhs: Rhs) -> Self::Output;
}
#[lang = "bitxor"]
pub trait BitXor<Rhs = Self> {
    type Output;
    fn bitxor(self, rhs: Rhs) -> Self::Output;
}
#[lang = "shl"]
pub trait Shl<Rhs = Self> {
    type Output;
    fn shl(self, rhs: Rhs) -> Self::Output;
}
#[lang = "shr"]
pub trait Shr<Rhs = Self> {
    type Output;
    fn shr(self, rhs: Rhs) -> Self::Output;
}
#[lang = "add_assign"]
pub trait AddAssign<Rhs = Self> {
    fn add_assign(&mut self, rhs: Rhs);
}
#[lang = "sub_assign"]
pub trait SubAssign<Rhs = Self> {
    fn sub_assign(&mut self, rhs: Rhs);
}
#[lang = "mul_assign"]
pub trait MulAssign<Rhs = Self> {
    fn mul_assign(&mut self, rhs: Rhs);
}

macro_rules! num_impls {
    ($($t:ty)*) => ($(
        impl Add for $t {
            type Output = $t;
            #[rustc_inherit_overflow_checks]
            fn add(self, other: $t) -> $t { self + other }
        }
        impl Sub for $t {
            type Output = $t;
            #[rustc_inherit_overflow_checks]
            fn sub(self, other: $t) -> $t { self - other }
        }
        impl Mul for $t {
            type Output = $t;
            #[rustc_inherit_overflow_checks]
            fn mul(self, other: $t) -> $t { self * other }
        }
        impl Div for $t {
            type Output = $t;
            fn div(self, other: $t) -> $t { self / other }
        }
        impl Rem for $t {
            type Output = $t;
            fn rem(self, other: $t) -> $t { self % other }
        }
        impl AddAssign for $t {
            #[rustc_inherit_overflow_checks]
            fn add_assign(&mut self, other: $t) { *self += other }
        }
        impl SubAssign for $t {
            #[rustc_inherit_overflow_checks]
            fn sub_assign(&mut self, other: $t) { *self -= other }
        }
        impl MulAssign for $t {
            #[rustc_inherit_overflow_checks]
            fn mul_assign(&mut self, other: $t) { *self *= other }
        }
        impl PartialEq for $t {
            fn eq(&self, other: &$t) -> bool { *self == *other }
            fn ne(&self, other: &$t) -> bool { *self != *other }
        }
        impl PartialOrd for $t {
            fn partial_cmp(&self, other: &$t) -> Option<Ordering> {
                if *self < *other {
                    Some(Ordering::Less)
                } else if *self > *other {
                    Some(Ordering::Greater)
                } else if *self == *other {
                    Some(Ordering::Equal)
                } else {
                    None
                }
            }
            fn lt(&self, other: &$t) -> bool { *self < *other }
            fn le(&self, other: &$t) -> bool { *self <= *other }
            fn gt(&self, other: &$t) -> bool { *self > *other }
            fn ge(&self, other: &$t) -> bool { *self >= *other }
        }
    )*)
}

num_impls! { u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 }

macro_rules! neg_impls {
    ($($t:ty)*) => ($(
        impl Neg for $t {
            type Output = $t;
            #[rustc_inherit_overflow_checks]
            fn neg(self) -> $t { -self }
        }
    )*)
}

neg_impls! { i8 i16 i32 i64 i128 isize f32 f64 }

macro_rules! bit_impls {
    ($($t:ty)*) => ($(
        impl Not for $t {
            type Output = $t;
            fn not(self) -> $t { !self }
        }
        impl BitAnd for $t {
            type Output = $t;
            fn bitand(self, rhs: $t) -> $t { self & rhs }
        }
        impl BitOr for $t {
            type Output = $t;
            fn bitor(self, rhs: $t) -> $t { self | rhs }
        }
        impl BitXor for $t {
            type Output = $t;
            fn bitxor(self, other: $t) -> $t { self ^ other }
        }
    )*)
}

bit_impls! { bool u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize }

macro_rules! shift_impl {
    ($t:ty, $($f:ty)*) => ($(
        impl Shl<$f> for $t {
            type Output = $t;
            #[rustc_inherit_overflow_checks]
            fn shl(self, other: $f) -> $t { self << other }
        }
        impl Shr<$f> for $t {
            type Output = $t;
            #[rustc_inherit_overflow_checks]
            fn shr(self, other: $f) -> $t { self >> other }
        }
    )*)
}

macro_rules! shift_impls {
    ($($t:ty)*) => ($(
        shift_impl! { $t, u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize }
    )*)
}

shift_impls! { u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize }

#[lang = "eq"]
pub trait PartialEq<Rhs: PointeeSized = Self>: PointeeSized {
    fn eq(&self, other: &Rhs) -> bool;
    fn ne(&self, other: &Rhs) -> bool {
        !self.eq(other)
    }
}

impl PartialEq for char {
    fn eq(&self, other: &char) -> bool {
        *self == *other
    }
    fn ne(&self, other: &char) -> bool {
        *self != *other
    }
}

impl PartialEq for bool {
    fn eq(&self, other: &bool) -> bool {
        *self == *other
    }
    fn ne(&self, other: &bool) -> bool {
        *self != *other
    }
}

impl<A: PointeeSized, B: PointeeSized> PartialEq<&B> for &A
where
    A: PartialEq<B>,
{
    fn eq(&self, other: &&B) -> bool {
        PartialEq::eq(*self, *other)
    }
    fn ne(&self, other: &&B) -> bool {
        PartialEq::ne(*self, *other)
    }
}

/// core::cmp::Ordering, with core's discriminants.
#[repr(i8)]
pub enum Ordering {
    Less = -1,
    Equal = 0,
    Greater = 1,
}

/// core::option::Option, the program's own.
pub enum Option<T> {
    None,
    Some(T),
}

pub use Option::{None, Some};

impl<T: Copy> Clone for Option<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Copy> Copy for Option<T> {}

#[lang = "partial_ord"]
pub trait PartialOrd<Rhs: PointeeSized = Self>: PartialEq<Rhs> + PointeeSized {
    fn partial_cmp(&self, other: &Rhs) -> Option<Ordering>;
    fn lt(&self, other: &Rhs) -> bool {
        match self.partial_cmp(other) {
            Some(Ordering::Less) => true,
            _ => false,
        }
    }
    fn le(&self, other: &Rhs) -> bool {
        match self.partial_cmp(other) {
            Some(Ordering::Less | Ordering::Equal) => true,
            _ => false,
        }
    }
    fn gt(&self, other: &Rhs) -> bool {
        match self.partial_cmp(other) {
            Some(Ordering::Greater) => true,
            _ => false,
        }
    }
    fn ge(&self, other: &Rhs) -> bool {
        match self.partial_cmp(other) {
            Some(Ordering::Greater | Ordering::Equal) => true,
            _ => false,
        }
    }
}

impl PartialOrd for char {
    fn partial_cmp(&self, other: &char) -> Option<Ordering> {
        if *self < *other {
            Some(Ordering::Less)
        } else if *self > *other {
            Some(Ordering::Greater)
        } else {
            Some(Ordering::Equal)
        }
    }
    fn lt(&self, other: &char) -> bool {
        *self < *other
    }
    fn le(&self, other: &char) -> bool {
        *self <= *other
    }
    fn gt(&self, other: &char) -> bool {
        *self > *other
    }
    fn ge(&self, other: &char) -> bool {
        *self >= *other
    }
}

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn wrapping_add<T: Copy>(a: T, b: T) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn wrapping_sub<T: Copy>(a: T, b: T) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn wrapping_mul<T: Copy>(a: T, b: T) -> T;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn add_with_overflow<T: Copy>(x: T, y: T) -> (T, bool);

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn mul_with_overflow<T: Copy>(x: T, y: T) -> (T, bool);

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn size_of<T>() -> usize;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const fn align_of<T>() -> usize;

#[rustc_intrinsic]
#[rustc_nounwind]
pub const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
