//! Instances (crates/blaze-cli/tests/ladder/README.md, rung 5): a generic fn is built once per
//! list of generic arguments it is called with. The arguments live in the program's one
//! *ground table*, a [`PortableTys`] that holds only types with no generic parameter, no hole
//! and no error; two instances are the same exactly when their items and their interned
//! argument lists are equal, so an [`Instance`] is its own identity within a run.
//!
//! [`ground`] carries a type of another table (a body's [`blaze_typeck::Checked`] types, a
//! signature's, a struct's fields) into the ground table, each `Param(i)` replaced by the
//! instance's `args[i]`. It walks the reachable nodes in index order: a portable table's
//! children have smaller indices than their parents, so no recursion and no stack.

use blaze_ids::{ContentId, Hasher};
use blaze_own::{Alloc, Vec};
use blaze_typeck::{ExprId, ItemKey, List, PNode, PTy, PTys, PortableTys};

use crate::BuildError;
use crate::layout::LayoutFacts;

/// A fn item and its generic arguments, interned in the ground table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Instance {
    pub item: ItemKey,
    pub args: List,
}

/// An instance's key: the hash of its item's code identity and its ground arguments, written
/// as trees (never as table indices, which depend on what else the table holds). References
/// name an instance by it; the key of the record its build makes is
/// [`crate::vtable::record_key`] of it and the body's trait-shape reads, so equal record keys
/// build equal code and an instance is built once per record key (rules 52, 77).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InstanceKey(pub ContentId);

const INSTANCE_DOMAIN: &str = "blaze-build instance key 2";

/// The key of the instance `args` of the item whose code identity is `code`. `args` is a list
/// of `ground`, which holds no parameter, hole or error ([`ground`] made it).
pub fn instance_key(code: ContentId, args: List, ground: PTys<'_>, scratch: Alloc<'_>) -> Result<InstanceKey, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "an instance's arguments past the ground table" };
    let mut h = Hasher::new(INSTANCE_DOMAIN);
    h.id(&code);
    let top = ground.list(args).map_err(malformed)?;
    h.update(&(top.len() as u64).to_le_bytes());
    // Pre-order, children in order: a stack of what is left to write.
    let mut left: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    left.extend_from_slice(top);
    left.reverse();
    let mut children: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    while let Some(t) = left.pop() {
        let node = ground.node(t).map_err(malformed)?;
        let (tag, payload): (u8, u128) = match node {
            PNode::Bool => (0, 0),
            PNode::Char => (1, 0),
            PNode::Int(i) => (2, i as u128),
            PNode::Uint(u) => (3, u as u128),
            PNode::Float(f) => (4, f as u128),
            PNode::Str => (5, 0),
            PNode::Never => (6, 0),
            PNode::Tuple(_) => (7, 0),
            PNode::Ref(m, _) => (8, m as u128),
            PNode::Ptr(m, _) => (9, m as u128),
            // The length is the second child, a `ConstValue`.
            PNode::Array(..) => (10, 0),
            PNode::Slice(_) => (11, 0),
            // A pattern type: its base, then its canonical ranges' values, are its children.
            PNode::Pat(..) => (15, 0),
            PNode::Adt(key, _) => (12, key.0),
            PNode::FnDef(key, _) => (13, key.0),
            // A const generic argument: its bits; its type is its one child.
            PNode::ConstValue { bits, .. } => (14, bits),
            // A trait object (a generic fn at `T = dyn Tr`, `drop_in_place::<dyn Tr>`): its
            // predicates in their one order are its children; each names its trait by key.
            PNode::Dynamic(_) => (16, 0),
            PNode::ExTrait(key, _) => (17, key.0),
            PNode::ExProj { trait_, name, .. } => (18, (u128::from(name.0) << 64) | u128::from(trait_.0)),
            PNode::ExAuto(key) => (19, key.0),
            // A closure (typeck.md, "A closure's identity"): its owner and its ORDINAL among the
            // owner body's closures (never a body-local expression id); its children the
            // owner's arguments. Its signature, kind and environment are facts of that identity.
            PNode::Closure { owner, ordinal, .. } => {
                h.update(&ordinal.to_le_bytes());
                (20, owner.0)
            }
            // A fn pointer (`Fn::call` at `[fn(u64) -> u64, (u64,)]`, lib.rs `build_fn_shim`;
            // a generic fn at `T = fn(..)`): its header (ABI and safety, one byte) is part of
            // the type; its children the inputs, then the output.
            PNode::FnPtr(_, header) => (21, u128::from(header.bits())),
            // A coroutine (coroutine.rs, the module doc), by identity as a closure is: its owner
            // and its ORDINAL among the owner body's async blocks; its children the owner's
            // arguments. Its output and state are facts of that identity. A future still named by
            // its expression never reaches a ground table (`ground` refuses it).
            PNode::Coroutine { owner, ordinal, .. } => {
                h.update(&ordinal.to_le_bytes());
                (23, owner.0)
            }
            PNode::Future(..) => {
                return Err(BuildError::UnsupportedSig {
                    what: "an async block's future named by its expression in an instance's arguments (only its identity, `PNode::Coroutine`, crosses a body: coroutine.rs)",
                });
            }
            PNode::Alias { .. }
            | PNode::Opaque(..)
            | PNode::Param(_)
            | PNode::ConstParam(_)
            | PNode::Infer
            | PNode::Error => {
                return Err(BuildError::Malformed { what: "a type with no ground form in an instance's arguments" });
            }
        };
        children.clear();
        each_child(ground, node, &mut children)?;
        h.update(&[tag]).update(&payload.to_le_bytes()).update(&(children.len() as u64).to_le_bytes());
        left.extend(children.iter().rev().copied());
    }
    Ok(InstanceKey(h.finish()))
}

/// The key of the instance of fn `code` at `args` (a list of `ground`), covering what each
/// projection of the fn's own types `from` normalizes to at those arguments: two instances that
/// differ only there (an impl's `type Item` edited) never share a key. Made as [`instance_key`]
/// over `args` followed by each projection's normalized type, in `from`'s index order; the
/// argument count is part of the hash and fixed by `code`, so the list is unambiguous, and a fn
/// with no projection keys exactly as [`instance_key`] keys it.
pub fn instance_key_normalized(
    code: ContentId,
    args: List,
    from: PTys<'_>,
    ground: &mut PortableTys<'_>,
    facts: &dyn LayoutFacts,
    scratch: Alloc<'_>,
) -> Result<InstanceKey, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "an instance's arguments past the ground table" };
    let mut all: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    all.extend_from_slice(ground.view().list(args).map_err(malformed)?);
    let given = all.len();
    for (at, node) in from.nodes().enumerate() {
        if matches!(node, PNode::Alias { .. }) {
            let normalized = crate::instance::ground(from, PTy(at as u32), &all[..given], ground, scratch, facts)?;
            all.push(normalized);
        }
    }
    if all.len() == given {
        return instance_key(code, args, ground.view(), scratch);
    }
    let list = ground.intern_list(&all).map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
    instance_key(code, list, ground.view(), scratch)
}

/// The one function every type lowered at an instance passes through (signatures, locals,
/// fields, casts, a callee's signature): `ty` of table `from`, in `ground` with `Param(i)`
/// replaced by `args[i]` (`args` are types of `ground`), every projection then normalized by
/// `facts` (normalize.rs). A closure grounds like a fn item: its arguments mapped. Refused,
/// naming the node: a parameter past `args`, a hole, an error, a projection the solver does not
/// answer, an opaque type, an async block's future. `scratch` is the allocator of the walk's
/// columns.
pub fn ground(
    from: PTys<'_>,
    ty: PTy,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
    facts: &dyn LayoutFacts,
) -> Result<PTy, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a type index past its table" };
    let mut projections = false;
    // Which nodes of `from` `ty` reaches: marked, then mapped in index order.
    let top = ty.0 as usize;
    let mut reached: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(top + 1, scratch);
    reached.resize(top + 1, false);
    reached[top] = true;
    let mut mapped: Vec<PTy, Alloc<'_>> = Vec::with_capacity_in(top + 1, scratch);
    mapped.resize(top + 1, PTy(u32::MAX));
    let mut children: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    // Marking runs from the top down: a node's children have smaller indices.
    for at in (0..=top).rev() {
        if !reached[at] {
            continue;
        }
        children.clear();
        each_child(from, from.node(PTy(at as u32)).map_err(malformed)?, &mut children)?;
        for child in children.iter() {
            *reached.get_mut(child.0 as usize).ok_or(BuildError::Malformed { what: "a child after its parent" })? = true;
        }
    }
    let mut list: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    for at in 0..=top {
        if !reached[at] {
            continue;
        }
        let mut map_list = |l: List, list: &mut Vec<PTy, Alloc<'_>>| -> Result<List, BuildError> {
            list.clear();
            for t in from.list(l).map_err(malformed)? {
                list.push(mapped[t.0 as usize]);
            }
            ground.intern_list(list).map_err(|_| BuildError::Malformed { what: "the ground table is full" })
        };
        let node = match from.node(PTy(at as u32)).map_err(malformed)? {
            PNode::Param(i) | PNode::ConstParam(i) => {
                mapped[at] = *args.get(i as usize).ok_or(BuildError::UnsupportedSig { what: "a generic parameter with no argument" })?;
                continue;
            }
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: mapped[ty.0 as usize], bits },
            n @ (PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never) => n,
            PNode::Tuple(l) => PNode::Tuple(map_list(l, &mut list)?),
            PNode::Ref(m, t) => PNode::Ref(m, mapped[t.0 as usize]),
            PNode::Ptr(m, t) => PNode::Ptr(m, mapped[t.0 as usize]),
            PNode::Array(t, n) => PNode::Array(mapped[t.0 as usize], mapped[n.0 as usize]),
            PNode::Slice(t) => PNode::Slice(mapped[t.0 as usize]),
            PNode::Pat(t, l) => PNode::Pat(mapped[t.0 as usize], map_list(l, &mut list)?),
            PNode::Adt(key, l) => PNode::Adt(key, map_list(l, &mut list)?),
            PNode::FnDef(key, l) => PNode::FnDef(key, map_list(l, &mut list)?),
            PNode::FnPtr(l, header) => PNode::FnPtr(map_list(l, &mut list)?, header),
            PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: map_list(args, &mut list)? },
            // A coroutine by identity grounds like a closure: its owner's arguments mapped
            // (coroutine.rs, the module doc). The checker's in-body form has no identity outside
            // its body: refused by name, never guessed.
            PNode::Coroutine { owner, ordinal, args } => PNode::Coroutine { owner, ordinal, args: map_list(args, &mut list)? },
            PNode::Future(..) => {
                return Err(BuildError::UnsupportedSig {
                    what: "an async block's future named by its expression (only its identity, `PNode::Coroutine`, crosses a body: coroutine.rs)",
                });
            }
            PNode::Alias { trait_, name, args } => {
                projections = true;
                PNode::Alias { trait_, name, args: map_list(args, &mut list)? }
            }
            // An `impl Trait` type is its hidden type at these arguments, the world's answer
            // (`LayoutFacts::opaque_hidden`; an `async fn`'s is its coroutine, coroutine.rs).
            // `list` holds the mapped arguments after `map_list`. Not answered: refused by name.
            PNode::Opaque(key, l) => {
                map_list(l, &mut list)?;
                mapped[at] = facts.opaque_hidden(key, &list, ground)?;
                continue;
            }
            // The predicates' order is by (kind, trait, name), which no argument changes: the
            // mapped list keeps the one form (`PortableTys::dynamic`).
            PNode::Dynamic(l) => PNode::Dynamic(map_list(l, &mut list)?),
            PNode::ExTrait(key, l) => PNode::ExTrait(key, map_list(l, &mut list)?),
            PNode::ExProj { trait_, name, args, term } => {
                PNode::ExProj { trait_, name, args: map_list(args, &mut list)?, term: mapped[term.0 as usize] }
            }
            PNode::ExAuto(key) => PNode::ExAuto(key),
            PNode::Infer | PNode::Error => return Err(BuildError::Malformed { what: "a hole or error type after a clean check" }),
        };
        mapped[at] = ground.intern(node).map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
    }
    if projections {
        return crate::normalize::normalize_ground(ground, mapped[top], facts, scratch);
    }
    Ok(mapped[top])
}

/// The children of `node` (of table `from`), pushed to `out`: the one order a ground type's
/// tree is written in (here, and blaze-ladder symbols.rs's instance names).
pub fn each_child(from: PTys<'_>, node: PNode, out: &mut Vec<PTy, Alloc<'_>>) -> Result<(), BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a list past its table" };
    match node {
        PNode::Tuple(l) | PNode::Adt(_, l) | PNode::FnDef(_, l) | PNode::FnPtr(l, _) | PNode::Opaque(_, l) => {
            out.extend_from_slice(from.list(l).map_err(malformed)?);
        }
        PNode::Alias { args, .. } | PNode::Closure { args, .. } | PNode::Coroutine { args, .. } => {
            out.extend_from_slice(from.list(args).map_err(malformed)?)
        }
        PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Slice(t) | PNode::ConstValue { ty: t, .. } => out.push(t),
        PNode::Future(_, o, i) | PNode::Array(o, i) => {
            out.push(o);
            out.push(i);
        }
        PNode::Pat(b, l) => {
            out.push(b);
            out.extend_from_slice(from.list(l).map_err(malformed)?);
        }
        PNode::Dynamic(l) | PNode::ExTrait(_, l) => out.extend_from_slice(from.list(l).map_err(malformed)?),
        PNode::ExProj { args, term, .. } => {
            out.extend_from_slice(from.list(args).map_err(malformed)?);
            out.push(term);
        }
        PNode::ExAuto(_)
        | PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Param(_)
        | PNode::ConstParam(_)
        | PNode::Infer
        | PNode::Error => {}
    }
    Ok(())
}
