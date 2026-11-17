//! A const parameter whose type is a FIELDLESS enum, and a unit variant as its value.
//!
//! The Reference, "Generic parameters", "Const generics": "The only allowed types of const
//! parameters are `u8`, `u16`, `u32`, `u64`, `u128`, `usize`, `i8`, `i16`, `i32`, `i64`,
//! `i128`, `isize`, `char` and `bool`." The pinned library goes past that under
//! `adt_const_params`, and the pinned nightly accepts it (library/core/src/intrinsics/mod.rs):
//!
//! ```text
//! #[derive(Debug, ConstParamTy, PartialEq, Eq)]
//! pub enum AtomicOrdering {
//!     Relaxed = 0,
//!     Release = 1,
//!     Acquire = 2,
//!     AcqRel = 3,
//!     SeqCst = 4,
//! }
//! pub const unsafe fn atomic_load<T: Copy, const ORD: AtomicOrdering, const VOLATILE: bool>(
//!     src: *const T,
//! ) -> T;
//! pub const unsafe fn atomic_xchg<T: Copy, const ORD: AtomicOrdering>(dst: *mut T, src: T) -> T;
//! ```
//!
//! called as (library/core/src/sync/atomic.rs, under `use crate::intrinsics::AtomicOrdering as
//! AO;`) `intrinsics::atomic_load::<T, { AO::SeqCst }, VOLATILE>(dst)`.
//!
//! The smallest general form is taken here: the parameter's type is an enum with no generic
//! parameters whose every variant is a unit variant ([`fieldless`], filed with the enum's head
//! in `Maps::fieldless_enums`; which TYPES a const parameter may have is const_param_ty.rs
//! `admits`, the one rule `own_generics` asks). Its value is ONE
//! VARIANT: the node `PortableTys::variant_value` makes, `ConstValue { ty: Adt(enum, []),
//! bits: the variant's index }` ([`term`]). The enum's key is in the value, so two values are
//! equal only when they are the same variant of the same enum: a variant of another enum with
//! the same index is a different term, and where the parameter's type is known it is refused
//! as a mismatch ([`Term::OtherEnum`]).
//!
//! Still refused, by name: a VALUE of any other non-primitive type (a struct's, a tuple's, an
//! array's, a reference's, `&'static str`'s, an enum's with fields or with generics: a value
//! tree, which has no node; the parameter itself has a head, const_param_ty.rs). A const
//! argument of an enum type that is not a path to a variant
//! ([`NOT_A_VARIANT`]: a constant item of that type, an expression that computes a variant):
//! a constant's evaluated bits are its discriminant as the layout stores it, not its
//! variant's index, and nothing here maps one to the other yet.
//! OWED: the enum is not checked to implement `ConstParamTy` (the nightly's E0741).

use blaze_id::ItemKey as LibKey;
use blaze_lower::{View, form as lf, head};
use blaze_surface::{ItemDetail, Range};
use blaze_typeck::{ItemKey, PNode, PTy, PortableTys};

use super::keys::checker_of;
use super::source::NamesSource;
use super::types::{Why, intern, intern_list};
use crate::resolve::{Kind, ResolveError, Target};

/// The refusal of a const argument of an enum type that is not a path to one of its variants.
pub(super) const NOT_A_VARIANT: &str = "a const argument of an enum type that is not a path to a unit variant (a constant item or an expression of that type)";

/// The refusal of a path longer than [`MAX_SEGMENTS`] where a variant's path is read.
pub(super) const TOO_LONG: &str = "a const argument's path of more than sixteen segments";

/// The most segments a variant's path is read with (`crate::a::b::Enum::Variant` has five):
/// a fixed array, so reading one allocates nothing.
const MAX_SEGMENTS: usize = 16;

/// How many variants the item `key` has when it is a fieldless enum with no generic
/// parameters (`generics`: its own type and const parameter count; `v`: its lowered head),
/// else `None`: a struct, a union, an enum with a tuple or record variant, an enum with
/// generics. An enum with no variant is fieldless and has no value.
pub(super) fn fieldless<'n, S: NamesSource<'n>>(names: S, key: LibKey, v: &View<'_>, generics: u32) -> Result<Option<u32>, Why> {
    if generics != 0 || !matches!(names.detail_of(key), Some(ItemDetail::Enum)) {
        return Ok(None);
    }
    let rows = v.rows(v.list(head::FIELDS))?;
    for &row in rows {
        let variant = lf::Variant::decode(row)?;
        if !matches!(variant.shape, lf::Shape::Unit) {
            return Ok(None);
        }
    }
    Ok(Some(rows.len() as u32))
}

/// What a written const argument is where the parameter's type is `want`.
pub(super) enum Term {
    /// `want` is no enum type (an integer, `bool`, `char`, a hole): the caller's own reading.
    NotEnum,
    /// A unit variant of `want`'s enum: its value.
    Is(PTy),
    /// A variant of ANOTHER enum than `want`'s (its key): a type mismatch, which the caller
    /// words with the argument's text.
    OtherEnum(ItemKey),
}

/// The const argument `text` (braces already taken off, trimmed) for a parameter of type
/// `want`. Where `want` is an `Adt` with no arguments (a fieldless enum's value is a unit
/// variant; a struct's is a value tree, so for a struct this refuses every argument by name)
/// the argument must be a path that
/// `resolve` (the value namespace, from the argument's module) names a variant with: of that
/// enum it is [`Term::Is`] its value, the node `PortableTys::variant_value` makes; of another
/// enum [`Term::OtherEnum`]. Anything else there is refused by name ([`NOT_A_VARIANT`]).
pub(super) fn term(tys: &mut PortableTys<'_>, resolve: &dyn Fn(bool, &[&str]) -> Result<Target, ResolveError>, want: PTy, text: &str, r: Range) -> Result<Term, Why> {
    let enum_ = match tys.view().get(want) {
        Some(&PNode::Adt(k, l)) if l.len == 0 => k,
        _ => return Ok(Term::NotEnum),
    };
    let (abs, rest) = match text.strip_prefix("::") {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut segs: [&str; MAX_SEGMENTS] = [""; MAX_SEGMENTS];
    let mut n = 0usize;
    for s in rest.split("::") {
        let s = s.trim();
        let ident = s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ident {
            return Err(Why::Unsupported(NOT_A_VARIANT, r));
        }
        if n == MAX_SEGMENTS {
            return Err(Why::Unsupported(TOO_LONG, r));
        }
        segs[n] = s;
        n += 1;
    }
    let found = resolve(abs, &segs[..n]).map_err(|e| Why::Resolve(e, r))?;
    match found.kind {
        Kind::Variant { adt, index } if found.consumed as usize == n => {
            let of = checker_of(adt);
            if of != enum_ {
                return Ok(Term::OtherEnum(of));
            }
            // As `PortableTys::variant_value` builds it: the enum with no arguments, then the
            // variant's index under it.
            let none = intern_list(tys, &[])?;
            let ty = intern(tys, PNode::Adt(enum_, none))?;
            Ok(Term::Is(intern(tys, PNode::ConstValue { ty, bits: u128::from(index) })?))
        }
        _ => Err(Why::Unsupported(NOT_A_VARIANT, r)),
    }
}
