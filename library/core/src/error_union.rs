//! blaze's error unions: the type of `A | B` in a crate with `#![blaze::error_unions]`, an
//! extension of blaze's, not the Rust project's (`library/UPSTREAM` lists this module among
//! blaze's changes to the standard library).
//!
//! hir-ty lowers `A | B` to [`ErrorUnion`] over the tuple of its members in one canonical order
//! (`hir_ty::lower::error_union`), so `A | B`, `B | A` and `A | A | B` are one type, and the
//! value is one member's, held in the enum [`Members::Repr`] of the tuple. Everything else a
//! union is, it is from the items here:
//!
//! - `Debug`, `Display`, `Error`, `Clone` and `Hash` delegate to the member held.
//! - A member, or a union of some of the members, converts to the union: [`UnionFrom`], which
//!   only blaze's trait solver answers, says where each of its values goes (its `Path`), and
//!   [`Inject`], written here over those paths, puts it there. `From` is implemented through
//!   them, so `?` widens; a coercion to a union calls [`widen`].
//!
//! Every item is unstable under `blaze_error_unions`; nothing else in `core` refers to it, so a
//! crate without error unions sees `core` as the nightly's.

use crate::error::Error;
use crate::fmt;
use crate::hash::{Hash, Hasher};

/// The members of an error union: a tuple of 2 to 12 types (hir-ty's
/// `ERROR_UNION_MAX_MEMBERS`), and the enum that holds one value of one of them.
#[unstable(feature = "blaze_error_unions", issue = "none")]
pub trait Members {
    /// One value of one member: variant `Mi` holds the tuple's `i`th type.
    #[unstable(feature = "blaze_error_unions", issue = "none")]
    type Repr;
}

/// The type of the error union of the members `M`, a tuple in canonical order: what `A | B`
/// names.
#[lang = "error_union"]
#[unstable(feature = "blaze_error_unions", issue = "none")]
pub struct ErrorUnion<M: Members>(
    /// The value, of the member its variant names.
    #[unstable(feature = "blaze_error_unions", issue = "none")]
    pub M::Repr,
);

/// `Self` is an error union and `S` one of its members, or an error union of some of its
/// members but not all of them. The trait solver answers it from the types alone; no impl of it
/// can be written.
#[lang = "error_union_from"]
#[rustc_deny_explicit_impl]
#[unstable(feature = "blaze_error_unions", issue = "none")]
pub trait UnionFrom<S> {
    /// Where each value of `S` goes in `Self`: for a member, the index of the member it is in
    /// `Self` as that many one-tuples around `()` (`()` is the first member, `((),)` the
    /// second); for a union, the tuple of its members' paths in its own order.
    #[unstable(feature = "blaze_error_unions", issue = "none")]
    type Path;
}

/// `Self` holds each value of `S` at the path `P`, the [`UnionFrom::Path`] of `S` in `Self`.
#[unstable(feature = "blaze_error_unions", issue = "none")]
pub trait Inject<S, P> {
    /// The value of `Self` that holds `value`.
    #[unstable(feature = "blaze_error_unions", issue = "none")]
    fn inject(value: S) -> Self;
}

/// `value`, of a member of `U` or of a union of some of its members, as a `U`: what a coercion
/// to an error union calls.
#[lang = "error_union_widen"]
#[unstable(feature = "blaze_error_unions", issue = "none")]
#[inline]
pub fn widen<S, U>(value: S) -> U
where
    U: UnionFrom<S> + Inject<S, <U as UnionFrom<S>>::Path>,
{
    <U as Inject<S, <U as UnionFrom<S>>::Path>>::inject(value)
}

impl<S, U> From<S> for U
where
    U: UnionFrom<S> + Inject<S, <U as UnionFrom<S>>::Path>,
{
    /// `value` in the union `U`: [`widen`].
    #[inline]
    fn from(value: S) -> U {
        widen(value)
    }
}

/// `impl Inject<$member, $path> for` the union of `$all`: the variant `$variant` of `$repr`.
macro_rules! inject_member {
    ($repr:ident ($($all:ident),+) $variant:ident $member:ident $path:ty) => {
        impl<$($all),+> Inject<$member, $path> for ErrorUnion<($($all,)+)> {
            #[inline]
            fn inject(value: $member) -> Self {
                ErrorUnion($repr::$variant(value))
            }
        }
    };
}

macro_rules! members {
    ($($repr:ident $all:tt { $($variant:ident: $member:ident, $param:ident @ $path:ty;)+ })+) => { $(
        /// One value of one member of an error union of these members.
        #[unstable(feature = "blaze_error_unions", issue = "none")]
        pub enum $repr<$($member),+> {
            $(
                #[unstable(feature = "blaze_error_unions", issue = "none")]
                $variant($member),
            )+
        }

        impl<$($member),+> Members for ($($member,)+) {
            type Repr = $repr<$($member),+>;
        }

        $( inject_member!($repr $all $variant $member $path); )+

        /// A union of these members into any union that has each of them: each member's value
        /// goes where its path says.
        impl<U, $($member,)+ $($param),+> Inject<ErrorUnion<($($member,)+)>, ($($param,)+)> for U
        where
            $(U: Inject<$member, $param>,)+
        {
            #[inline]
            fn inject(value: ErrorUnion<($($member,)+)>) -> U {
                match value.0 { $( $repr::$variant(it) => <U as Inject<$member, $param>>::inject(it), )+ }
            }
        }

        impl<$($member: fmt::Debug),+> fmt::Debug for $repr<$($member),+> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self { $( $repr::$variant(it) => fmt::Debug::fmt(it, f), )+ }
            }
        }

        impl<$($member: fmt::Debug),+> fmt::Debug for ErrorUnion<($($member,)+)> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.0, f)
            }
        }

        impl<$($member: fmt::Display),+> fmt::Display for ErrorUnion<($($member,)+)> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match &self.0 { $( $repr::$variant(it) => fmt::Display::fmt(it, f), )+ }
            }
        }

        impl<$($member: Error),+> Error for ErrorUnion<($($member,)+)> {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                match &self.0 { $( $repr::$variant(it) => it.source(), )+ }
            }
        }

        impl<$($member: Clone),+> Clone for ErrorUnion<($($member,)+)> {
            fn clone(&self) -> Self {
                match &self.0 { $( $repr::$variant(it) => ErrorUnion($repr::$variant(it.clone())), )+ }
            }
        }

        impl<$($member: Hash),+> Hash for ErrorUnion<($($member,)+)> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                match &self.0 { $( $repr::$variant(it) => it.hash(state), )+ }
            }
        }
    )+ };
}

members! {
    Of2 (T0, T1) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
    }
    Of3 (T0, T1, T2) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
    }
    Of4 (T0, T1, T2, T3) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
    }
    Of5 (T0, T1, T2, T3, T4) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
    }
    Of6 (T0, T1, T2, T3, T4, T5) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
        M5: T5, P5 @ ((((((),),),),),);
    }
    Of7 (T0, T1, T2, T3, T4, T5, T6) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
        M5: T5, P5 @ ((((((),),),),),);
        M6: T6, P6 @ (((((((),),),),),),);
    }
    Of8 (T0, T1, T2, T3, T4, T5, T6, T7) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
        M5: T5, P5 @ ((((((),),),),),);
        M6: T6, P6 @ (((((((),),),),),),);
        M7: T7, P7 @ ((((((((),),),),),),),);
    }
    Of9 (T0, T1, T2, T3, T4, T5, T6, T7, T8) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
        M5: T5, P5 @ ((((((),),),),),);
        M6: T6, P6 @ (((((((),),),),),),);
        M7: T7, P7 @ ((((((((),),),),),),),);
        M8: T8, P8 @ (((((((((),),),),),),),),);
    }
    Of10 (T0, T1, T2, T3, T4, T5, T6, T7, T8, T9) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
        M5: T5, P5 @ ((((((),),),),),);
        M6: T6, P6 @ (((((((),),),),),),);
        M7: T7, P7 @ ((((((((),),),),),),),);
        M8: T8, P8 @ (((((((((),),),),),),),),);
        M9: T9, P9 @ ((((((((((),),),),),),),),),);
    }
    Of11 (T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
        M5: T5, P5 @ ((((((),),),),),);
        M6: T6, P6 @ (((((((),),),),),),);
        M7: T7, P7 @ ((((((((),),),),),),),);
        M8: T8, P8 @ (((((((((),),),),),),),),);
        M9: T9, P9 @ ((((((((((),),),),),),),),),);
        M10: T10, P10 @ (((((((((((),),),),),),),),),),);
    }
    Of12 (T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11) {
        M0: T0, P0 @ ();
        M1: T1, P1 @ ((),);
        M2: T2, P2 @ (((),),);
        M3: T3, P3 @ ((((),),),);
        M4: T4, P4 @ (((((),),),),);
        M5: T5, P5 @ ((((((),),),),),);
        M6: T6, P6 @ (((((((),),),),),),);
        M7: T7, P7 @ ((((((((),),),),),),),);
        M8: T8, P8 @ (((((((((),),),),),),),),);
        M9: T9, P9 @ ((((((((((),),),),),),),),),);
        M10: T10, P10 @ (((((((((((),),),),),),),),),),);
        M11: T11, P11 @ ((((((((((((),),),),),),),),),),),);
    }
}
