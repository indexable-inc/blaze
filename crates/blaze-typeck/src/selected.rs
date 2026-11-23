//! What the check decided, at each call, for every projection the callee's signature names
//! and for every trait bound of the callee whose instance may hold a region or a caller's type
//! parameter: the impl that answers the bound or gives `<T as Tr>::Out` its value there, or why
//! none does. Kept in the record ([`crate::Checked::selected`]) so the borrow check reads the
//! selected impl's `type Out = ..;` text at the call (blaze-flow-lower impl_call.rs) instead of
//! refusing the call (`Form::CallHiddenInput`) or selecting again itself (rule 77).
//!
//! Where a row comes from: lowering a callee's signature at a call (`Lower::Args` with
//! `Cause::Expr(call)`: rules/call.rs, rules/method.rs) turns each written projection into a
//! fresh variable and a projection obligation (machine.rs `lower_at`, the `PNode::Alias` arm);
//! the same arm notes `(call, trait, name, args)`. At writeback the arguments are resolved and
//! exported, and when they are ground the impl is chosen by the one selection home for ground
//! arguments, `instance::select_in` (the selection the code builder's `resolve_instance` makes),
//! over the BODY's own `Reads`: every world fact it consults is on the body's read list (rule
//! 21), and an impl's facts were already read by the solver when it normalized the obligation.
//!
//! The Reference, "Implementations", "Trait implementations": at a call with type arguments
//! fixed, "the" implementation of `Tr` for `T` is the one that applies (coherence makes it
//! unique; `min_specialization` picks the most specific: instance.rs); "Associated Items",
//! "Associated types": its `type Out = U;` defines `<T as Tr>::Out` as `U`.
//!
//! Bound rows (`name: None`). rules/call.rs `record_lifetime_impls` notes every trait clause
//! of a called fn (any subject, any trait, any arity) instantiated at the call, unless the
//! written clause holds a projection ([`holds_alias`]: lowering it again would file a second
//! projection obligation and a second named row). Writeback keeps one only where its trait
//! relates something (not `Sized`, `MetaSized`, `PointeeSized`, `Destruct`) and its resolved
//! instance may hold a region or a caller's type parameter ([`may_relate`]): the Reference,
//! "Trait and lifetime bounds": "Bounds on an item must be satisfied when using the item", and
//! "Implementations": a where-clause holds at the instance the use makes, through the impl
//! that applies there, so `need::<&'x u8>` under `T: Marker` with `impl Marker for &'static u8`
//! makes `'x: 'static` though `Marker` names no lifetime. A row selected `Impl` brings the
//! impl's own where-clauses at the impl's arguments as further bound rows at the same call
//! (after the call's own; the same filter; nested to [`MOST_ROW_NESTING`]): an impl holds only
//! where its where-clauses do (`impl<T: Clone> Clone for Vec<T>` at `Vec<&'x u8>` asks
//! `&'x u8: Clone`).

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Limit};
use crate::ids::{ExprId, ItemKey, List, Name};
use crate::portable::{PNode, PTy, PTys};

/// How one projection of a callee's signature was decided at one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selected {
    /// The impl `imp` at its arguments `args` (a list of the record's `types`): its own
    /// `type Name` member is the projection's value at this call.
    Impl { imp: ItemKey, args: List },
    /// The projection's arguments name the body's own generics (or an alias or opaque type):
    /// a where-clause of the body's environment or a generic impl decides it per instance, not
    /// the call. Also a ground row an impl holds for where a where-clause of the body may be
    /// the candidate that applies instead (one naming no type parameter outside a projection,
    /// such as `&'a u8: Marker`, of the row's trait or a subtrait of it: rustc prefers such a
    /// param-env candidate over an impl, and the erased arguments cannot tell its lifetimes
    /// from the impl's). The borrow check keeps refusing such a call by name.
    Generic,
    /// The arguments are ground, no impl holds, and a where-clause of the body may answer the
    /// row (as for [`Selected::Generic`]): the solver proved it by a builtin rule or that
    /// where-clause; no impl text gives its value.
    NotImpl,
    /// Selecting needed a world fact the check had not read (`ResolveError::Missing`); not
    /// decided here, never guessed: the borrow check refuses the call by name.
    Unanswered,
    /// `Self` is the body's own type parameter and the body's where-clause `clause` proves the
    /// row: its index among the owner's predicates (`World::predicates`, the order
    /// blaze-ladder heads_regions.rs `fill_clauses` numbers them, `ClauseView::nth`). For a
    /// projection row, a projection clause `T: Tr<Name = U>` is named where the owner has
    /// exactly one; else its one trait clause `T: Tr<..>`. Two candidates, or none, stay
    /// [`Selected::Generic`] (rustc's param-env candidate wins over impls only when it is the
    /// one that applies; deciding between two needs their arguments compared).
    ParamEnv { clause: u32 },
    /// `Self` is an opaque type: its declared bounds prove the row.
    OpaqueBound,
    /// `Self` is a `dyn` type: the object's own predicates prove the row.
    ObjectBound,
    /// Ground, no written impl, and no where-clause of the caller can answer it (the caller
    /// assumes only `Sized`, the row's `global_env`, or each of its clauses names a type
    /// parameter outside a projection, which no ground goal unifies with): a builtin rule
    /// proves it (closures' `Fn*`, `Sized`, `Copy`/`Clone` of tuples, arrays, fn pointers,
    /// `Unsize`, auto traits; the Reference, "Special types and traits").
    Builtin,
}

/// One projection of a callee's signature at one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Projected {
    /// The call or method call.
    pub expr: ExprId,
    /// Its ordinal among this call's rows, in the order the signature was lowered (the inputs
    /// left to right, then the output; within one written type, as `lower_at` meets it): the
    /// order the borrow check's reader numbers a signature's hidden positions (`Region::Hidden`).
    pub at: u32,
    pub trait_: ItemKey,
    /// The associated type, or `None` for a trait bound without a projection.
    pub name: Option<Name>,
    /// `[Self, trait args..]` at the call, a list of the record's `types`.
    pub args: List,
    /// The type checker established that the caller assumes only built-in Sized predicates.
    pub global_env: bool,
    pub selected: Selected,
}

/// The most rows one body keeps (rule 18): past it the check stops with `Limit::Solver`.
pub(crate) const MOST_PROJECTED: usize = 1 << 16;

/// The most nodes the groundness walk (rules/writeback/selected_rows.rs) visits in one row's
/// arguments (rule 18): past it the check stops with `Limit::Solver`.
pub(crate) const MOST_NODES: u32 = 1 << 16;

/// How deep a selected impl's where-clauses are filed as further rows (an impl's clause
/// selected through another impl ..; rule 18): blaze-borrow relate.rs `MOST_IMPL_NESTING`,
/// the depth the borrow check relates them to. A clause past it gets no row, and the borrow
/// check refuses its tie by name (no row is never a decided answer).
pub(crate) const MOST_ROW_NESTING: u32 = 32;

/// Whether a bound row's instance `roots` (`[Self, trait args..]`, resolved and exported) may
/// hold a region or a caller's type parameter: the filter that keeps a bound row
/// (`name: None`). A row that may not relates nothing at the call and is not kept.
///
/// - No: `bool`, `char`, the numbers, `str`, `!`, a const (a parameter or a value), an auto
///   trait row of an object; and a tuple, array, slice, raw pointer or pattern type of those.
/// - May: a reference or a trait object (a region by their syntax), a type parameter (the
///   caller's), and every nominal type whose lifetime parameters typeck does not see: an ADT,
///   a fn item, a fn pointer (a `for<'a>` binder), a closure, a coroutine, an async block's
///   future, an opaque type, a projection, an error.
///
/// Typeck keeps no ADT's lifetime arity (portable types erase regions; D148), so `Vec<u8>`
/// is kept as `Vec<&u8>` is; the borrow check's `inert` drops a row whose instance holds no
/// position (blaze-borrow relate.rs).
pub(crate) fn may_relate(tys: PTys<'_>, roots: &[PTy], alloc: Alloc<'_>) -> Result<bool, CheckError> {
    let mut stack: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    stack.extend_from_slice(roots);
    let mut steps = 0u32;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let node = tys.node(t)?;
        match node {
            PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never
            | PNode::ConstParam(_) | PNode::ConstValue { .. } | PNode::ExAuto(_) => {}
            // An extern type has no generics, so no region.
            PNode::Foreign(_) => {}
            PNode::Tuple(_) | PNode::Record(..) | PNode::Union(_) | PNode::Array(..) | PNode::Slice(_) | PNode::Ptr(..) | PNode::Pat(..) => tys.children_of(node, &mut stack)?,
            PNode::Ref(..) | PNode::Dynamic(_) | PNode::Param(_) | PNode::Adt(..) | PNode::FnDef(..) | PNode::FnPtr(..)
            | PNode::Closure { .. } | PNode::Coroutine { .. } | PNode::Future(..) | PNode::Opaque(..) | PNode::Alias { .. }
            | PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::Infer | PNode::Error => return Ok(true),
        }
    }
    Ok(false)
}

/// Whether the list `l` of `tys` (a where-clause's written arguments) holds a projection
/// anywhere: such a clause is not noted as a bound row (rules/call.rs `record_lifetime_impls`,
/// and an impl's where-clause at writeback). Lowering it at the call again would register a
/// second projection obligation and file a second named row at the call, past the count the
/// borrow check numbers hidden positions by; its tie is refused by name downstream (no row).
pub(crate) fn holds_alias(tys: PTys<'_>, l: List, alloc: Alloc<'_>) -> Result<bool, CheckError> {
    let mut stack: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    stack.extend_from_slice(tys.list(l)?);
    let mut steps = 0u32;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return Err(CheckError::Limit(Limit::Solver));
        }
        let node = tys.node(t)?;
        if matches!(node, PNode::Alias { .. }) {
            return Ok(true);
        }
        tys.children_of(node, &mut stack)?;
    }
    Ok(false)
}
