//! blaze's error unions: the type of `A | B` in a crate with `#![blaze::error_unions]`, an
//! extension of blaze's, not the Rust project's (`library/UPSTREAM` lists this module among
//! blaze's changes to the standard library).
//!
//! hir-ty lowers `A | B` to [`ErrorUnion`] over the tuple of its members in one canonical order
//! (`hir_ty::lower::error_union`), so `A | B`, `B | A` and `A | A | B` are one type, and the
//! value is one member's, held in the enum [`Members::Repr`] of the tuple. Everything else a
//! union is, it is from the impls here: `Debug`, `Display` and `Error` delegate to the member
//! held. Every item is unstable under `blaze_error_unions`; nothing else in `core` refers to
//! it, so a crate without error unions sees `core` as the nightly's.

use crate::error::Error;
use crate::fmt;

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

macro_rules! members {
    ($($repr:ident { $($variant:ident: $member:ident,)+ })+) => { $(
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
    )+ };
}

members! {
    Of2 { M0: T0, M1: T1, }
    Of3 { M0: T0, M1: T1, M2: T2, }
    Of4 { M0: T0, M1: T1, M2: T2, M3: T3, }
    Of5 { M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, }
    Of6 { M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, M5: T5, }
    Of7 { M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, M5: T5, M6: T6, }
    Of8 { M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, M5: T5, M6: T6, M7: T7, }
    Of9 { M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, M5: T5, M6: T6, M7: T7, M8: T8, }
    Of10 { M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, M5: T5, M6: T6, M7: T7, M8: T8, M9: T9, }
    Of11 {
        M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, M5: T5, M6: T6, M7: T7, M8: T8, M9: T9,
        M10: T10,
    }
    Of12 {
        M0: T0, M1: T1, M2: T2, M3: T3, M4: T4, M5: T5, M6: T6, M7: T7, M8: T8, M9: T9,
        M10: T10, M11: T11,
    }
}
