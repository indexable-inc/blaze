//! The four hashes of an item (docs/v2/plan/lanes/items.md section 6.1): its forms (`form`) with every
//! slot filled by the hash of what it refers to.
//!
//! - **interface**: the interface form, each ref by its interface hash.
//! - **layout** (of a type): the interface form and the target's data layout, each type it
//!   refers to by its layout hash and anything else (a trait in a bound) by its interface
//!   hash: a field's type changing size changes it, transitively.
//! - **code** (of an item with a body): its interface hash and its body form, each ref by its
//!   interface hash, so a callee's body edit leaves it as it was.
//! - **deep**: the interface and body forms, each ref by its deep hash, for what inlines.
//! - **signature**: the signature form (the interface form with a marker in place of what a
//!   body leaks into it), each ref by its signature hash: what a body's identity names an
//!   item by, which no body's inference may go into.
//!
//! Items that refer to each other are hashed as one unit, as Unison does: the members of a
//! strongly connected component of the graph of refs are ordered by their content, each ref
//! inside it is the position of its target, and each member's hash is the unit's and its
//! position. Components are found by Tarjan's algorithm over the refs of one crate (a cycle
//! never crosses crates, dependencies being acyclic): from the item alone, or once for all of
//! the deep graph of a local crate (`members` says when). A component is hashed once for all
//! its members, and each ref
//! outside it is filled by the tracked query of that ref, which never reaches back. Each form
//! is a tracked query, so an edit rewrites the forms it touched and a hash that comes out
//! equal stops what depends on it.
//!
//! A form that could not be written (the program is not one rustc accepts) gives no hash but
//! an [`Unhashable`] naming the item: never a hash salted by where the item is, which two
//! different edits could share.

use std::{alloc::Allocator, fmt, fmt::Write as _};

use base_db::Db;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Frame, Tag, frame};
use hir_def::AdtId;
use hir_ty::{
    InferBodyId,
    db::{HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
};

use crate::{
    CodeHash, DeepHash, InterfaceHash, ItemHashes, ItemId, LayoutHash, SignatureHash, form, key,
};

pub const INTERFACE_DOMAIN: &str = "blaze v2 interface hash 1";
pub const SIGNATURE_DOMAIN: &str = "blaze v2 signature hash 1";
pub const LAYOUT_DOMAIN: &str = "blaze v2 layout hash 1";
pub const CODE_DOMAIN: &str = "blaze v2 code hash 1";
pub const DEEP_DOMAIN: &str = "blaze v2 deep hash 1";
pub const CLOSURE_CODE_DOMAIN: &str = "blaze v2 closure code hash 1";
pub const COROUTINE_CODE_DOMAIN: &str = "blaze v2 coroutine code hash 1";

/// Why an item has no hash: a part of it is not of a program rustc accepts (an error type,
/// MIR that failed to lower, a macro without rules). A build stops at it, naming the item.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Unhashable {
    pub item: ItemId,
    pub what: &'static str,
}

impl Unhashable {
    /// The error a build stops with: the item by kind and path, what could not be hashed, and
    /// for MIR that failed to lower, the error lowering it gave.
    pub fn describe(&self, db: &Db) -> String {
        // alloc: the message a build stops with, written once at the edge.
        let mut said = format!(
            "blaze-items: {:?} `{}` has no hash: {}",
            self.item.kind(),
            key::path(db, self.item),
            self.what
        );
        if self.what == form::Missed::NoMir.what()
            && let Some(error) = form::Cx::new(db).mir_error(self.item)
        {
            _ = write!(said, " ({error})");
        }
        said
    }
}

impl fmt::Display for Unhashable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {:?} has no hash: {}", self.item.kind(), self.item, self.what)
    }
}

impl std::error::Error for Unhashable {}

#[blaze_query::tracked(returns(ref))]
pub(crate) fn interface_form(db: &Db, id: ItemId) -> form::Sub {
    form::Cx::new(db).interface(id)
}

#[blaze_query::tracked(returns(ref))]
pub(crate) fn signature_form(db: &Db, id: ItemId) -> form::Sub {
    form::Cx::leak_free(db).signature(id)
}

#[blaze_query::tracked(returns(ref))]
pub(crate) fn body_form(db: &Db, id: ItemId) -> Option<form::Sub> {
    form::Cx::new(db).body(id).map(|code| code.sub)
}

/// Which graph a hash is over, and so what its refs are filled with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Graph {
    Interface,
    Layout,
    Deep,
    /// The signature forms, each ref by its signature hash: no body is read.
    Signature,
}

impl Graph {
    /// Whether `id` has a hash in this graph: only a type has a layout.
    pub(crate) fn node(self, id: ItemId) -> bool {
        self != Graph::Layout || matches!(id, ItemId::Adt(_))
    }

    fn domain(self) -> &'static str {
        match self {
            Graph::Interface => INTERFACE_DOMAIN,
            Graph::Layout => LAYOUT_DOMAIN,
            Graph::Deep => DEEP_DOMAIN,
            Graph::Signature => SIGNATURE_DOMAIN,
        }
    }

    /// The form every ref of `id` in this graph is a slot of, but a body's.
    fn form(self, db: &Db, id: ItemId) -> &form::Sub {
        match self {
            Graph::Interface | Graph::Layout | Graph::Deep => interface_form(db, id),
            Graph::Signature => signature_form(db, id),
        }
    }

    /// Appends the refs of `id` in this graph to `out`, in slot order.
    fn refs_into<A: Allocator>(self, db: &Db, id: ItemId, out: &mut Vec<ItemId, A>) {
        out.extend_from_slice(&self.form(db, id).refs);
        if self == Graph::Deep
            && let Some(body) = body_form(db, id)
        {
            out.extend_from_slice(&body.refs);
        }
    }

    /// Whether an edge to `to` can be inside a component of an item of `krate`: a ref to
    /// another crate never is, and the layout of a type depends on other types' layouts only.
    fn inside(self, db: &Db, krate: base_db::Crate, to: ItemId) -> bool {
        (self != Graph::Layout || matches!(to, ItemId::Adt(_))) && to.krate(db) == krate
    }

    /// What `id` is apart from its refs.
    fn own(self, db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
        let interface = self.form(db, id);
        missed(interface, id)?;
        match self {
            Graph::Interface | Graph::Signature => Ok(interface.digest),
            Graph::Layout => {
                let target = match &id.krate(db).workspace_data(db).target {
                    Ok(target) => target,
                    Err(_) => {
                        return Err(Unhashable { item: id, what: "its crate has no target" });
                    }
                };
                let mut h = Hasher::new(LAYOUT_DOMAIN);
                h.update(b"own")
                    .id(&interface.digest)
                    .part(target.data_layout.as_bytes())
                    .part(arch_name(target.arch).as_bytes());
                Ok(h.finish())
            }
            Graph::Deep => {
                let mut h = Hasher::new(DEEP_DOMAIN);
                h.update(b"own").id(&interface.digest);
                match body_form(db, id) {
                    Some(body) => {
                        missed(body, id)?;
                        h.update(&[1]).id(&body.digest)
                    }
                    None => h.update(&[0]),
                };
                Ok(h.finish())
            }
        }
    }

    /// The hash a ref outside the component is filled with.
    fn fill(self, db: &Db, to: ItemId) -> Result<ContentId, Unhashable> {
        match (self, to) {
            (Graph::Interface, _) => interface_of(db, to),
            (Graph::Layout, ItemId::Adt(_)) => layout_of(db, to),
            (Graph::Layout, _) => interface_of(db, to),
            (Graph::Deep, _) => deep_of(db, to),
            (Graph::Signature, _) => signature_of(db, to),
        }
    }
}

/// The arch as a layout hash spells it: its `Debug` name, written out so no string is built.
fn arch_name(arch: base_db::target::Arch) -> &'static str {
    match arch {
        base_db::target::Arch::Wasm32 => "Wasm32",
        base_db::target::Arch::Wasm64 => "Wasm64",
        base_db::target::Arch::Other => "Other",
    }
}

fn missed(sub: &form::Sub, item: ItemId) -> Result<(), Unhashable> {
    match sub.missed {
        Some(why) => Err(Unhashable { item, what: why.what() }),
        None => Ok(()),
    }
}

#[blaze_query::tracked(returns(copy))]
fn interface_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Interface, id)
}

#[blaze_query::tracked(returns(copy))]
fn layout_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Layout, id)
}

#[blaze_query::tracked(returns(copy))]
fn deep_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Deep, id)
}

#[blaze_query::tracked(returns(copy))]
fn signature_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Signature, id)
}

#[blaze_query::tracked(returns(copy))]
fn code_of(db: &Db, id: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let Some(body) = body_form(db, id) else { return Ok(None) };
    let interface = interface_of(db, id)?;
    let mut h = Hasher::new(CODE_DOMAIN);
    h.id(&interface);
    Ok(Some(filled_into(db, &mut h, body, id)?))
}

/// A form's digest with each ref filled by its interface hash: what a fact that spells a form
/// (a closure's captures, a field's type) stands for.
pub(crate) fn filled(
    db: &Db,
    domain: &'static str,
    sub: &form::Sub,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    filled_into(db, &mut Hasher::new(domain), sub, owner)
}

fn filled_into(
    db: &Db,
    h: &mut Hasher,
    sub: &form::Sub,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    missed(sub, owner)?;
    h.id(&sub.digest).update(&(sub.refs.len() as u64).to_le_bytes());
    for &to in sub.refs.iter() {
        h.id(&interface_of(db, to)?);
    }
    Ok(h.finish())
}

/// [`filled`] with each ref by its signature hash, for a form a [`form::Cx::leak_free`] wrote:
/// what a body's inference may ask without inferring any body.
pub(crate) fn signature_filled(
    db: &Db,
    domain: &'static str,
    sub: &form::Sub,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    missed(sub, owner)?;
    let mut h = Hasher::new(domain);
    h.id(&sub.digest).update(&(sub.refs.len() as u64).to_le_bytes());
    for &to in sub.refs.iter() {
        h.id(&signature_of(db, to)?);
    }
    Ok(h.finish())
}

/// Preimages written back to back, each hashed alone in one batched call: bit for bit what
/// one [`Hasher`] per preimage gives, as `update` and `id` write the bytes a `Hasher` would.
struct Batch<'f> {
    bytes: Vec<u8, &'f Frame>,
    ends: Vec<usize, &'f Frame>,
}

impl<'f> Batch<'f> {
    fn new(frame: &'f Frame) -> Batch<'f> {
        Batch { bytes: Vec::new_in(frame), ends: Vec::new_in(frame) }
    }

    fn update(&mut self, bytes: &[u8]) -> &mut Batch<'f> {
        self.bytes.extend_from_slice(bytes);
        self
    }

    fn id(&mut self, id: &ContentId) -> &mut Batch<'f> {
        self.update(&id.0)
    }

    fn word(&mut self, n: usize) -> &mut Batch<'f> {
        self.update(&(n as u64).to_le_bytes())
    }

    /// Ends the preimage being written.
    fn end(&mut self) {
        self.ends.push(self.bytes.len());
    }

    /// Each ended preimage's hash, in order; the batch is left empty.
    fn hash(&mut self, domain: &'static str) -> Vec<ContentId> {
        let frame = *self.bytes.allocator();
        let mut inputs: Vec<&[u8], &Frame> = Vec::with_capacity_in(self.ends.len(), frame);
        let mut start = 0;
        for &end in self.ends.iter() {
            inputs.push(&self.bytes[start..end]);
            start = end;
        }
        // alloc: one hash per member, the colors a round refines, made by `of_many`.
        let hashes = ContentId::of_many(domain, &inputs);
        drop(inputs);
        self.bytes.clear();
        self.ends.clear();
        hashes
    }

    /// The hash of everything written, as one preimage.
    fn hash_one(&self, domain: &'static str) -> ContentId {
        ContentId::of(domain, &self.bytes)
    }
}

/// The hash of `root` in `graph`: of its component, as the module doc says. A component of
/// one item with no ref to itself is streamed from its forms; any other is hashed once for all
/// its members ([`unit`]), each member's hash being the unit's and its position.
fn component_hash(db: &Db, graph: Graph, root: ItemId) -> Result<ContentId, Unhashable> {
    let first = match members(db, graph, root) {
        Members::Alone => match alone(db, graph, root)? {
            Some(hash) => return Ok(hash),
            None => root,
        },
        Members::Table(members) => members[0],
        Members::Searched(members) => members[0],
    };
    let unit = match graph {
        Graph::Interface => interface_unit(db, first),
        Graph::Layout => layout_unit(db, first),
        Graph::Deep => deep_unit(db, first),
        Graph::Signature => signature_unit(db, first),
    }
    .as_ref()
    .map_err(|it| *it)?;
    let at = unit.members.binary_search(&root).expect("an item is a member of its component's unit");
    let mut h = Hasher::new(graph.domain());
    h.update(b"member").id(&unit.whole).update(&u64::from(unit.ranks[at]).to_le_bytes());
    Ok(h.finish())
}

/// A component hashed as one unit: its members sorted by id, the hash of the whole, and each
/// member's position in the content order the whole is spelled in.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Unit {
    members: Box<[ItemId]>,
    whole: ContentId,
    ranks: Box<[u32]>,
}

/// The unit of the component whose first member (by id) is `first`: asked once per component,
/// not once per member.
#[blaze_query::tracked(returns(ref))]
fn interface_unit(db: &Db, first: ItemId) -> Result<Unit, Unhashable> {
    unit(db, Graph::Interface, first)
}

#[blaze_query::tracked(returns(ref))]
fn layout_unit(db: &Db, first: ItemId) -> Result<Unit, Unhashable> {
    unit(db, Graph::Layout, first)
}

#[blaze_query::tracked(returns(ref))]
fn deep_unit(db: &Db, first: ItemId) -> Result<Unit, Unhashable> {
    unit(db, Graph::Deep, first)
}

#[blaze_query::tracked(returns(ref))]
fn signature_unit(db: &Db, first: ItemId) -> Result<Unit, Unhashable> {
    unit(db, Graph::Signature, first)
}

fn unit(db: &Db, graph: Graph, first: ItemId) -> Result<Unit, Unhashable> {
    // alloc: a unit's members, kept in its memo.
    let members: Box<[ItemId]> = match members(db, graph, first) {
        Members::Alone => Box::new([first]), // alloc: as above, an item with a ref to itself.
        Members::Table(members) => Box::from(members),
        Members::Searched(members) => members.into_boxed_slice(),
    };
    let n = members.len();
    let domain = graph.domain();
    let inside = |to: ItemId| members.binary_search(&to).ok();
    // alloc: what the members are spelled from is gathered while their refs are filled, and a
    // fill is the hash of another component, which recurses as deep as the graph of refs: a
    // frame (which nests a bounded depth) opens only once every fill is in.
    let keys: Vec<crate::ItemKey> = members.iter().map(|&id| key::item_key(db, id)).collect(); // alloc: as above.
    // Members are spelled and filled in key order, so the member an error names is the same
    // in every build whatever ids the database gave them.
    // alloc: as above.
    let mut by_key: Vec<usize> = (0..n).collect();
    by_key.sort_unstable_by_key(|&at| keys[at]);
    // alloc: as above.
    let mut owns = vec![ContentId([0; 32]); n];
    for &at in by_key.iter() {
        owns[at] = graph.own(db, members[at])?;
    }
    // Every member's refs in one list, member `at`'s at `starts[at]..starts[at + 1]`, and what
    // each ref outside the component is filled with, `None` for one inside it.
    // alloc: as above.
    let mut starts = Vec::with_capacity(n + 1);
    // alloc: as above; one list for every member's refs.
    let mut refs: Vec<ItemId> = Vec::new();
    starts.push(0);
    for &member in members.iter() {
        graph.refs_into(db, member, &mut refs);
        starts.push(refs.len());
    }
    let of = |at: usize| starts[at]..starts[at + 1];
    // alloc: as above.
    let mut fills: Vec<Option<ContentId>> = vec![None; refs.len()];
    for &at in by_key.iter() {
        for slot in of(at) {
            if inside(refs[slot]).is_none() {
                fills[slot] = Some(graph.fill(db, refs[slot])?);
            }
        }
    }

    let (whole, ranks) = frame::task(Tag::Other, |frame| {
        let mut b = Batch::new(frame);
        // Order the members by content: their own content and what is outside, refined by the
        // content of the members they refer to until the classes stop splitting; members still
        // alike are ordered by key, which only matters where swapping them hashes alike. Each
        // round hashes every member in one batch.
        for at in 0..n {
            b.update(b"shape").id(&owns[at]);
            spell_fills(&mut b, &refs[of(at)], &fills[of(at)], |_| 0);
            b.end();
        }
        let mut color = b.hash(domain);
        let mut classes = distinct(&color, frame);
        for _ in 0..n {
            for at in 0..n {
                b.update(b"refine").id(&color[at]);
                for to in &refs[of(at)] {
                    if let Some(at) = inside(*to) {
                        b.id(&color[at]);
                    }
                }
                b.end();
            }
            let next = b.hash(domain);
            let next_classes = distinct(&next, frame);
            color = next;
            if next_classes == classes {
                break;
            }
            classes = next_classes;
        }
        let mut order: Vec<usize, &Frame> = Vec::with_capacity_in(n, frame);
        order.extend(0..n);
        order.sort_by(|&a, &b| color[a].0.cmp(&color[b].0).then(keys[a].cmp(&keys[b])));
        // alloc: each member's position, kept in the unit's memo.
        let mut rank = vec![0u32; n].into_boxed_slice();
        for (at, &member) in order.iter().enumerate() {
            rank[member] = u32::try_from(at).expect("blaze-items: a component past u32::MAX items");
        }

        b.update(b"cycle").word(n);
        for &at in order.iter() {
            b.id(&owns[at]);
            spell_fills(&mut b, &refs[of(at)], &fills[of(at)], |to| {
                rank[inside(to).expect("a ref without a fill is to a member")] as usize
            });
        }
        (b.hash_one(domain), rank)
    });
    Ok(Unit { members, whole, ranks })
}

/// The hash of `only`, a component of one item, when none of its refs is to itself, streamed
/// from its forms as [`component_hash`] would spell it; `None` for one that is.
fn alone(db: &Db, graph: Graph, only: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let own = graph.own(db, only)?;
    let interface: &[ItemId] = &graph.form(db, only).refs;
    let body: &[ItemId] = match graph {
        Graph::Deep => body_form(db, only).as_ref().map_or(&[][..], |it| &it.refs[..]),
        Graph::Interface | Graph::Layout | Graph::Signature => &[],
    };
    let refs = || interface.iter().chain(body).copied();
    if refs().any(|to| to == only) {
        return Ok(None);
    }
    let mut h = Hasher::new(graph.domain());
    h.update(b"item").id(&own).update(&((interface.len() + body.len()) as u64).to_le_bytes());
    for to in refs() {
        h.update(&[1]).id(&graph.fill(db, to)?);
    }
    Ok(Some(h.finish()))
}

fn spell_fills(
    b: &mut Batch<'_>,
    refs: &[ItemId],
    fills: &[Option<ContentId>],
    inside: impl Fn(ItemId) -> usize,
) {
    b.word(refs.len());
    for (&to, fill) in refs.iter().zip(fills) {
        match fill {
            Some(fill) => b.update(&[1]).id(fill),
            None => b.update(&[0]).word(inside(to)),
        };
    }
}

fn distinct(color: &[ContentId], frame: &Frame) -> usize {
    let mut sorted: Vec<&ContentId, &Frame> = Vec::with_capacity_in(color.len(), frame);
    sorted.extend(color.iter());
    sorted.sort_unstable_by_key(|it| it.0);
    sorted.dedup();
    sorted.len()
}

/// The members of an item's component.
pub(crate) enum Members<'db> {
    /// The item alone.
    Alone,
    /// Two or more, sorted, from the crate's table.
    Table(&'db [ItemId]),
    /// Two or more, sorted, from a search of its own.
    Searched(Vec<ItemId>),
}

/// The component of `root` in `graph` within its crate.
///
/// In the deep graph of a local crate, whose every body is lowered and hashed, the components
/// are found by one search over all its items ([`Components`]). Everywhere else an item is
/// searched from alone, so a hash reads the forms of what its item reaches and no more: an
/// interface hash is asked while a body is being solved (a goal's facts), and an interface
/// form reads the inference of a body that leaks into it, so a search over every item of the
/// crate would reach the body being solved. A dependency's items are hashed only as far as
/// something reaches them. An item the table has not reached (an item in a body that no item
/// outside one refers to) is searched from alone too, past every item the table has: those
/// cannot reach it.
pub(crate) fn members(db: &Db, graph: Graph, root: ItemId) -> Members<'_> {
    let krate = root.krate(db);
    let table = (graph == Graph::Deep && krate.data(db).origin.is_local())
        .then(|| components(db, graph, krate));
    if let Some(found) = table.and_then(|it| it.get(root)) {
        return match found {
            [] => Members::Alone,
            members => Members::Table(members),
        };
    }
    let mut members = searched(db, graph, krate, table, root);
    if members.len() == 1 {
        return Members::Alone;
    }
    members.sort_unstable();
    Members::Searched(members)
}

/// The members of `root`'s component, by a search from it that stops at every item `known`
/// has. Unsorted.
pub(crate) fn searched(
    db: &Db,
    graph: Graph,
    krate: base_db::Crate,
    known: Option<&Components>,
    root: ItemId,
) -> Vec<ItemId> {
    frame::task(Tag::Other, |frame| {
        // alloc: the component outlives the search's frame.
        let mut members = Vec::new();
        Tarjan::new(db, graph, krate, known, frame).search(root, &mut |component: &[ItemId]| {
            if component[0] == root {
                members.extend_from_slice(component);
            }
        });
        members
    })
}

/// A local crate's components in one graph: every item of the crate and every item they reach
/// in it, with the members of each component of two or more. Numbered by first member, each
/// sorted, so two searches that find the same components give equal tables whatever order
/// they met the items in, and a table an edit leaves as it was stops what reads it.
#[derive(PartialEq, Eq, Debug, Default)]
pub(crate) struct Components {
    /// Each item searched and its component, [`ALONE`] for an item by itself.
    of: HashMap<ItemId, u32>,
    /// Component `c`'s members are `members[starts[c]..starts[c + 1]]`.
    members: Box<[ItemId]>,
    starts: Box<[u32]>,
}

const ALONE: u32 = u32::MAX;

impl Components {
    /// The members of `id`'s component, empty for an item by itself; `None` for an item not
    /// searched.
    pub(crate) fn get(&self, id: ItemId) -> Option<&[ItemId]> {
        match *self.of.get(&id)? {
            ALONE => Some(&[]),
            c => Some(&self.members[self.starts[c as usize] as usize..self.starts[c as usize + 1] as usize]),
        }
    }

    pub(crate) fn contains(&self, id: ItemId) -> bool {
        self.of.contains_key(&id)
    }
}

fn components(db: &Db, graph: Graph, krate: base_db::Crate) -> &Components {
    match graph {
        Graph::Deep => deep_components(db, krate),
        Graph::Interface | Graph::Layout | Graph::Signature => {
            unreachable!("only the deep graph is searched a crate at a time (see `members`)")
        }
    }
}

#[blaze_query::tracked(returns(ref))]
fn deep_components(db: &Db, krate: base_db::Crate) -> Components {
    find_components(db, Graph::Deep, krate)
}

/// The refs of an item in the deep graph that stay in its crate: a body edit that calls
/// nothing new gives the same list, and leaves [`deep_components`] as it was.
#[blaze_query::tracked(returns(deref))]
fn deep_refs(db: &Db, id: ItemId) -> Box<[ItemId]> {
    let krate = id.krate(db);
    // alloc: a memo's list, at its final length.
    let mut refs = Vec::new();
    Graph::Deep.refs_into(db, id, &mut refs);
    refs.retain(|&to| Graph::Deep.inside(db, krate, to));
    refs.into_boxed_slice()
}

/// Tarjan's algorithm from every item of `krate` not yet reached: linear in the items and refs
/// of the crate, where a search per item is the sum of what each reaches.
pub(crate) fn find_components(db: &Db, graph: Graph, krate: base_db::Crate) -> Components {
    frame::task(Tag::Other, |frame| {
        let mut tarjan = Tarjan::new(db, graph, krate, None, frame);
        // Every component of two or more, as a range of `found`.
        let mut found: Vec<ItemId, &Frame> = Vec::new_in(frame);
        let mut cycles: Vec<(usize, usize), &Frame> = Vec::new_in(frame);
        let mut record = |component: &[ItemId]| {
            if component.len() > 1 {
                let start = found.len();
                found.extend_from_slice(component);
                found[start..].sort_unstable();
                cycles.push((start, found.len()));
            }
        };
        for id in key::crate_item_ids(db, krate) {
            if graph.node(id) && !tarjan.index.contains_key(&id) {
                tarjan.search(id, &mut record);
            }
        }
        cycles.sort_unstable_by_key(|&(start, _)| found[start]);
        let mut of = HashMap::default();
        // alloc: the memo's map, one entry per item searched.
        of.reserve(tarjan.index.len());
        for &id in tarjan.index.keys() {
            of.insert(id, ALONE);
        }
        // alloc: the memo's lists, at their final lengths.
        let mut members = Vec::with_capacity(found.len());
        let mut starts = Vec::with_capacity(cycles.len() + 1);
        starts.push(0);
        for (c, &(start, end)) in cycles.iter().enumerate() {
            let c = u32::try_from(c).expect("blaze-items: more than u32::MAX components");
            for &id in &found[start..end] {
                of.insert(id, c);
            }
            members.extend_from_slice(&found[start..end]);
            starts.push(u32::try_from(members.len()).expect("blaze-items: a crate past u32::MAX items"));
        }
        Components { of, members: members.into_boxed_slice(), starts: starts.into_boxed_slice() }
    })
}

/// A node being searched from: `edges[next..end]` are its edges still to follow, and
/// `edges[start..]` is its when it is popped.
struct Visit {
    node: ItemId,
    at: usize,
    start: usize,
    next: usize,
    end: usize,
}

/// Tarjan's algorithm over the refs of one crate (iterative: a chain of refs may be long). The
/// edges of every frame on the work stack are one list: a frame's are its tail when it is
/// popped. Refs to items `known` has are not followed: they are in components found already.
struct Tarjan<'a, 'f> {
    db: &'a Db,
    graph: Graph,
    krate: base_db::Crate,
    known: Option<&'a Components>,
    // alloc: std's map takes no allocator; one per search.
    index: HashMap<ItemId, usize>,
    low: Vec<usize, &'f Frame>,
    on_stack: Vec<bool, &'f Frame>,
    stack: Vec<ItemId, &'f Frame>,
    edges: Vec<ItemId, &'f Frame>,
    work: Vec<Visit, &'f Frame>,
}

impl<'a, 'f> Tarjan<'a, 'f> {
    fn new(
        db: &'a Db,
        graph: Graph,
        krate: base_db::Crate,
        known: Option<&'a Components>,
        frame: &'f Frame,
    ) -> Self {
        Tarjan {
            db,
            graph,
            krate,
            known,
            index: HashMap::default(),
            low: Vec::new_in(frame),
            on_stack: Vec::new_in(frame),
            stack: Vec::new_in(frame),
            edges: Vec::new_in(frame),
            work: Vec::new_in(frame),
        }
    }

    fn visit(&mut self, node: ItemId) {
        let at = self.low.len();
        self.index.insert(node, at);
        self.low.push(at);
        self.on_stack.push(true);
        self.stack.push(node);
        let start = self.edges.len();
        match self.graph {
            Graph::Deep => self.edges.extend_from_slice(deep_refs(self.db, node)),
            Graph::Interface | Graph::Layout | Graph::Signature => {
                self.edges.extend_from_slice(&self.graph.form(self.db, node).refs);
                let mut kept = start;
                for read in start..self.edges.len() {
                    if self.graph.inside(self.db, self.krate, self.edges[read]) {
                        self.edges[kept] = self.edges[read];
                        kept += 1;
                    }
                }
                self.edges.truncate(kept);
            }
        }
        if let Some(known) = self.known {
            let mut kept = start;
            for read in start..self.edges.len() {
                if !known.contains(self.edges[read]) {
                    self.edges[kept] = self.edges[read];
                    kept += 1;
                }
            }
            self.edges.truncate(kept);
        }
        let end = self.edges.len();
        self.work.push(Visit { node, at, start, next: start, end });
    }

    /// Searches from `root`, which no search has reached yet, giving `found` each component as
    /// it is finished, its first member the one it was entered by.
    fn search(&mut self, root: ItemId, found: &mut impl FnMut(&[ItemId])) {
        self.visit(root);
        while let Some(visit) = self.work.last_mut() {
            if visit.next < visit.end {
                let to = self.edges[visit.next];
                visit.next += 1;
                let from = visit.at;
                match self.index.get(&to).copied() {
                    None => self.visit(to),
                    Some(at) if self.on_stack[at] => self.low[from] = self.low[from].min(at),
                    Some(_) => {}
                }
                continue;
            }
            let Visit { node, at, start, .. } = self.work.pop().expect("the node just looked at");
            self.edges.truncate(start);
            if let Some(parent) = self.work.last() {
                self.low[parent.at] = self.low[parent.at].min(self.low[at]);
            }
            if self.low[at] == at {
                let first = self
                    .stack
                    .iter()
                    .rposition(|&member| member == node)
                    .expect("a node stays on the stack until its component leaves it");
                found(&self.stack[first..]);
                for member in self.stack.drain(first..) {
                    self.on_stack[self.index[&member]] = false;
                }
            }
        }
    }
}

/// The interface hash of an item.
pub fn interface_hash(db: &Db, id: ItemId) -> Result<InterfaceHash, Unhashable> {
    interface_of(db, id).map(InterfaceHash)
}

/// The signature hash of an item: its interface hash without what bodies leak into it
/// ([`crate::SignatureHash`]). Writing it infers no body, so a body's inference may ask it of
/// any item, its own owner included.
pub fn signature_hash(db: &Db, id: ItemId) -> Result<SignatureHash, Unhashable> {
    signature_of(db, id).map(SignatureHash)
}

/// The deep layout hash of a type.
pub fn layout_hash(db: &Db, adt: AdtId) -> Result<LayoutHash, Unhashable> {
    layout_of(db, ItemId::Adt(adt)).map(LayoutHash)
}

/// The code hash of an item with a body, `None` for one without.
pub fn code_hash(db: &Db, id: ItemId) -> Result<Option<CodeHash>, Unhashable> {
    Ok(code_of(db, id)?.map(CodeHash))
}

pub fn deep_hash(db: &Db, id: ItemId) -> Result<DeepHash, Unhashable> {
    deep_of(db, id).map(DeepHash)
}

pub fn hashes(db: &Db, id: ItemId) -> Result<ItemHashes, Unhashable> {
    Ok(ItemHashes {
        kind: id.kind(),
        interface: interface_hash(db, id)?,
        layout: match id {
            ItemId::Adt(adt) => Some(layout_hash(db, adt)?),
            _ => None,
        },
        code: code_hash(db, id)?,
        deep: deep_hash(db, id)?,
    })
}

/// The item a closure or coroutine is written in.
fn owner_of<'db>(db: &'db Db, loc: InternedClosure<'db>) -> ItemId {
    match loc.owner {
        InferBodyId::DefWithBodyId(body) => ItemId::of_body(db, body),
        InferBodyId::AnonConstId(anon) => key::generic_def_item(anon.loc(db).owner.generic_def(db)),
    }
}

/// The code hash of one closure, for its codegen alone: its own MIR, signature and captures
/// (#906), each ref by its interface hash.
pub fn closure_code<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Result<CodeHash, Unhashable> {
    closure_code_of(db, closure).map(CodeHash)
}

#[blaze_query::tracked(returns(copy))]
fn closure_code_of<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).closure_code(closure).sub;
    filled(db, CLOSURE_CODE_DOMAIN, &sub, owner_of(db, closure.loc(db)))
}

/// The code hash of one coroutine (an `async` block or fn body, a `gen` block), with its
/// saved locals.
pub fn coroutine_code<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<CodeHash, Unhashable> {
    coroutine_code_of(db, coroutine).map(CodeHash)
}

#[blaze_query::tracked(returns(copy))]
fn coroutine_code_of<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).coroutine_code(coroutine).sub;
    filled(db, COROUTINE_CODE_DOMAIN, &sub, owner_of(db, coroutine.loc(db)))
}
