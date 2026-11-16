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
use blaze_typeck::{ItemKey, List, PNode, PTy, PTys, PortableTys};

use crate::BuildError;

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
            PNode::FnPtr(..)
            | PNode::Closure(..)
            | PNode::Future(..)
            | PNode::Alias { .. }
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

/// `ty` of table `from`, in `ground` with `Param(i)` replaced by `args[i]` (`args` are types of
/// `ground`). Refused, naming the node: a parameter past `args`, a hole, an error, an alias,
/// an opaque type, a closure, a fn pointer: none has a ground form the ladder builds yet.
/// `scratch` is the allocator of the walk's two columns.
pub fn ground(
    from: PTys<'_>,
    ty: PTy,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a type index past its table" };
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
            PNode::Closure(..) => return Err(BuildError::UnsupportedSig { what: "a closure type in an instance" }),
            PNode::Future(..) => return Err(BuildError::UnsupportedSig { what: "an async block's future in an instance" }),
            PNode::Alias { .. } => return Err(BuildError::UnsupportedSig { what: "an associated type in an instance" }),
            PNode::Opaque(..) => return Err(BuildError::UnsupportedSig { what: "an `impl Trait` type in an instance" }),
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
    Ok(mapped[top])
}

/// The children of `node` (of table `from`), pushed to `out`.
fn each_child(from: PTys<'_>, node: PNode, out: &mut Vec<PTy, Alloc<'_>>) -> Result<(), BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a list past its table" };
    match node {
        PNode::Tuple(l) | PNode::Adt(_, l) | PNode::FnDef(_, l) | PNode::FnPtr(l, _) | PNode::Opaque(_, l) => {
            out.extend_from_slice(from.list(l).map_err(malformed)?);
        }
        PNode::Alias { args, .. } => out.extend_from_slice(from.list(args).map_err(malformed)?),
        PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Slice(t) | PNode::Closure(_, t) | PNode::ConstValue { ty: t, .. } => out.push(t),
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
