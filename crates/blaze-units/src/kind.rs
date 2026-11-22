//! The kinds: zero-sized types that keep two quantities of one dimension apart.

use core::{fmt::Debug, hash::Hash};

/// A kind of a physical quantity ([`crate::Length`], [`crate::Angle`], ...). Implement it for a
/// zero-sized type to make a kind of one's own.
pub trait Kind: Copy + Eq + Ord + Hash + Debug + Default + 'static {}

/// The kind of a quantity that has none in particular.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Plain;
impl Kind for Plain {}

/// A kind of time measured: [`Wall`] or [`Cpu`].
pub trait TimeKind: Copy + Eq + Ord + Hash + Debug + Default + 'static {}

/// Time on the wall clock.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Wall;
impl TimeKind for Wall {}

/// Time a processor spent running the thing measured.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Cpu;
impl TimeKind for Cpu {}

/// A kind of bytes counted.
pub trait ByteKind: Copy + Eq + Ord + Hash + Debug + Default + 'static {}

macro_rules! byte_kinds {
    ($($(#[$doc:meta])* $name:ident;)*) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
        pub struct $name;
        impl ByteKind for $name {}
    )*};
}

byte_kinds! {
    /// Bytes an allocator handed out.
    Allocated;
    /// Bytes resident in memory (the footprint, rule 50).
    Resident;
    /// Bytes read from a file or the network.
    Read;
    /// Bytes written to a file or the network.
    Written;
    /// Bytes put through a hash.
    Hashed;
}

/// What a [`crate::Count`] counts: the noun printed after the number (`12 cells`).
pub trait Counted: Copy + Eq + Ord + Hash + Debug + Default + 'static {
    /// The plural noun, lower case.
    const NOUN: &'static str;
}
