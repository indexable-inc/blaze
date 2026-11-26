//! Zero-cost, `#![forbid(unsafe_code)]`, `#![no_std]` Finite Const Value-Set Pattern Types
//! (`bstd::pat_ty`, `docs/v2/plan/design/refined-values.md`).
//!
//! # Two-Tier Design
//! 1. **Compiler-native finite value-set pattern types (`#![blaze::pattern_types]`)**: backed by
//!    `blaze_ty::pat_ty::ConstSetPatTy` and `blaze_lower::form::TyPat::In`, supporting
//!    `SemVer is in VERSIONS` and `&str is in ["1.1.1", "1.2.2"]` with zero wrapper types, free
//!    subset subtyping (`OLD <: ALL <: T`), exhaustive `match` narrowing, and `&uniq` write guards.
//! 2. **Portable zero-cost Rust surface (this module)**: works today under both Blaze and `rustc`
//!    with **zero `unsafe`**, **zero proc macros**, and **zero heap allocation** via
//!    [`ValueSet`], [`FinitePat`], [`InSet`], and [`crate::define_value_set!`]:
//!
//! ```ignore
//! #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
//! struct SemVer { major: u16, minor: u16, patch: u16 }
//!
//! const VERSIONS: [SemVer; 3] = [
//!     SemVer { major: 1, minor: 1, patch: 1 },
//!     SemVer { major: 1, minor: 2, patch: 2 },
//!     SemVer { major: 3, minor: 4, patch: 5 },
//! ];
//!
//! bstd::define_value_set! {
//!     pub struct KnownVersions: SemVer = &VERSIONS;
//! }
//!
//! let v = bstd::pat_ty::InSet::<SemVer, KnownVersions>::refine(VERSIONS[1]).unwrap();
//! assert_eq!(v.major, 1);
//! assert_eq!(v.widen(), VERSIONS[1]);
//! ```

#![forbid(unsafe_code)]

use core::fmt;
use core::hash::{Hash, Hasher};
use core::marker::PhantomData;
use core::ops::Deref;

/// Type-level finite compile-time value set over `T` (mirrors `T is in CONST_SLICE`).
pub trait ValueSet<T: 'static> {
    /// Allowed compile-time values of `T`.
    const VALUES: &'static [T];
}

/// A finite pattern-refined value whose inhabitants are restricted to `Self::VALUES`.
pub trait FinitePat: Sized {
    /// Underlying unrefined base type.
    type Base: PartialEq + 'static;

    /// Allowed values of [`Self::Base`].
    const VALUES: &'static [Self::Base];

    /// Refines `value` into `Self` iff `value` appears in [`Self::VALUES`].
    fn refine(value: Self::Base) -> Option<Self>;

    /// Widens `self` back to [`Self::Base`] with zero runtime cost.
    fn widen(self) -> Self::Base;

    /// Borrows the underlying [`Self::Base`] value without allowing mutating escapes.
    fn as_base(&self) -> &Self::Base;
}

/// Typed error returned by [`InSet::refine_with_report`] when a value is not a member of `S::VALUES`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotInValueSet<T: 'static> {
    /// The value that failed refinement.
    pub rejected: T,
    /// The full allowed set against which `rejected` was checked.
    pub allowed: &'static [T],
}

impl<T: fmt::Debug + 'static> fmt::Display for NotInValueSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "value {:?} is not in the allowed finite value set ({} permitted values)",
            self.rejected,
            self.allowed.len()
        )
    }
}

impl<T: fmt::Debug + 'static> core::error::Error for NotInValueSet<T> {}

/// Zero-overhead, layout-identical (`#[repr(transparent)]`) value of `T` proven to belong to
/// `S::VALUES` (portable library counterpart of `T is in CONST_SLICE`).
///
/// Only immutable deref (`Deref<Target = T>`, `AsRef<T>`) is exposed — `DerefMut` is intentionally
/// omitted so field mutation cannot silently move the value outside `S::VALUES`.
#[repr(transparent)]
pub struct InSet<T, S> {
    value: T,
    _set: PhantomData<S>,
}

impl<T, S> InSet<T, S> {
    /// Borrows the underlying `T` value.
    #[inline(always)]
    pub const fn as_base(&self) -> &T {
        &self.value
    }

    /// Widens back into the unrefined base type `T` at zero cost.
    #[inline(always)]
    pub fn widen(self) -> T {
        self.value
    }
}

impl<T: PartialEq + 'static, S: ValueSet<T>> InSet<T, S> {
    /// Returns `true` iff `value` is a member of `S::VALUES`.
    #[inline]
    pub fn contains(value: &T) -> bool {
        S::VALUES.iter().any(|allowed| allowed == value)
    }

    /// Refines `value` into `InSet<T, S>` iff `value` is a member of `S::VALUES`.
    #[inline]
    pub fn refine(value: T) -> Option<Self> {
        if Self::contains(&value) {
            Some(Self { value, _set: PhantomData })
        } else {
            None
        }
    }

    /// Refines `value` into `InSet<T, S>`, returning a structured [`NotInValueSet`] report on
    /// refusal naming both the rejected value and the allowed set.
    #[inline]
    pub fn refine_with_report(value: T) -> Result<Self, NotInValueSet<T>> {
        if Self::contains(&value) {
            Ok(Self { value, _set: PhantomData })
        } else {
            Err(NotInValueSet { rejected: value, allowed: S::VALUES })
        }
    }

    /// Attempts to re-refine `self` into another value set `W` (succeeds for any superset `W`).
    #[inline]
    pub fn try_into_set<W: ValueSet<T>>(self) -> Option<InSet<T, W>> {
        InSet::<T, W>::refine(self.value)
    }
}

impl<T: PartialEq + 'static, S: ValueSet<T>> FinitePat for InSet<T, S> {
    type Base = T;

    const VALUES: &'static [T] = S::VALUES;

    #[inline]
    fn refine(value: Self::Base) -> Option<Self> {
        InSet::refine(value)
    }

    #[inline(always)]
    fn widen(self) -> Self::Base {
        InSet::widen(self)
    }

    #[inline(always)]
    fn as_base(&self) -> &Self::Base {
        InSet::as_base(self)
    }
}

impl<T: Clone, S> Clone for InSet<T, S> {
    #[inline]
    fn clone(&self) -> Self {
        Self { value: self.value.clone(), _set: PhantomData }
    }
}

impl<T: Copy, S> Copy for InSet<T, S> {}

impl<T: PartialEq, S> PartialEq for InSet<T, S> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq, S> Eq for InSet<T, S> {}

impl<T: Hash, S> Hash for InSet<T, S> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<T: fmt::Debug, S> fmt::Debug for InSet<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.value, f)
    }
}

impl<T: fmt::Display, S> fmt::Display for InSet<T, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.value, f)
    }
}

impl<T, S> Deref for InSet<T, S> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T, S> AsRef<T> for InSet<T, S> {
    #[inline(always)]
    fn as_ref(&self) -> &T {
        &self.value
    }
}

/// Declares a zero-sized [`ValueSet`] marker (and optional [`InSet`] type alias) backed by a
/// compile-time constant slice (`const VALUES: &'static [T]`).
///
/// # Examples
/// ```ignore
/// bstd::define_value_set! {
///     pub struct KnownVersions: SemVer = &VERSIONS;
/// }
///
/// // Or with a convenience type alias `pub type KnownVersion = InSet<SemVer, KnownVersions>`:
/// bstd::define_value_set! {
///     pub struct KnownVersions: SemVer = &VERSIONS;
///     pub type KnownVersion;
/// }
/// ```
#[macro_export]
macro_rules! define_value_set {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident : $ty:ty = $values:expr $(;)?
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        $vis struct $name;

        impl $crate::pat_ty::ValueSet<$ty> for $name {
            const VALUES: &'static [$ty] = $values;
        }
    };
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident : $ty:ty = $values:expr;
        $(#[$alias_meta:meta])*
        $alias_vis:vis type $alias:ident $(;)?
    ) => {
        $crate::define_value_set! {
            $(#[$meta])*
            $vis struct $name : $ty = $values;
        }

        $(#[$alias_meta])*
        $alias_vis type $alias = $crate::pat_ty::InSet<$ty, $name>;
    };
}

#[cfg(test)]
mod tests {
    use core::mem::{align_of, size_of};

    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    struct SemVer {
        major: u16,
        minor: u16,
        patch: u16,
    }

    const VERSIONS: [SemVer; 3] = [
        SemVer { major: 1, minor: 1, patch: 1 },
        SemVer { major: 1, minor: 2, patch: 2 },
        SemVer { major: 3, minor: 4, patch: 5 },
    ];

    const LEGACY_VERSIONS: [SemVer; 2] = [
        SemVer { major: 1, minor: 1, patch: 1 },
        SemVer { major: 1, minor: 2, patch: 2 },
    ];

    crate::define_value_set! {
        struct AllVersions: SemVer = &VERSIONS;
        type KnownVersion;
    }

    crate::define_value_set! {
        struct LegacyVersions: SemVer = &LEGACY_VERSIONS;
        type LegacyVersion;
    }

    crate::define_value_set! {
        struct VersionStrings: &'static str = &["1.1.1", "1.2.2", "3.4.5"];
    }

    #[test]
    fn semver_const_set_refines_valid_versions_and_rejects_invalid_ones() {
        // Layout-transparent: zero wrapper overhead.
        assert_eq!(size_of::<KnownVersion>(), size_of::<SemVer>());
        assert_eq!(align_of::<KnownVersion>(), align_of::<SemVer>());

        let v111 = SemVer { major: 1, minor: 1, patch: 1 };
        let v122 = SemVer { major: 1, minor: 2, patch: 2 };
        let v345 = SemVer { major: 3, minor: 4, patch: 5 };
        let invalid = SemVer { major: 2, minor: 0, patch: 0 };

        assert!(KnownVersion::contains(&v111));
        assert!(KnownVersion::contains(&v122));
        assert!(KnownVersion::contains(&v345));
        assert!(!KnownVersion::contains(&invalid));

        let refined = <KnownVersion as FinitePat>::refine(v122).expect("1.2.2 is in VERSIONS");
        // Deref + AsRef + Copy + Eq + Hash.
        assert_eq!(refined.major, 1);
        assert_eq!(refined.minor, 2);
        assert_eq!(refined.patch, 2);
        assert_eq!(refined.as_ref(), &v122);
        assert_eq!(refined.as_base(), &v122);
        let copy = refined;
        assert_eq!(copy, refined);
        assert_eq!(refined.widen(), v122);

        // Invalid version is rejected by both `refine` and `refine_with_report`.
        assert_eq!(KnownVersion::refine(invalid), None);
        let err = KnownVersion::refine_with_report(invalid).unwrap_err();
        assert_eq!(err.rejected, invalid);
        assert_eq!(err.allowed, &VERSIONS);

        // Subset widening via `try_into_set`.
        let legacy = LegacyVersion::refine(v111).expect("1.1.1 is legacy");
        let widened: KnownVersion = legacy.try_into_set::<AllVersions>().expect("legacy <: all");
        assert_eq!(*widened, v111);
        assert!(LegacyVersion::refine(v345).is_none());
    }

    #[test]
    fn str_const_set_refines_literals() {
        type StrVer = InSet<&'static str, VersionStrings>;
        assert_eq!(<StrVer as FinitePat>::VALUES, &["1.1.1", "1.2.2", "3.4.5"]);
        let ok = StrVer::refine("3.4.5").expect("3.4.5 allowed");
        assert_eq!(*ok, "3.4.5");
        assert!(StrVer::refine("0.0.1").is_none());
    }
}
