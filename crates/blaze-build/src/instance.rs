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
use blaze_typeck::{BodyView, ItemKey, List, PNode, PTy, PTys, PortableTys};

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

// Form 3: each argument by its Merkle digest ([`write_trees`]), not its tree's stream.
const INSTANCE_DOMAIN: &str = "blaze-build instance key 3";

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

const TYPE_ID_DOMAIN: &str = "blaze-build type_id 2";

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

/// Writes the ground types `top`: their count, then each one's DIGEST. A node's digest is the
/// hash of its tag, its payload (and a closure's or a coroutine's ordinal), its count of
/// children and each child's digest in order: a Merkle hash of its tree, computed once per
/// DISTINCT type (memo by ground id), so linear in the distinct types `top` reaches, never in
/// its tree. The tree of a type of shared parts is exponentially larger (`(T, T)` doubled 40
/// times is 2^41 nodes; blaze-ladder symbols.rs, "WHY A FOREST"), and hashing it was the next
/// stop after the instance names. Never a table index: two tables give one digest for one
/// structure (rule 25). The one identity form of [`instance_key`] and [`type_id_hash`].
fn write_trees(h: &mut Hasher, top: &[PTy], ground: PTys<'_>, scratch: Alloc<'_>) -> Result<(), BuildError> {
    h.update(&(top.len() as u64).to_le_bytes());
    let mut digests = Digests::new_in(scratch);
    for &t in top {
        let d = digests.of(t, ground)?;
        h.update(&d);
    }
    Ok(())
}

const NODE_DOMAIN: &str = "blaze-build type node 1";

/// One open node of [`Digests::of`]'s walk: its type, its node, where its children are in
/// `kids` and where their digests are in `done`.
#[derive(Clone, Copy)]
struct OpenNode {
    ty: PTy,
    node: PNode,
    kids_at: u32,
    kids_len: u32,
    done_at: u32,
}

/// The digests of one [`write_trees`]: (type, digest) rows found by open addressing over the
/// type's id (0 empty, else row + 1; a power of two long, at most half full), and the walk's
/// stacks. All in the caller's scratch allocator; none outlives the call.
struct Digests<'s> {
    memo: Vec<(PTy, [u8; 32]), Alloc<'s>>,
    slots: Vec<u32, Alloc<'s>>,
    open: Vec<OpenNode, Alloc<'s>>,
    kids: Vec<PTy, Alloc<'s>>,
    done: Vec<[u8; 32], Alloc<'s>>,
    /// Nodes hashed: one per distinct type (the linear claim's measure, read by its test).
    #[cfg(test)]
    hashed: u64,
}

impl<'s> Digests<'s> {
    fn new_in(scratch: Alloc<'s>) -> Digests<'s> {
        Digests {
            memo: Vec::new_in(scratch),
            slots: Vec::new_in(scratch),
            open: Vec::new_in(scratch),
            kids: Vec::new_in(scratch),
            done: Vec::new_in(scratch),
            #[cfg(test)]
            hashed: 0,
        }
    }

    fn slot(t: PTy, len: usize) -> usize {
        let h = u64::from(t.0).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        ((h ^ (h >> 31)) as usize) & (len - 1)
    }

    fn get(&self, t: PTy) -> Option<[u8; 32]> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = Digests::slot(t, self.slots.len());
        loop {
            match self.slots[at] {
                0 => return None,
                n if self.memo[n as usize - 1].0 == t => return Some(self.memo[n as usize - 1].1),
                _ => at = (at + 1) & mask,
            }
        }
    }

    fn put(&mut self, t: PTy, d: [u8; 32]) {
        if (self.memo.len() + 1) * 2 > self.slots.len() {
            let len = (self.slots.len() * 2).max(16);
            self.slots.clear();
            self.slots.resize(len, 0);
            for row in 0..self.memo.len() {
                let mut at = Digests::slot(self.memo[row].0, len);
                while self.slots[at] != 0 {
                    at = (at + 1) & (len - 1);
                }
                self.slots[at] = row as u32 + 1;
            }
        }
        let mask = self.slots.len() - 1;
        let mut at = Digests::slot(t, self.slots.len());
        while self.slots[at] != 0 {
            at = (at + 1) & mask;
        }
        self.slots[at] = self.memo.len() as u32 + 1;
        self.memo.push((t, d));
    }

    /// `t`'s digest: post-order, children in order, by a stack of open nodes (no recursion);
    /// a type met again is its memo.
    fn of(&mut self, t: PTy, ground: PTys<'_>) -> Result<[u8; 32], BuildError> {
        if let Some(d) = self.get(t) {
            return Ok(d);
        }
        self.open_node(t, ground)?;
        let mut last = [0u8; 32];
        while let Some(&o) = self.open.last() {
            let have = self.done.len() - o.done_at as usize;
            if have < o.kids_len as usize {
                let child = self.kids[o.kids_at as usize + have];
                match self.get(child) {
                    Some(d) => self.done.push(d),
                    None => self.open_node(child, ground)?,
                }
                continue;
            }
            let (tag, payload, ordinal) = node_head(o.node)?;
            let mut h = Hasher::new(NODE_DOMAIN);
            h.update(&[tag]).update(&payload.to_le_bytes());
            if let Some(ordinal) = ordinal {
                h.update(&ordinal.to_le_bytes());
            }
            h.update(&u64::from(o.kids_len).to_le_bytes());
            for i in o.done_at as usize..self.done.len() {
                let child = self.done[i];
                h.update(&child);
            }
            let d = h.finish().0;
            #[cfg(test)]
            {
                self.hashed += 1;
            }
            self.put(o.ty, d);
            self.kids.truncate(o.kids_at as usize);
            self.done.truncate(o.done_at as usize);
            self.open.pop();
            if !self.open.is_empty() {
                self.done.push(d);
            }
            last = d;
        }
        Ok(last)
    }

    /// `t` opened: its node read and checked to have a ground form, its children stacked.
    fn open_node(&mut self, t: PTy, ground: PTys<'_>) -> Result<(), BuildError> {
        let node = ground.node(t).map_err(|_| BuildError::Malformed { what: "an instance's arguments past the ground table" })?;
        node_head(node)?;
        let kids_at = self.kids.len();
        each_child(ground, node, &mut self.kids)?;
        let kids_len = self.kids.len() - kids_at;
        self.open.push(OpenNode { ty: t, node, kids_at: kids_at as u32, kids_len: kids_len as u32, done_at: self.done.len() as u32 });
        Ok(())
    }
}

/// A node's tag, payload and (a closure's or a coroutine's) ordinal, as its digest holds them;
/// a type with no ground form refused by name.
fn node_head(node: PNode) -> Result<(u8, u128, Option<u32>), BuildError> {
    let (tag, payload, ordinal): (u8, u128, Option<u32>) = match node {
        PNode::Bool => (0, 0, None),
        PNode::Char => (1, 0, None),
        PNode::Int(i) => (2, i as u128, None),
        PNode::Uint(u) => (3, u as u128, None),
        PNode::Float(f) => (4, f as u128, None),
        PNode::Str => (5, 0, None),
        PNode::Never => (6, 0, None),
        PNode::Tuple(_) => (7, 0, None),
        PNode::Record(s, _) => (25, u128::from(s.0), None),
        PNode::Union(_) => (26, 0, None),
        PNode::Ref(m, _) => (8, m as u128, None),
        PNode::Ptr(m, _) => (9, m as u128, None),
        // The length is the second child, a `ConstValue`.
        PNode::Array(..) => (10, 0, None),
        PNode::Slice(_) => (11, 0, None),
        // A pattern type: its base, then its canonical ranges' values, are its children.
        PNode::Pat(..) => (15, 0, None),
        PNode::Adt(key, _) => (12, key.0, None),
        PNode::FnDef(key, _) => (13, key.0, None),
        // A const generic argument: its bits; its type is its one child.
        PNode::ConstValue { bits, .. } => (14, bits, None),
        // A trait object (a generic fn at `T = dyn Tr`, `drop_in_place::<dyn Tr>`): its
        // predicates in their one order are its children; each names its trait by key.
        PNode::Dynamic(_) => (16, 0, None),
        PNode::ExTrait(key, _) => (17, key.0, None),
        PNode::ExProj { trait_, name, .. } => (18, (u128::from(name.0) << 64) | u128::from(trait_.0), None),
        PNode::ExAuto(key) => (19, key.0, None),
        // An extern type, by its item (no children). A tag no earlier node had, so no key
        // made before it changes.
        PNode::Foreign(key) => (24, key.0, None),
        // A closure (typeck.md, "A closure's identity"): its owner and its ORDINAL among the
        // owner body's closures (never a body-local expression id); its children the owner's
        // arguments. Its signature, kind and environment are facts of that identity.
        PNode::Closure { owner, ordinal, .. } => (20, owner.0, Some(ordinal)),
        // A fn pointer (`Fn::call` at `[fn(u64) -> u64, (u64,)]`, lib.rs `build_fn_shim`; a
        // generic fn at `T = fn(..)`): its header (ABI and safety, one byte) is part of the type;
        // its children the inputs, then the output.
        PNode::FnPtr(_, header) => (21, u128::from(header.bits()), None),
        // A coroutine (coroutine.rs, the module doc), by identity as a closure is: its owner and
        // its ORDINAL among the owner body's async blocks; its children the owner's arguments.
        // Its output and state are facts of that identity. A future still named by its
        // expression never reaches a ground table (`ground` refuses it).
        PNode::Coroutine { owner, ordinal, .. } => (23, owner.0, Some(ordinal)),
        PNode::Future(..) => {
            return Err(BuildError::UnsupportedSig {
                what: "an async block's future named by its expression in an instance's arguments (only its identity, `PNode::Coroutine`, crosses a body: coroutine.rs)",
            });
        }
        PNode::Alias { .. } | PNode::Opaque(..) | PNode::Param(_) | PNode::ConstParam(_) | PNode::Infer | PNode::Error => {
            return Err(BuildError::Malformed { what: "a type with no ground form in an instance's arguments" });
        }
    };
    Ok((tag, payload, ordinal))
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

/// Synthetic item key naming the `ReifyShim` of a `#[track_caller]` fn item taken as a fn
/// pointer: `Instance { item: REIFY_SHIM_ITEM, args: [PNode::FnDef(target, target_args)] }`.
///
/// The shim has the plain fn pointer ABI (no hidden `&Location` parameter), materializes a
/// `Location` row for `target`'s own definition site (`Adts::def_site(target)`), and calls
/// `target` directly with that location passed last.
pub const REIFY_SHIM_ITEM: ItemKey = ItemKey(u128::from_be_bytes(*b"blaze-build rfy1"));

/// The `ReifyShim` instance of `#[track_caller]` fn `callee`:
/// `Instance { item: REIFY_SHIM_ITEM, args: [PNode::FnDef(callee.item, callee.args)] }`.
pub fn reify_shim_instance(callee: Instance, ground: &mut PortableTys<'_>) -> Result<Instance, BuildError> {
    let full = |_| BuildError::Malformed { what: "the ground table is full" };
    let fn_ty = ground.intern(PNode::FnDef(callee.item, callee.args)).map_err(full)?;
    let args = ground.intern_list(&[fn_ty]).map_err(full)?;
    Ok(Instance { item: REIFY_SHIM_ITEM, args })
}

/// If `instance` is a `ReifyShim` (`item == REIFY_SHIM_ITEM`, `args == [FnDef(target, args)]`),
/// returns the underlying tracked fn `Instance`.
pub fn reify_shim_of(instance: Instance, ground: PTys<'_>) -> Option<Instance> {
    if instance.item != REIFY_SHIM_ITEM {
        return None;
    }
    let &[fn_ty] = ground.list(instance.args).ok()? else { return None };
    let PNode::FnDef(item, args) = ground.node(fn_ty).ok()? else { return None };
    Some(Instance { item, args })
}

/// The list a `ReifyShim` for `target` is keyed by over `target`'s code identity: `target`'s own
/// arguments first (so any projection in `target`'s checked types normalizes at them), then
/// `REIFY_SHIM_ITEM` as a `FnDef`. Never equal to `target`'s own list: it is one longer, and the
/// count is hashed.
pub fn reify_shim_key_args(target: Instance, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Result<List, BuildError> {
    let malformed = |_| BuildError::Malformed { what: "a reify shim's target arguments past the ground table" };
    let full = |_| BuildError::Malformed { what: "the ground table is full" };
    let mut all: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    all.extend_from_slice(ground.view().list(target.args).map_err(malformed)?);
    all.push(ground.intern(PNode::FnDef(REIFY_SHIM_ITEM, List::EMPTY)).map_err(full)?);
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
        let raw_node = from.node(PTy(at as u32)).map_err(malformed)?;
        if let PNode::Record(s, _) = raw_node
            && let Some(names) = from.record_names(s)
        {
            ground
                .register_record_schema(s, names)
                .map_err(|_| BuildError::Malformed { what: "the ground table is full" })?;
        }
        let mut map_list = |l: List, list: &mut Vec<PTy, Alloc<'_>>| -> Result<List, BuildError> {
            list.clear();
            for t in from.list(l).map_err(malformed)? {
                list.push(mapped[t.0 as usize]);
            }
            ground.intern_list(list).map_err(|_| BuildError::Malformed { what: "the ground table is full" })
        };
        let node = match raw_node {
            PNode::Param(i) | PNode::ConstParam(i) => {
                mapped[at] = *args.get(i as usize).ok_or(BuildError::UnsupportedSig { what: "a generic parameter with no argument" })?;
                continue;
            }
            PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: mapped[ty.0 as usize], bits },
            n @ (PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str | PNode::Never) => n,
            PNode::Tuple(l) => PNode::Tuple(map_list(l, &mut list)?),
            PNode::Record(s, l) => PNode::Record(s, map_list(l, &mut list)?),
            PNode::Union(l) => PNode::Union(map_list(l, &mut list)?),
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
                    // Asked at GROUND arguments, every projection among them normalized first
                    // (as the whole type is at the end): the question names its arguments, and
                    // a projection is no instance's argument (core's `for_each` and its inner
                    // `fn call<T>(..) -> impl FnMut((), T)` at `T = <I as Iterator>::Item`).
                    if projections {
                        for t in list.iter_mut() {
                            *t = crate::normalize::normalize_ground(ground, *t, facts, scratch)?;
                        }
                    }
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
            PNode::Foreign(key) => PNode::Foreign(key),
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
        PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::Adt(_, l) | PNode::FnDef(_, l) | PNode::FnPtr(l, _) | PNode::Opaque(_, l) => {
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
        | PNode::Foreign(_)
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

#[cfg(test)]
mod digest_tests {
    //! [`write_trees`] is linear in DISTINCT types: one hash a distinct node, measured by the
    //! count of nodes hashed (never wall time), with the tree walk it replaced as the control.

    use super::*;
    use blaze_typeck::UintTy;

    /// `(t, t)` nested `depth` times over `u8`, after `noise` unrelated types.
    fn doubling(ground: &mut PortableTys<'_>, depth: u32, noise: usize) -> PTy {
        let others = [PNode::Bool, PNode::Char, PNode::Str, PNode::Never, PNode::Uint(UintTy::U16)];
        for &other in others.iter().take(noise) {
            ground.intern(other).expect("an unrelated type");
        }
        let mut t = ground.intern(PNode::Uint(UintTy::U8)).expect("u8");
        for _ in 0..depth {
            let pair = ground.intern_list(&[t, t]).expect("a pair");
            t = ground.intern(PNode::Tuple(pair)).expect("a tuple");
        }
        t
    }

    /// The control: the nodes a tree walk visits (the old `write_trees`, pre-order, no memo).
    fn tree_walk_visits(ground: PTys<'_>, top: PTy, scratch: Alloc<'_>) -> u64 {
        let mut left: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
        left.push(top);
        let mut kids: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
        let mut visits = 0;
        while let Some(t) = left.pop() {
            visits += 1;
            kids.clear();
            each_child(ground, ground.node(t).expect("a ground type"), &mut kids).expect("its children");
            left.extend_from_slice(&kids);
        }
        visits
    }

    /// The doubling chain at depth 40 hashes 41 nodes (u8 and 40 tuples), the count of its
    /// distinct types; the key is equal from a table that gives the types other ids, and
    /// differs at another depth. CONTROL: the tree walk at depth 10 visits 2^11 - 1 nodes (at
    /// 40 it would visit 2^41 - 1, which is why it is not run).
    #[test]
    fn a_doubling_chain_hashes_each_distinct_type_once() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut a = PortableTys::new_in(alloc);
        let t = doubling(&mut a, 40, 0);
        let mut digests = Digests::new_in(alloc);
        let d = digests.of(t, a.view()).expect("a digest");
        assert_eq!(digests.hashed, 41, "one hash a distinct type");
        assert_eq!(digests.memo.len(), 41);
        let mut b = PortableTys::new_in(alloc);
        let u = doubling(&mut b, 40, 5);
        assert_ne!(t, u, "the two tables give the type other ids");
        let mut again = Digests::new_in(alloc);
        assert_eq!(again.of(u, b.view()).expect("a digest"), d, "content only (rule 25)");
        let code = ContentId([3; 32]);
        let key_a = instance_key(code, a.intern_list(&[t]).expect("args"), a.view(), alloc).expect("a key");
        let key_b = instance_key(code, b.intern_list(&[u]).expect("args"), b.view(), alloc).expect("a key");
        assert_eq!(key_a, key_b);
        let shallower = doubling(&mut a, 39, 0);
        let key_c = instance_key(code, a.intern_list(&[shallower]).expect("args"), a.view(), alloc).expect("a key");
        assert_ne!(key_a, key_c, "another type, another key");
        assert_eq!(type_id_hash(t, a.view(), alloc).expect("a type id"), type_id_hash(u, b.view(), alloc).expect("a type id"));
        // Control: the tree walk it replaced.
        let small = doubling(&mut a, 10, 0);
        assert_eq!(tree_walk_visits(a.view(), small, alloc), (1 << 11) - 1, "control: the tree is exponential");
        let mut small_digests = Digests::new_in(alloc);
        small_digests.of(small, a.view()).expect("a digest");
        assert_eq!(small_digests.hashed, 11, "and the digest linear at the same depth");
    }
}
