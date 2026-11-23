//! Coroutine layouts as DEMANDED records (ASYNC.md D-A8, prompt P8): before an instance is
//! built, the coroutines its ground types hold BY VALUE are listed, and their states are made
//! first, in dependency order, so the build never meets a state that is not made yet (no
//! "Missing, build, build again").
//!
//! - [`held_coroutines`]: a work list over ground types, through what a value holds inline
//!   (tuple fields, array elements, an ADT's fields of every variant (a union's too), a
//!   closure's environment, a pattern type's base), never through a pointer, a reference, a
//!   slice or a trait object (their pointee is not part of the value). A coroutine met is
//!   listed and NOT entered: what its own state holds is [`layout_order`]'s question, asked of
//!   that coroutine's own types.
//! - [`layout_order`]: the coroutines one instance demands, each after every coroutine its
//!   state holds, by a stack (no recursion). A coroutine reached again while it is still on the
//!   stack holds itself by value: the Reference, "Async functions", and rustc's E0733
//!   ("recursion in an async fn requires boxing"); refused by name, never laid out.
//!
//! Layout of coroutines is not specified by the Reference; that a state holds every future it
//! keeps across an `.await` by value is ("Async blocks": the future is "roughly equivalent to
//! an enum with one variant per await point", each variant holding what is live there).

use blaze_build::layout::LayoutFacts;
use blaze_build::{BuildError, CoroutineOf, coroutine_of};
use blaze_own::{Alloc, Vec};
use blaze_typeck::{PNode, PTy, PortableTys};

/// The refusal of a coroutine whose state holds itself by value.
pub(crate) const HOLDS_ITSELF: &str = "a coroutine whose state holds itself (E0733: recursion in an async fn requires boxing)";

/// The deepest chain of coroutines held one inside another by value that one demand lays out,
/// and the most coroutines it lists (rule 18): past it the demand is refused by name.
pub(crate) const MOST_HELD: usize = 4096;

/// Each coroutine type `roots` (types of `ground`) hold by value, deduplicated, in first-met
/// order. ADT fields and closure environments are read through `facts` and grounded into
/// `ground` at the type's own arguments (`blaze_build::ground`, the one grounding, which also
/// normalizes a field's projection). An ADT whose row is not made is `BuildError::NoAdt` (a
/// need the lazy program fills, then the instance is built again). Refused by name: a closure
/// whose facts the world does not answer, and a node a ground type never holds (a parameter, a hole, an
/// alias, an opaque type, a future still named by its expression).
pub(crate) fn held_coroutines<'a>(
    ground: &mut PortableTys<'_>,
    roots: &[PTy],
    facts: &dyn LayoutFacts,
    alloc: Alloc<'a>,
) -> Result<Vec<CoroutineOf, Alloc<'a>>, BuildError> {
    let past = |_| BuildError::Malformed { what: "a type past the ground table (held_coroutines)" };
    let mut out: Vec<CoroutineOf, Alloc<'a>> = Vec::new_in(alloc);
    let mut todo: Vec<PTy, Alloc<'a>> = Vec::with_capacity_in(roots.len(), alloc);
    todo.extend_from_slice(roots);
    // Every ground type entered, sorted: each is entered once.
    let mut seen: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    let mut args: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    while let Some(t) = todo.pop() {
        match seen.binary_search(&t) {
            Ok(_) => continue,
            Err(at) => seen.insert_shift(at, t),
        }
        let node = ground.view().node(t).map_err(past)?;
        match node {
            // Nothing held inline past the value's own bytes: scalars, `str` and slices
            // (unsized), pointers and references (the pointee is elsewhere), fn items (no
            // bytes), fn pointers, trait objects and their parts, a constant's value.
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Ref(..)
            | PNode::Ptr(..)
            | PNode::Slice(_)
            | PNode::FnDef(..)
            | PNode::FnPtr(..)
            | PNode::Dynamic(_)
            | PNode::ExTrait(..)
            | PNode::ExProj { .. }
            | PNode::ExAuto(_)
            // An extern type: no fields, and never held by value (it has no size).
            | PNode::Foreign(_)
            | PNode::ConstValue { .. } => {}
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => todo.extend_from_slice(ground.view().list(l).map_err(past)?),
            // The element; the length is a constant.
            PNode::Array(element, _) => todo.push(element),
            // A pattern type is held as its base.
            PNode::Pat(base, _) => todo.push(base),
            PNode::Adt(key, l) => {
                // An ADT whose row is not made is the build's `NoAdt`, the one form of that
                // need: a lazy program makes the row and the instance is built again.
                let adt = facts.adt(key).ok_or(BuildError::NoAdt { adt: key })?;
                args.clear();
                args.extend_from_slice(ground.view().list(l).map_err(past)?);
                // `Adt::fields` holds every variant's fields: an enum holds each by value in
                // its variant, a union each in its one place.
                for field in adt.fields {
                    let g = blaze_build::ground(adt.types, field.ty, &args, ground, alloc, facts)?;
                    todo.push(g);
                }
            }
            PNode::Closure { owner, ordinal, args: l } => {
                let closure = facts.closure(owner, ordinal).ok_or(BuildError::UnsupportedSig {
                    what: "a closure whose facts the world did not answer (`LayoutFacts::closure`), met while listing the coroutines a value holds",
                })?;
                args.clear();
                args.extend_from_slice(ground.view().list(l).map_err(past)?);
                let env = blaze_build::ground(closure.types, closure.env, &args, ground, alloc, facts)?;
                todo.push(env);
            }
            PNode::Coroutine { .. } => {
                let of = coroutine_of(ground.view(), t).ok_or(BuildError::Malformed {
                    what: "a coroutine node `coroutine_of` does not read (held_coroutines)",
                })?;
                if !out.iter().any(|o| *o == of) {
                    if out.len() >= MOST_HELD {
                        return Err(BuildError::UnsupportedSig { what: "a value holding more than 4096 coroutines by value" });
                    }
                    out.push(of);
                }
            }
            PNode::Param(_)
            | PNode::ConstParam(_)
            | PNode::Alias { .. }
            | PNode::Opaque(..)
            | PNode::Future(..)
            | PNode::Infer
            | PNode::Error => {
                return Err(BuildError::Malformed {
                    what: "a parameter, alias, opaque type, hole or future named by its expression in a ground type (held_coroutines)",
                });
            }
        }
    }
    Ok(out)
}

/// One coroutine on [`layout_order`]'s stack: what its state holds, and the next of those to
/// visit.
struct Frame<'a> {
    of: CoroutineOf,
    held: Vec<CoroutineOf, Alloc<'a>>,
    next: usize,
}

/// The coroutines whose states must be made before an instance is built, in dependency order
/// (a state that holds another comes after it), each once: `roots` are the coroutines the
/// instance's own types hold by value ([`held_coroutines`]; a coroutine instance lists ITSELF
/// first, so a state holding itself is found). `held_by` lists the coroutines one coroutine's
/// state holds by value, from that coroutine's own types, or answers `None` when its state is
/// made already (it is then neither listed nor entered). Refused by name: a coroutine reached
/// while it is on the stack ([`HOLDS_ITSELF`], E0733), and a chain or a list past [`MOST_HELD`].
pub(crate) fn layout_order<'a>(
    roots: &[CoroutineOf],
    held_by: &mut dyn FnMut(CoroutineOf) -> Result<Option<Vec<CoroutineOf, Alloc<'a>>>, BuildError>,
    alloc: Alloc<'a>,
) -> Result<Vec<CoroutineOf, Alloc<'a>>, OrderError<'a>> {
    let mut order: Vec<CoroutineOf, Alloc<'a>> = Vec::new_in(alloc);
    let mut stack: Vec<Frame<'a>, Alloc<'a>> = Vec::new_in(alloc);
    for &root in roots {
        if order.iter().any(|o| *o == root) {
            continue;
        }
        let Some(held) = held_by(root).map_err(OrderError::Build)? else { continue };
        stack.push(Frame { of: root, held, next: 0 });
        while let Some(top) = stack.last_mut() {
            let Some(&child) = top.held.get(top.next) else {
                // Every coroutine its state holds is ordered: this one is next.
                let of = top.of;
                stack.pop();
                if order.len() >= MOST_HELD {
                    return Err(OrderError::Build(BuildError::UnsupportedSig { what: "more than 4096 coroutine states demanded by one instance" }));
                }
                order.push(of);
                continue;
            };
            top.next += 1;
            if let Some(at) = stack.iter().position(|f| f.of == child) {
                // The cycle, item by item: from the first frame that is `child` to the top,
                // then `child` again.
                let mut cycle: Vec<CoroutineOf, Alloc<'a>> = Vec::with_capacity_in(stack.len() - at + 1, alloc);
                cycle.extend(stack[at..].iter().map(|f| f.of));
                cycle.push(child);
                return Err(OrderError::HoldsItself(cycle));
            }
            if order.iter().any(|o| *o == child) {
                continue;
            }
            if stack.len() >= MOST_HELD {
                return Err(OrderError::Build(BuildError::UnsupportedSig { what: "more than 4096 futures held one inside another by value" }));
            }
            let Some(held) = held_by(child).map_err(OrderError::Build)? else { continue };
            stack.push(Frame { of: child, held, next: 0 });
        }
    }
    Ok(order)
}

/// Why [`layout_order`] made no order.
pub(crate) enum OrderError<'a> {
    /// A state holds itself by value ([`HOLDS_ITSELF`], E0733): the coroutines of the cycle in
    /// the order each holds the next, the first repeated last. The caller names each by its
    /// owner fn's path.
    HoldsItself(Vec<CoroutineOf, Alloc<'a>>),
    /// A refusal of `held_by`, or a bound past [`MOST_HELD`].
    Build(BuildError),
}
