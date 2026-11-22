//! How a function's outside references are named, for every tier. Moved whole from
//! crates/blaze-emit/src/record.rs (its `Outside`, `Named`, `Names`), which re-exports them.

use core::fmt;

use blaze_code::record::Symbol;

use crate::routines::RoutinePlaces;

/// What a function's code or data names outside itself, as the LIR function's reference rows
/// say it (a system routine is not among them: the record names it by its C name itself).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outside {
    /// The function of reference place `place` (`RefKind::Func`).
    Func(u32),
    /// The static of reference place `place` (`RefKind::Static`).
    Static(u32),
    /// The vtable of reference place `place` (`RefKind::Vtable`).
    Vtable(u32),
    /// The thread-local of reference place `place` (`RefKind::Tls`): its import word holds its
    /// offset in a thread's block.
    Tls(u32),
    /// The program's thread-local descriptor static (crates/blaze-build/src/tls.rs).
    TlsDesc,
    /// The program's thread-local getter, `fn(desc) -> block`.
    TlsGetter,
}

impl fmt::Display for Outside {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Outside::Func(place) => write!(f, "function reference {place}"),
            Outside::Static(place) => write!(f, "static reference {place}"),
            Outside::Vtable(place) => write!(f, "vtable reference {place}"),
            Outside::Tls(place) => write!(f, "thread-local reference {place}"),
            Outside::TlsDesc => f.write_str("the thread-local descriptor"),
            Outside::TlsGetter => f.write_str("the thread-local getter"),
        }
    }
}

/// How one [`Outside`] is named in the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Named<'n> {
    /// By its symbol; its space is its kind's (a function's `Space::Func`, a static's and the
    /// descriptor's `Space::Static`, a vtable's `Space::Vtable`, a thread-local's `Space::Tls`).
    Symbol(Symbol),
    /// A C library name the launcher binds with `dlsym` (`Space::Outside`): a function or a
    /// static only, and with no addend.
    C(&'n str),
}

/// The caller's names for what the function references: the run path knows which instance,
/// static or C name each reference place is.
pub trait Names {
    /// `of`'s name, or `None` where the caller has none (a tier refuses the function by name).
    fn name(&self, of: Outside) -> Option<Named<'_>>;

    /// Where each soft-float routine is among the function references [`Names::name`] answers
    /// (`Outside::Func(place)`): the routine table, the seam's input for every tier that
    /// legalizes (D197 root 6, piece (a)). [`RoutinePlaces::NONE`] where the caller binds none:
    /// an `f128` operation is then refused naming its routine. Required, so no caller hands
    /// none by default (rule 75).
    fn routines(&self) -> RoutinePlaces;
}
