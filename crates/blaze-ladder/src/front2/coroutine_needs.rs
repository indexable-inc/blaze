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
    collect_coroutines(ground, roots, facts, false, alloc)
}

/// Each coroutine type `roots` hold by value OR reference through pointers, references, slices,
/// fn items/pointers, trait objects, or generic type arguments, deduplicated, in first-met order.
///
/// Used when collecting an instance's root coroutines before building (`roots_of_instance`) and
/// before constant evaluation (`const_at::pull`), where functions like `size_of_val_raw::<C>`,
/// `Pin<&mut Timeout<C>>::project`, `MaybeDone::<C>::output_mut`, or `drop_in_place::<Wrapper<C>>`
/// query `C`'s layout even though `C` is behind a pointer/reference or only in type arguments.
/// [`held_coroutines`] remains strictly by-value for [`layout_order`]'s E0733 cycle check.
pub(crate) fn referenced_coroutines<'a>(
    ground: &mut PortableTys<'_>,
    roots: &[PTy],
    facts: &dyn LayoutFacts,
    alloc: Alloc<'a>,
) -> Result<Vec<CoroutineOf, Alloc<'a>>, BuildError> {
    collect_coroutines(ground, roots, facts, true, alloc)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Walk {
    /// Syntactic-only scan of type constructors and generic arguments (never calls `facts.adt`
    /// or `facts.closure`, guaranteeing termination on recursive pointer ADTs and avoiding huge
    /// transitive `Arc<...>` / `Box<...>` / `Vec<...>` / `&Provider` graph walks).
    Syntactic = 0,
    /// By-value traversal from the root types.
    Inline = 1,
}

fn is_lazy_need(err: &BuildError) -> bool {
    matches!(
        err,
        BuildError::NoAdt { .. }
            | BuildError::MissingRecord { .. }
            | BuildError::OpaqueOwnerUnchecked { .. }
            | BuildError::Select(_)
    )
}

/// Fast local check on an ADT's type table: returns `true` only if `ty` (within `types`,
/// without touching `facts` or the trait solver) directly mentions an `Opaque`, `Coroutine`,
/// or `Future` node. Used so `Walk::Syntactic` behind `&T`/`*mut T` still catches non-generic
/// TAIT coroutine fields without expanding arbitrary `Provider`/`ArcInner`/`BTreeNode` structs.
fn adt_field_mentions_opaque_or_coro(types: blaze_typeck::PTys<'_>, ty: PTy) -> bool {
    let mut stack = [PTy(0); 32];
    let mut len = 1usize;
    stack[0] = ty;
    while len > 0 {
        len -= 1;
        let cur = stack[len];
        let Ok(node) = types.node(cur) else { continue };
        match node {
            PNode::Opaque(..) | PNode::Coroutine { .. } | PNode::Future(..) => return true,
            PNode::Tuple(l)
            | PNode::Record(_, l)
            | PNode::Union(l)
            | PNode::Adt(_, l)
            | PNode::Closure { args: l, .. }
            | PNode::FnDef(_, l)
            | PNode::FnPtr(l, _)
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => {
                if let Ok(elems) = types.list(l) {
                    for &e in elems {
                        if len < stack.len() {
                            stack[len] = e;
                            len += 1;
                        }
                    }
                }
            }
            PNode::Array(inner, _) | PNode::Pat(inner, _) | PNode::Ref(_, inner) | PNode::Ptr(_, inner) | PNode::Slice(inner) => {
                if len < stack.len() {
                    stack[len] = inner;
                    len += 1;
                }
            }
            _ => {}
        }
    }
    false
}

fn collect_coroutines<'a>(
    ground: &mut PortableTys<'_>,
    roots: &[PTy],
    facts: &dyn LayoutFacts,
    through_ptrs: bool,
    alloc: Alloc<'a>,
) -> Result<Vec<CoroutineOf, Alloc<'a>>, BuildError> {
    let past = |_| BuildError::Malformed { what: "a type past the ground table (held_coroutines)" };
    let mut out: Vec<CoroutineOf, Alloc<'a>> = Vec::new_in(alloc);
    let mut todo: Vec<(PTy, Walk), Alloc<'a>> = Vec::with_capacity_in(roots.len(), alloc);
    for &r in roots {
        todo.push((r, Walk::Inline));
    }
    // Every ground type entered, sorted by `PTy`, storing the strongest `Walk` mode already run.
    let mut seen: Vec<(PTy, Walk), Alloc<'a>> = Vec::new_in(alloc);
    let mut args: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    while let Some((t, mode)) = todo.pop() {
        match seen.binary_search_by_key(&t, |&(ty, _)| ty) {
            Ok(at) => {
                if seen[at].1 >= mode {
                    continue;
                }
                seen[at].1 = mode;
            }
            Err(at) => seen.insert_shift(at, (t, mode)),
        }
        let node = ground.view().node(t).map_err(past)?;
        match node {
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::ExAuto(_)
            // An extern type: no fields, and never held by value (it has no size).
            | PNode::Foreign(_)
            | PNode::ConstValue { .. } => {}
            PNode::Ref(_, inner) | PNode::Ptr(_, inner) | PNode::Slice(inner) => {
                if through_ptrs {
                    todo.push((inner, Walk::Syntactic));
                    // For a direct pointer/reference to a non-generic ADT holding a TAIT
                    // (`Opaque`) coroutine field (e.g. `Pin<&mut ConcreteHolder>`), ground ONLY
                    // fields whose own ADT definition mentions `Opaque`/`Coroutine`/`Future`.
                    // Ordinary data structs (`Provider`, `ArcInner`, `BTreeNode`, `Timeout<F>`)
                    // never pay any `blaze_build::ground` or trait-solver cost here.
                    if mode == Walk::Inline
                        && let Ok(PNode::Adt(key, l)) = ground.view().node(inner)
                        && let Some(adt) = facts.adt(key)
                    {
                        args.clear();
                        args.extend_from_slice(ground.view().list(l).map_err(past)?);
                        for field in adt.fields {
                            if !adt_field_mentions_opaque_or_coro(adt.types, field.ty) {
                                continue;
                            }
                            match blaze_build::ground(adt.types, field.ty, &args, ground, alloc, facts) {
                                Ok(g) => todo.push((g, Walk::Syntactic)),
                                Err(e) if !is_lazy_need(&e) => {}
                                Err(e) => return Err(e),
                            }
                        }
                    }
                }
            }
            PNode::FnDef(_, l) | PNode::FnPtr(l, _) | PNode::Dynamic(l) | PNode::ExTrait(_, l) => {
                if through_ptrs {
                    for &elem in ground.view().list(l).map_err(past)? {
                        todo.push((elem, Walk::Syntactic));
                    }
                }
            }
            PNode::ExProj { args: l, term, .. } => {
                if through_ptrs {
                    todo.push((term, Walk::Syntactic));
                    for &elem in ground.view().list(l).map_err(past)? {
                        todo.push((elem, Walk::Syntactic));
                    }
                }
            }
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                for &elem in ground.view().list(l).map_err(past)? {
                    todo.push((elem, mode));
                }
            }
            // The element; the length is a constant.
            PNode::Array(element, _) => todo.push((element, mode)),
            // A pattern type is held as its base.
            PNode::Pat(base, _) => todo.push((base, mode)),
            PNode::Adt(key, l) => {
                args.clear();
                args.extend_from_slice(ground.view().list(l).map_err(past)?);
                if through_ptrs {
                    for &arg in args.iter() {
                        todo.push((arg, Walk::Syntactic));
                    }
                }
                if mode == Walk::Syntactic {
                    continue;
                }
                // An ADT whose row is not made is the build's `NoAdt`, the one form of that
                // need: a lazy program makes the row and the instance is built again.
                let adt = facts.adt(key).ok_or(BuildError::NoAdt { adt: key })?;
                // `Adt::fields` holds every variant's fields: an enum holds each by value in
                // its variant, a union each in its one place.
                for field in adt.fields {
                    // Fast-path primitive fields, direct generic parameters, and pointers when
                    // `!through_ptrs` without allocating or invoking `blaze_build::ground`.
                    match adt.types.node(field.ty) {
                        Ok(
                            PNode::Bool
                            | PNode::Char
                            | PNode::Int(_)
                            | PNode::Uint(_)
                            | PNode::Float(_)
                            | PNode::Str
                            | PNode::Never
                            | PNode::ExAuto(_)
                            | PNode::Foreign(_)
                            | PNode::ConstValue { .. },
                        ) => continue,
                        Ok(PNode::Ref(..) | PNode::Ptr(..) | PNode::Slice(_)) if !through_ptrs => continue,
                        Ok(PNode::Param(idx)) => {
                            if let Some(&arg) = args.get(idx as usize) {
                                todo.push((arg, mode));
                                continue;
                            }
                        }
                        _ => {}
                    }
                    let g = blaze_build::ground(adt.types, field.ty, &args, ground, alloc, facts)?;
                    todo.push((g, mode));
                }
            }
            PNode::Closure { owner, ordinal, args: l } => {
                args.clear();
                args.extend_from_slice(ground.view().list(l).map_err(past)?);
                if through_ptrs {
                    for &arg in args.iter() {
                        todo.push((arg, Walk::Syntactic));
                    }
                }
                if mode == Walk::Syntactic {
                    continue;
                }
                let Some(closure) = facts.closure(owner, ordinal) else {
                    return Err(BuildError::UnsupportedSig {
                        what: "a closure whose facts the world did not answer (`LayoutFacts::closure`), met while listing the coroutines a value holds",
                    });
                };
                let env = blaze_build::ground(closure.types, closure.env, &args, ground, alloc, facts)?;
                todo.push((env, mode));
            }
            PNode::Coroutine { args: l, .. } => {
                let of = coroutine_of(ground.view(), t).ok_or(BuildError::Malformed {
                    what: "a coroutine node `coroutine_of` does not read (held_coroutines)",
                })?;
                if !out.iter().any(|o| *o == of) {
                    if out.len() >= MOST_HELD {
                        return Err(BuildError::UnsupportedSig { what: "a value holding more than 4096 coroutines by value" });
                    }
                    out.push(of);
                }
                if through_ptrs {
                    for &arg in ground.view().list(l).map_err(past)? {
                        todo.push((arg, Walk::Syntactic));
                    }
                }
            }
            PNode::Param(_)
            | PNode::ConstParam(_)
            | PNode::Alias { .. }
            | PNode::Opaque(..)
            | PNode::Future(..)
            | PNode::Infer
            | PNode::Error => {
                if mode != Walk::Inline {
                    continue;
                }
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

#[cfg(test)]
mod tests {
    use blaze_build::layout::{ClosureFacts, LayoutFacts, Missing, ReprAttrs, ValidRange};
    use blaze_build::{CoroutineFacts, StateShape};
    use blaze_mem::Frame as MemFrame;
    use blaze_typeck::{Adt, ItemKey, Mutbl, Name, PNode, PTy, PTys, PortableTys};

    use super::*;

    struct EmptyFacts;

    impl LayoutFacts for EmptyFacts {
        fn adt(&self, _key: ItemKey) -> Option<Adt<'_>> { None }
        fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> { Err(Missing { item: key }) }
        fn discriminant(&self, adt: ItemKey, _variant: u32) -> Result<Option<i128>, Missing> { Err(Missing { item: adt }) }
        fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> { Err(Missing { item: key }) }
        fn normalize(&self, _trait_: ItemKey, _name: Name, _args: &[PTy], _ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
            Err(BuildError::Malformed { what: "unused" })
        }
        fn closure(&self, _owner: ItemKey, _ordinal: u32) -> Option<ClosureFacts<'_>> { None }
        fn coroutine(&self, _owner: ItemKey, _ordinal: u32) -> Option<CoroutineFacts<'_>> { None }
        fn coroutine_state(&self, owner: ItemKey, _ordinal: u32, _args: &[PTy], _ground: PTys<'_>) -> Result<Option<StateShape>, Missing> {
            Err(Missing { item: owner })
        }
        fn opaque_hidden(&self, _opaque: ItemKey, _args: &[PTy], _ground: &mut PortableTys<'_>) -> Result<PTy, BuildError> {
            Err(BuildError::Malformed { what: "unused" })
        }
    }

    #[test]
    fn referenced_finds_coroutines_behind_ptrs_refs_and_fndefs_while_held_skips_them() {
        let Ok(frame) = MemFrame::new(blaze_mem::tag::Tag::Infer) else { unreachable!() };
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut ground = PortableTys::new_in(alloc);
        let owner = ItemKey(42);
        let empty = ground.intern_list(&[]).ok().unwrap_or_else(|| unreachable!());
        let coro = ground.intern(PNode::Coroutine { owner, ordinal: 0, args: empty }).ok().unwrap_or_else(|| unreachable!());
        let raw_ptr = ground.intern(PNode::Ptr(Mutbl::Not, coro)).ok().unwrap_or_else(|| unreachable!());
        let mut_ref = ground.intern(PNode::Ref(Mutbl::Mut, coro)).ok().unwrap_or_else(|| unreachable!());
        let fn_args = ground.intern_list(&[coro]).ok().unwrap_or_else(|| unreachable!());
        let fn_def = ground.intern(PNode::FnDef(ItemKey(7), fn_args)).ok().unwrap_or_else(|| unreachable!());

        let expected = CoroutineOf { owner, ordinal: 0, owner_args: empty };
        let held = held_coroutines(&mut ground, &[raw_ptr, mut_ref, fn_def], &EmptyFacts, alloc).ok();
        assert_eq!(held.as_deref(), Some(&[][..]));

        for root in [raw_ptr, mut_ref, fn_def] {
            let found = referenced_coroutines(&mut ground, &[root], &EmptyFacts, alloc).ok();
            assert_eq!(found.as_deref(), Some(&[expected][..]));
        }
    }
}
