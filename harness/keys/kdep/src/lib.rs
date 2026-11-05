pub const SCALE: u32 = 3;

pub struct Shape {
    pub side: u32,
}

#[inline]
pub fn inlined(x: u32) -> u32 {
    x + 1
}

pub fn describe<T: std::fmt::Debug + Default>() -> String {
    format!("{:?} {}", T::default(), std::mem::size_of::<T>())
}

/// Checks for overflow when this crate does. Never inlined, because a function rustc
/// inlines into another crate checks when that crate does.
#[inline(never)]
pub fn stepped(x: u8) -> u8 {
    x + std::hint::black_box(1)
}

/// Checks for overflow when the crate that calls it does: the sum is the caller's type's.
pub fn sum<T: std::ops::Add<Output = T>>(a: T, b: T) -> T {
    a + b
}

/// Checks for overflow when this crate and the one that calls it both do.
pub fn widened<T: Into<u8>>(x: T) -> u8 {
    x.into() + std::hint::black_box(255)
}

/// As `widened`.
#[inline]
pub fn bumped(x: u8) -> u8 {
    x + std::hint::black_box(1)
}

/// Small enough that rustc, when it optimises, compiles it in the crate that calls it, which
/// then decides with the crate it is written in whether it checks.
pub fn small(x: u8) -> u8 {
    x + 1
}
