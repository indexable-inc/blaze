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
use blaze_typeck::{BodyView, ExprId, ItemKey, List, PNode, PTy, PTys, PortableTys};

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
    write_trees(&mut h, top, ground, scratch)?;
    Ok(InstanceKey(h.finish()))
}

const TYPE_ID_DOMAIN: &str = "blaze-build type_id 1";

/// `core::intrinsics::reflection::type_id::<T>()`'s value: the 128 bits `TypeId` holds (its
/// `data` words, little-endian, as `TypeId::as_u128` reads them back). The pinned core: "Gets an
/// identifier which is globally unique to the specified type. This function will return the same
/// value for a type regardless of whichever crate it is invoked in."
///
/// It is the first 128 bits of the BLAKE3 hash, in a domain of its own, of `t`'s tree as
/// [`instance_key`] writes a ground type (rule 30: one form of a type's identity): every node's
/// kind, its item key (a stable, path-derived key, never a table index or an address) or its
/// value, and its children in order. So it is equal on every machine and build for one type and
/// for a type and any alias of it (an alias is resolved before a type is ground), and different
/// for two types but with probability 2^-128. Lifetimes are erased, as `TypeId` requires
/// (`TypeId::of` asks `T: 'static`, and two types differing only in lifetimes are one): a ground
/// type holds no region. One function for every caller: the constant evaluator (blaze-consteval,
/// core's `const { type_id::<T>() }`) and the builder's table row both call it, so the run-time
/// value and the constant are one.
///
/// A known gap, stated: a higher-ranked `for<'a> fn(&'a ())` and `fn(&'static ())` are two
/// types to rustc (core's `any.rs` example), but a ground fn pointer holds no binder, so they
/// hash alike here.
pub fn type_id_hash(t: PTy, ground: PTys<'_>, scratch: Alloc<'_>) -> Result<u128, BuildError> {
    let mut h = Hasher::new(TYPE_ID_DOMAIN);
    write_trees(&mut h, &[t], ground, scratch)?;
    Ok(h.finish().fingerprint().0)
}

/// Writes the ground types `top` as trees: their count, then each node pre-order, children in
/// order (never table indices, which depend on what else the table holds). The one tree form of
/// [`instance_key`] and [`type_id_hash`].
fn write_trees(h: &mut Hasher, top: &[PTy], ground: PTys<'_>, scratch: Alloc<'_>) -> Result<(), BuildError> {
    let malformed = |_| BuildError::Malformed { what: "an instance's arguments past the ground table" };
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
    Ok(())
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

/// The list a closure's `Fn*` method instance `instance` (`[closure, (A..)]`, its item the trait
/// method) is keyed by, over its OWNER's code identity: the owner's arguments first (so the
/// owner's projections normalize at them), then the method as a `FnDef`, then the instance's own
/// arguments. Never equal to an owner's own list: it is longer, and the count is hashed. The one
/// form (rule 30) of the run's instance keys (blaze-ladder run.rs) and of the constant
/// evaluator's fn pointers (blaze-consteval's `ClosureFnPointer`), so a record's fn word names
/// the slot the run builds.
pub fn closure_method_key_args(instance: Instance, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<List, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a closure instance's arguments past the ground table" };
    let full = |_| BuildError::Malformed { what: "the ground table is full" };
    let mut own: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    own.extend_from_slice(ground.view().list(instance.args).map_err(malformed)?);
    let of = own
        .first()
        .and_then(|&t| crate::closure_of(ground.view(), t))
        .ok_or(BuildError::Malformed { what: "a closure method instance whose self is not a closure" })?;
    let mut all: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    all.extend_from_slice(ground.view().list(of.owner_args).map_err(malformed)?);
    all.push(ground.intern(PNode::FnDef(instance.item, List::EMPTY)).map_err(full)?);
    all.extend_from_slice(&own);
    ground.intern_list(&all).map_err(full)
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
    map_tree(from, ty, args, ground, scratch, Settle::Instance { facts, own: None })
}

/// [`ground`] of a type of `body`'s OWN record at its instance (`args`, the walk's): the same
/// mapping, and each future the record names by expression, at any depth (`Fut` of a callee's
/// arguments, a closure's `-> impl Future` output), is its identity at `args`
/// (`PNode::Coroutine` of `body.owner`; coroutine.rs, the module doc), as `copy_written` makes
/// it. A future of an expression that is neither an async block nor an async closure of
/// `body` is refused by name.
pub fn ground_own(
    from: PTys<'_>,
    ty: PTy,
    args: &[PTy],
    body: &BodyView<'_>,
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
    facts: &dyn LayoutFacts,
) -> Result<PTy, BuildError> {
    map_tree(from, ty, args, ground, scratch, Settle::Instance { facts, own: Some(body) })
}

/// Record type `ty` of `body`'s check (table `from`) copied AS WRITTEN into `out`, a facts
/// table in the owner's generic parameters (`params[i]` is `Param(i)` of `out`): the form a
/// closure's or a coroutine's facts are kept in (`layout::closure_facts`,
/// `coroutine::coroutine_facts`) until an instance grounds them at its arguments through
/// [`ground`] with the world's facts. So nothing only an instance can settle is settled here:
/// a projection and an `impl Trait` type are kept with their arguments mapped (normalized and
/// revealed at the instance), and an `async` block or async closure of `body`, which its own
/// record names by expression (`PNode::Future`), is its identity (`PNode::Coroutine` of
/// `body.owner` at `params`: coroutine.rs, the module doc). Refused by name: a hole or an error
/// type, a parameter past `params`, a future of an expression that is neither form.
pub fn copy_written(
    from: PTys<'_>,
    ty: PTy,
    params: &[PTy],
    body: &BodyView<'_>,
    out: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    map_tree(from, ty, params, out, scratch, Settle::Written(body))
}

/// What [`map_tree`] does with the forms only an instance can settle: the one difference
/// between grounding at an instance and copying a record as written.
#[derive(Clone, Copy)]
enum Settle<'f, 'b> {
    /// At an instance ([`ground`], [`ground_own`]): projections normalized and opaques revealed
    /// by `facts`; a future named by its expression is its identity when `own` is the body whose
    /// record names it, else refused.
    Instance { facts: &'f dyn LayoutFacts, own: Option<&'f BodyView<'b>> },
    /// As written ([`copy_written`]): projections and opaques kept; this body's futures named by
    /// expression become their identities.
    Written(&'f BodyView<'b>),
}

/// The one mapping of a record type's tree into another table, `Param(i)` replaced by
/// `args[i]`, the instance-only forms as `settle` says.
fn map_tree(
    from: PTys<'_>,
    ty: PTy,
    args: &[PTy],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
    settle: Settle<'_, '_>,
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
        let node = from.node(PTy(at as u32)).map_err(malformed)?;
        // Copied as written, a future by expression is its identity: its output and interior
        // are facts of that identity, never parts of the type, so they are not mapped.
        if let (Settle::Written(_) | Settle::Instance { own: Some(_), .. }, PNode::Future(..)) = (settle, node) {
            continue;
        }
        each_child(from, node, &mut children)?;
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
            PNode::Future(e, ..) => match settle {
                Settle::Instance { own: None, .. } => {
                    return Err(BuildError::UnsupportedSig {
                        what: "an async block's future named by its expression (only its identity, `PNode::Coroutine`, crosses a body: coroutine.rs)",
                    });
                }
                // The checker's rule for the ordinal (an async block's among the body's blocks,
                // an async closure's past `CLOSURE_BASE`), the owner's parameters (or, at an
                // instance of the body itself, its arguments) the identity's arguments.
                Settle::Written(body) | Settle::Instance { own: Some(body), .. } => {
                    let ordinal = blaze_typeck::coroutine_id::coroutine_ordinal(body, e).ok_or(BuildError::Malformed {
                        what: "a future named by an expression that is neither an async block nor an async closure of its body",
                    })?;
                    let params = ground.intern_list(args).map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
                    mapped[at] = crate::coroutine_identity(ground, body.owner, ordinal, params)?;
                    continue;
                }
            },
            PNode::Alias { trait_, name, args } => {
                projections |= matches!(settle, Settle::Instance { .. });
                PNode::Alias { trait_, name, args: map_list(args, &mut list)? }
            }
            // An `impl Trait` type is its hidden type at these arguments, the world's answer
            // (`LayoutFacts::opaque_hidden`; an `async fn`'s is its coroutine, coroutine.rs).
            // `list` holds the mapped arguments after `map_list`. Not answered: refused by name.
            // Copied as written it is kept, revealed when an instance grounds it.
            PNode::Opaque(key, l) => match settle {
                Settle::Instance { facts, .. } => {
                    map_list(l, &mut list)?;
                    mapped[at] = facts.opaque_hidden(key, &list, ground)?;
                    continue;
                }
                Settle::Written(_) => PNode::Opaque(key, map_list(l, &mut list)?),
            },
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
    if let (true, Settle::Instance { facts, .. }) = (projections, settle) {
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
