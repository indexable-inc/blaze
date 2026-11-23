//! Whether a ground type needs drop: the one home of the rule for whoever asks it without a
//! layout at hand (the constant evaluator's `needs_drop` intrinsic; the builder's walk reads the
//! same fact off the layouts it already made, `Layout::needs_drop`, by the same rule).
//!
//! The rule (the Reference, "Destructors": when a value is dropped, "If `T: Drop`, calling
//! `<T as core::ops::Drop>::drop`", then "If `T` is a struct, drop each field ... If `T` is an
//! enum, drop the fields of the active variant ... If `T` is a tuple, drop each element ... If
//! `T` is an array, drop each element ... If `T` is a slice, drop each element ... If `T` is a
//! closure, drop each captured variable ... If `T` is a trait object, run the destructor of the
//! underlying type ... any other type: nothing"; "Unions": "Union fields are never dropped";
//! core's `ManuallyDrop<T>` (`#[lang = "manually_drop"]`) "inhibits the compiler from
//! automatically calling `T`'s destructor"). The pinned core, library/core/src/intrinsics/
//! mod.rs, `needs_drop`: "Returns `true` if the actual type given as `T` requires drop glue";
//! library/core/src/mem/mod.rs: it "may return `true` for types that don't actually need to be
//! dropped" but never `false` for one that does. So: a type needs drop when it has a `Drop`
//! impl, is a trait object (its value's type is not known) or a coroutine (its drop reads the
//! state), or when a part it drops (a field of any variant, an element, a capture) needs drop;
//! `[T; 0]` holds no element and needs drop only by its own impl, of which an array has none.
//!
//! Sans-IO: the `Drop` impl answer is the caller's ([`crate::Adts::drop_impl`]'s question), the
//! ADTs' and closures' definitions are [`LayoutFacts`]. A work list, no recursion; every ground
//! type is visited at most once, and a walk past [`SEEN_CAP`] distinct types is refused.

use core::fmt;

use blaze_own::{Alloc, Vec};
use blaze_typeck::{AdtKind, CheckError, ItemKey, PNode, PTy, PortableTys};

use crate::BuildError;
use crate::instance::ground;
use crate::layout::LayoutFacts;

/// The most distinct ground types one question visits (rule 18: a table has a cap). A real
/// type reaches a few hundred; the cap only bounds a pathological generic nest.
pub const SEEN_CAP: usize = 1 << 16;

/// Why [`needs_drop`] gave no answer: each names the type or item it stopped at.
#[derive(Debug)]
pub enum NeedsDropError<E> {
    /// The caller's `Drop` impl question failed (a missing record, a solver error).
    Ask(E),
    /// Grounding a field's or a capture's type failed (a projection the solver does not answer).
    Ground(BuildError),
    /// A type that is not ground where a ground one is required: a generic parameter, a
    /// projection, an opaque type, a hole or an error, by its form's name.
    NotGround { what: &'static str, ty: PTy },
    /// An array whose length is not a ground `usize` value.
    ArrayLength { ty: PTy },
    /// An ADT the facts do not define.
    NoAdt { adt: ItemKey },
    /// A closure whose facts the world did not answer.
    NoClosure { owner: ItemKey, ordinal: u32 },
    /// A type index past the ground table, or a list past it.
    Malformed { what: &'static str },
    /// More than [`SEEN_CAP`] distinct types visited.
    TooManyTypes { cap: usize },
}

impl<E: fmt::Display> fmt::Display for NeedsDropError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const RULE: &str = "the Reference, \"Destructors\"";
        match self {
            NeedsDropError::Ask(e) => write!(f, "`needs_drop`: the `Drop` impl question failed: {e}"),
            NeedsDropError::Ground(e) => write!(f, "`needs_drop`: a part's type: {e}"),
            NeedsDropError::NotGround { what, ty } => {
                write!(f, "`needs_drop` of type {} reached {what}, which is not a ground type ({RULE})", ty.0)
            }
            NeedsDropError::ArrayLength { ty } => write!(f, "`needs_drop` of array type {} whose length is not a ground `usize`", ty.0),
            NeedsDropError::NoAdt { adt } => write!(f, "`needs_drop`: the facts define no ADT {:#x}", adt.0),
            NeedsDropError::NoClosure { owner, ordinal } => {
                write!(f, "`needs_drop`: no facts for closure {ordinal} of {:#x}", owner.0)
            }
            NeedsDropError::Malformed { what } => write!(f, "`needs_drop`: a malformed type table: {what}"),
            NeedsDropError::TooManyTypes { cap } => write!(f, "`needs_drop` visited more than {cap} distinct types"),
        }
    }
}

/// Whether ground type `ty` of `ground` needs drop (the module doc's rule). `manually_drop` is
/// core's `ManuallyDrop` ADT (`#[lang = "manually_drop"]`), `None` when the library declares
/// none; `drop_impl` answers whether a ground ADT type has a `Drop` impl (its `drop` fn, as
/// [`crate::Adts::drop_impl`]). `scratch` holds the work list and the visited set.
pub fn needs_drop<'g, E>(
    ty: PTy,
    ground_tys: &mut PortableTys<'g>,
    facts: &dyn LayoutFacts,
    manually_drop: Option<ItemKey>,
    drop_impl: &mut dyn FnMut(PTy, &mut PortableTys<'g>) -> Result<Option<ItemKey>, E>,
    scratch: Alloc<'_>,
) -> Result<bool, NeedsDropError<E>> {
    let malformed = |what: &'static str| move |_: CheckError| NeedsDropError::Malformed { what };
    let mut todo: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    let mut seen: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    let mut args: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    todo.push(ty);
    while let Some(t) = todo.pop() {
        if seen.contains(&t) {
            continue;
        }
        if seen.len() >= SEEN_CAP {
            return Err(NeedsDropError::TooManyTypes { cap: SEEN_CAP });
        }
        seen.push(t);
        let node = ground_tys.view().node(t).map_err(malformed("a type past the ground table"))?;
        match node {
            // Nothing to drop: "any other type".
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Ref(..)
            | PNode::Ptr(..)
            | PNode::FnDef(..)
            | PNode::FnPtr(..) => {}
            // An extern type has no drop glue: it is never owned by value (no size), and
            // what it holds is foreign code's to free.
            PNode::Foreign(_) => {}
            // A pattern type's values are its base's (an integer, `char` or raw pointer).
            PNode::Pat(base, _) => todo.push(base),
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                todo.extend_from_slice(ground_tys.view().list(l).map_err(malformed("a tuple's list past the ground table"))?);
            }
            PNode::Slice(elem) => todo.push(elem),
            PNode::Array(elem, len) => match ground_tys.view().node(len).map_err(malformed("an array length past the ground table"))? {
                PNode::ConstValue { bits: 0, .. } => {}
                PNode::ConstValue { .. } => todo.push(elem),
                _ => return Err(NeedsDropError::ArrayLength { ty: t }),
            },
            // The underlying type is not known: its destructor may do anything.
            PNode::Dynamic(_) => return Ok(true),
            // A coroutine's drop is its pass's `drop`, which reads the state's tag.
            PNode::Coroutine { .. } => return Ok(true),
            PNode::Future(..) => {
                return Err(NeedsDropError::Ground(BuildError::UnsupportedSig {
                    what: "an async block's future named by its expression in a ground type (only its identity, `PNode::Coroutine`, crosses a body: coroutine.rs)",
                }));
            }
            PNode::Closure { owner, ordinal, args: l } => {
                let c = facts.closure(owner, ordinal).ok_or(NeedsDropError::NoClosure { owner, ordinal })?;
                args.clear();
                args.extend_from_slice(ground_tys.view().list(l).map_err(malformed("a closure's arguments past the ground table"))?);
                let env = ground(c.types, c.env, &args, ground_tys, scratch, facts).map_err(NeedsDropError::Ground)?;
                todo.push(env);
            }
            PNode::Adt(k, l) => {
                if drop_impl(t, ground_tys).map_err(NeedsDropError::Ask)?.is_some() {
                    return Ok(true);
                }
                // "Union fields are never dropped"; nor is `ManuallyDrop`'s value.
                if manually_drop == Some(k) {
                    continue;
                }
                let adt = facts.adt(k).ok_or(NeedsDropError::NoAdt { adt: k })?;
                if adt.kind == AdtKind::Union {
                    continue;
                }
                args.clear();
                args.extend_from_slice(ground_tys.view().list(l).map_err(malformed("an ADT's arguments past the ground table"))?);
                if adt.generics as usize != args.len() {
                    return Err(NeedsDropError::Malformed { what: "an ADT type whose argument count is not its ADT's generic count" });
                }
                for variant in adt.variants {
                    let fields = adt.fields.get(variant.fields.range()).ok_or(NeedsDropError::Malformed { what: "an ADT's fields past their column" })?;
                    for f in fields {
                        let ft = ground(adt.types, f.ty, &args, ground_tys, scratch, facts).map_err(NeedsDropError::Ground)?;
                        todo.push(ft);
                    }
                }
            }
            PNode::Param(_) => return Err(NeedsDropError::NotGround { what: "a generic parameter", ty: t }),
            PNode::ConstParam(_) => return Err(NeedsDropError::NotGround { what: "a const parameter", ty: t }),
            PNode::Alias { .. } => return Err(NeedsDropError::NotGround { what: "a projection", ty: t }),
            PNode::Opaque(..) => return Err(NeedsDropError::NotGround { what: "an opaque type", ty: t }),
            PNode::Infer => return Err(NeedsDropError::NotGround { what: "a hole (`_`)", ty: t }),
            PNode::Error => return Err(NeedsDropError::NotGround { what: "the error type", ty: t }),
            PNode::ConstValue { .. } => return Err(NeedsDropError::NotGround { what: "a const value", ty: t }),
            PNode::ExTrait(..) | PNode::ExProj { .. } | PNode::ExAuto(_) => {
                return Err(NeedsDropError::NotGround { what: "a trait object's predicate", ty: t });
            }
        }
    }
    Ok(false)
}
