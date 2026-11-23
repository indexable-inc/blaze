//! Which types a const generic parameter may have: the one rule `own_generics` asks.
//!
//! The Reference, "Generic parameters", "Const generics": "The only allowed types of const
//! parameters are `u8`, `u16`, `u32`, `u64`, `u128`, `usize`, `i8`, `i16`, `i32`, `i64`,
//! `i128`, `isize`, `char` and `bool`." That is the stable language. The pinned library goes
//! past it under `adt_const_params` (library/core/src/lib.rs: `#![feature(adt_const_params)]`),
//! and states the extended rule as a trait (library/core/src/marker.rs):
//!
//! ```text
//! /// A marker for types which can be used as types of `const` generic parameters.
//! #[lang = "const_param_ty"]
//! pub trait ConstParamTy_: StructuralPartialEq + Eq {}
//!
//! marker_impls! {
//!     ConstParamTy_ for
//!         usize, u8, u16, u32, u64, u128,
//!         isize, i8, i16, i32, i64, i128,
//!         bool,
//!         char,
//!         (),
//!         {T: ConstParamTy_, const N: usize} [T; N],
//! }
//!
//! marker_impls! {
//!     #[unstable(feature = "unsized_const_params", issue = "128028")]
//!     ConstParamTy_ for
//!         str,
//!         {T: ConstParamTy_} [T],
//!         {T: ConstParamTy_ + ?Sized} &T,
//! }
//! ```
//!
//! with tuples of `ConstParamTy_` elements (library/core/src/tuple.rs: `impl<$($T:
//! ConstParamTy_),+> ConstParamTy_ for ($($T,)+)`), and a struct or enum by its own impl,
//! written or derived (`#[derive(ConstParamTy)]`; core's `mem::transmutability::Assume` writes
//! `impl ConstParamTy_ for Assume {}` and is the type of `TransmuteFrom<Src, const ASSUME:
//! Assume = { Assume::NOTHING }>`, library/core/src/mem/transmutability.rs:91).
//!
//! So [`admits`] takes, by the type's SHAPE, exactly the types those impls can cover: the
//! integers, `bool`, `char`, a struct or enum (`Adt`), a tuple and an array of admitted
//! types, and a shared reference to an admitted type, to `str` or to a slice of an admitted
//! type. `str` and `[T]` alone are refused: a const parameter's type is `Sized` (they are
//! admitted only behind `&`). Everything else is refused by name ([`NotConstParamTy`]): a
//! float, a raw pointer, a `&mut`, a fn pointer or fn item, `!`, a closure, a trait object, a
//! generic parameter (E0770: a const parameter's type may not name one), a projection, an
//! opaque type, a pattern type.
//!
//! The head needs only the parameter's TYPE; a VALUE of a struct, data-enum, reference,
//! tuple or array type (a value tree) has no `PNode` yet, and a const argument or default of
//! such a type is refused where it is read (types.rs `const_term`), by name: a head of an item
//! that declares such a parameter builds, and only a use that writes a value of it is refused.
//!
//! OWED, by name (not decided here, and never guessed):
//! - that the ADT really implements `ConstParamTy_` (E0741: `struct S;` with no impl is
//!   refused by the pinned nightly): a goal `S: ConstParamTy_` for the item-level check,
//!   which the rebuilt layer does not have yet (blaze-typeck rules/control.rs: "the rebuilt
//!   layer has no item-level check yet");
//! - the feature gate (E0658 without `adt_const_params` / `unsized_const_params`): front2
//!   reads no crate's `#![feature]` list (no `NamesSource` question answers it).

use blaze_own::{AVec, Alloc};
use blaze_typeck::{Mutbl, PNode, PTy, PortableTys};

/// The most nodes [`admits`] visits for one parameter's type (rule 18: a bounded work list).
const MOST_NODES: u32 = 1 << 12;

/// Why a type is not a const parameter's type, by the node that refused it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NotConstParamTy {
    /// `f16`, `f32`, `f64`, `f128`: no `ConstParamTy_` impl (floats have no `Eq`).
    Float,
    /// `*const T` / `*mut T`.
    RawPointer,
    /// `&mut T`: only `&T` has the impl.
    MutRef,
    /// `str` or `[T]` not behind `&`: a const parameter's type is `Sized`.
    Unsized,
    /// A fn pointer or a fn item's type.
    Fn,
    /// `!`.
    Never,
    /// A closure's or an async block's type.
    Closure,
    /// `dyn Trait`.
    TraitObject,
    /// An extern type (`extern { type T; }`), by value or behind `&`: core gives it no
    /// `ConstParamTy_` impl.
    ExternType,
    /// A generic parameter (E0770).
    Parameter,
    /// `<T as Tr>::X` or an opaque type: the type is not known at the head.
    Projection,
    /// A pattern type `T is PAT`.
    PatternType,
    /// A hole, an error type, or a node that is no type (a const value): a malformed head.
    NotAType,
    /// More than [`MOST_NODES`] nodes under the parameter's type.
    TooLarge,
}

impl NotConstParamTy {
    /// The refusal's text, naming the form and the rule.
    pub(super) fn text(self) -> &'static str {
        match self {
            NotConstParamTy::Float => "a const parameter of a floating-point type (E0741: no `ConstParamTy_` impl)",
            NotConstParamTy::RawPointer => "a const parameter of a raw-pointer type (E0741: no `ConstParamTy_` impl)",
            NotConstParamTy::MutRef => "a const parameter of a `&mut` type (E0741: only `&T` implements `ConstParamTy_`)",
            NotConstParamTy::Unsized => "a const parameter of type `str` or `[T]` not behind `&` (a const parameter's type is `Sized`)",
            NotConstParamTy::Fn => "a const parameter of a fn pointer or fn item type (E0741: no `ConstParamTy_` impl)",
            NotConstParamTy::Never => "a const parameter of type `!` (E0741: no `ConstParamTy_` impl)",
            NotConstParamTy::Closure => "a const parameter of a closure or async block type (E0741: no `ConstParamTy_` impl)",
            NotConstParamTy::TraitObject => "a const parameter of a trait object type (E0741: no `ConstParamTy_` impl)",
            NotConstParamTy::ExternType => "a const parameter of an extern type (E0741: no `ConstParamTy_` impl)",
            NotConstParamTy::Parameter => "a const parameter whose type names a generic parameter (E0770)",
            NotConstParamTy::Projection => "a const parameter whose type is a projection or an opaque type",
            NotConstParamTy::PatternType => "a const parameter of a pattern type",
            NotConstParamTy::NotAType => "a const parameter whose type is a hole, an error or no type (a malformed head)",
            NotConstParamTy::TooLarge => "a const parameter whose type has more than 4096 nodes",
        }
    }
}

/// The refusal of a const argument or default whose parameter's type has no value node: a
/// reference, a tuple, an array or a generic ADT (`R<{ "x" }, T>`, `W<{ (1, 2) }>`): its value
/// is a value tree, and `PNode::ConstValue` holds bits of a scalar or a fieldless enum's
/// variant only. Refused where the value is read, never at the parameter's head.
pub(super) const NO_VALUE_NODE: &str = "a const argument of a reference, tuple, array or generic ADT type (its value is a value tree, which has no type node yet)";

/// Whether a const argument of the parameter type `want` (its node; `None` where the table
/// has none) has a value node to be read into: the integers, `bool`, `char` (bits), an ADT
/// with no arguments (a path to a fieldless enum's unit variant: const_enum.rs `term`, which
/// refuses any other path by name), and a type not known here (`Infer`, `Error`: the
/// caller's own reading). `false` for every other admitted type ([`NO_VALUE_NODE`]).
pub(super) fn value_has_node(want: Option<&PNode>) -> bool {
    match want {
        Some(PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Infer | PNode::Error) => true,
        Some(PNode::Adt(_, l)) => l.len == 0,
        _ => false,
    }
}

/// Whether `t` may be a const parameter's type by its shape (the module's rule), by a work
/// list (no recursion): `Ok(())`, or the first node that refuses it. `alloc` holds the work
/// list for the call.
pub(super) fn admits(tys: &PortableTys<'_>, t: PTy, alloc: Alloc<'_>) -> Result<(), NotConstParamTy> {
    let view = tys.view();
    // Each entry: a type, and whether it stands right behind a `&` (where `str` and `[T]`
    // are admitted).
    let mut todo: AVec<'_, (PTy, bool)> = AVec::new_in(alloc);
    todo.push((t, false));
    let mut seen = 0u32;
    while let Some((t, behind_ref)) = todo.pop() {
        seen += 1;
        if seen > MOST_NODES {
            return Err(NotConstParamTy::TooLarge);
        }
        let Some(&node) = view.get(t) else { return Err(NotConstParamTy::NotAType) };
        match node {
            PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) => {}
            // A struct or enum: by its own `ConstParamTy_` impl (OWED: the item-level check).
            PNode::Adt(..) => {}
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                let elems = view.list(l).map_err(|_| NotConstParamTy::NotAType)?;
                for &e in elems {
                    todo.push((e, false));
                }
            }
            PNode::Array(elem, _) => todo.push((elem, false)),
            PNode::Ref(Mutbl::Not, inner) => todo.push((inner, true)),
            PNode::Ref(Mutbl::Mut, _) => return Err(NotConstParamTy::MutRef),
            PNode::Str if behind_ref => {}
            PNode::Slice(elem) if behind_ref => todo.push((elem, false)),
            PNode::Str | PNode::Slice(_) => return Err(NotConstParamTy::Unsized),
            PNode::Float(_) => return Err(NotConstParamTy::Float),
            PNode::Ptr(..) => return Err(NotConstParamTy::RawPointer),
            PNode::FnPtr(..) | PNode::FnDef(..) => return Err(NotConstParamTy::Fn),
            PNode::Never => return Err(NotConstParamTy::Never),
            PNode::Closure { .. } | PNode::Future(..) | PNode::Coroutine { .. } => return Err(NotConstParamTy::Closure),
            PNode::Dynamic(_) => return Err(NotConstParamTy::TraitObject),
            PNode::Foreign(_) => return Err(NotConstParamTy::ExternType),
            PNode::Param(_) | PNode::ConstParam(_) => return Err(NotConstParamTy::Parameter),
            PNode::Alias { .. } | PNode::Opaque(..) => return Err(NotConstParamTy::Projection),
            PNode::Pat(..) => return Err(NotConstParamTy::PatternType),
            PNode::Infer | PNode::Error | PNode::ConstValue { .. } | PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => {
                return Err(NotConstParamTy::NotAType);
            }
        }
    }
    Ok(())
}
