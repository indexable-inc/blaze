//! The four hashes of an item (docs/v2/plan/design/item-hash.md section 6.1): its forms (`form`) with every
//! slot filled by the hash of what it refers to.
//!
//! - **interface**: the interface form, each ref by its interface hash.
//! - **layout** (of a type): the interface form and the target's data layout, each type it
//!   refers to by its layout hash and anything else (a trait in a bound) by its interface
//!   hash: a field's type changing size changes it, transitively.
//! - **code** (of an item with a body): its interface hash and its body form, each ref by its
//!   interface hash, so a callee's body edit leaves it as it was. A constant the body names
//!   with no generic parameter left is spelled with the value it evaluates to as well
//!   (`form::Cx::values`): `<A as Tr>::K` spells `A` by its name-free interface, which tells
//!   nothing of the impl `A` selects.
//! - **deep**: the interface and body forms, each ref by its deep hash, for what inlines.
//! - **signature**: the signature form (the interface form with a marker in place of what a
//!   body leaks into it), each ref by its signature hash: what a body's identity names an
//!   item by, which no body's inference may go into.
//!
//! Items that refer to each other are hashed as one unit, as Unison does: the members of a
//! strongly connected component of the graph of refs are ordered by their content, each ref
//! inside it is the position of its target, and each member's hash is the unit's and its
//! position. Unlike Unison, what validates a type-check result names each ref by its target's
//! key as well (D51): the signature hash ([`Graph::keyed`]: from outside a component by key and
//! hash, inside it by position and key) and a fact's form ([`named_filled`]). Rust's types are
//! nominal: `struct Meters(u32)` and `struct Feet(u32)` have one hash and are two types, so
//! `f(Meters(1))` type-checks against `fn f(_: Meters)` and not against `fn f(_: Feet)`. The
//! interface, layout, code and deep hashes stay name-free: what codegen keys a template by is
//! what the code does, and two types of one shape compile alike; which items a compiled body
//! calls is bound by key beside its code ([`crate::bound`], the instance keys of
//! `hir-ty-identity`), never inside it. Components are found by Tarjan's algorithm over the refs of one crate (a cycle
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
use std::alloc::Global;

use base_db::CrateKey;

use crate::{
    CodeHash, DeepHash, InterfaceHash, ItemHashes, ItemId, ItemKey, LayoutHash, SignatureHash,
    form, key,
};

// Version 2 of the signature hash: a ref names its target by key as well as by hash (D51), so
// no row kept under a name-free signature hash is found by one. The interface, layout, code and
// deep hashes are name-free, as in version 1.
pub const INTERFACE_DOMAIN: &str = "blaze v2 interface hash 1";
pub const SIGNATURE_DOMAIN: &str = "blaze v2 signature hash 2";
pub const LAYOUT_DOMAIN: &str = "blaze v2 layout hash 1";
pub const CODE_DOMAIN: &str = "blaze v2 code hash 1";
pub const DEEP_DOMAIN: &str = "blaze v2 deep hash 1";
pub const CLOSURE_CODE_DOMAIN: &str = "blaze v2 closure code hash 1";
pub const COROUTINE_CODE_DOMAIN: &str = "blaze v2 coroutine code hash 1";
/// A ref named by its target's key and hash ([`keyed`]).
const KEYED_REF_DOMAIN: &str = "blaze v2 keyed ref 1";

/// Why an item has no hash: a part of it is not of a program rustc accepts (an error type,
/// MIR that failed to lower, a macro without rules), or it was read before the proc macros its
/// crate can expand were built. A build stops at it, naming the item.
///
/// It holds stable keys, never ids: a refusal is kept (in a query's value, a hash column, a
/// pending build's error) across commits, and an id is dead after the commit that drops its
/// arena. Whoever says it takes the ids again from the snapshot it is said in
/// ([`Unhashable::describe`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Unhashable {
    pub of: Unhashed,
    pub what: &'static str,
    /// For a read before proc macros were built ([`form::Missed::MacroPending`]): the
    /// proc-macro crate found unbuilt when it was refused, kept so that the error names it
    /// whenever it is said, after that build too.
    pub pending: Option<CrateKey>,
}

/// What has no hash, by its stable key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Unhashed {
    /// An item.
    Item(ItemKey),
    /// What a crate reads of the crates it loads: the impls and traits its `Implements`,
    /// `Method` and `Vtable` facts cover, or its code, at the start of a job of it.
    Crate(CrateKey),
}

impl Unhashed {
    /// `item`, by its key.
    pub fn item(db: &Db, item: ItemId) -> Unhashed {
        Unhashed::Item(key::item_key(db, item))
    }

    /// `krate`, by its key.
    pub fn krate(db: &Db, krate: base_db::Crate) -> Unhashed {
        Unhashed::Crate(krate.key(db))
    }
}

/// The item `key` names in `db`'s snapshot, if it has one.
fn item_in(db: &Db, key: ItemKey) -> Option<ItemId> {
    match key::resolve(db, key)? {
        key::Resolved::Item(item) => Some(item),
        _ => None,
    }
}

/// Writes `krate` by name in `db`'s snapshot, or by its key where the snapshot no longer has
/// it, to `out`.
fn write_crate(db: &Db, krate: CrateKey, out: &mut String) {
    _ = match db.crate_at(krate) {
        Some(it) => write!(out, "{}", it.named(db)),
        None => write!(out, "#{} (not in this snapshot)", krate.index()),
    };
}

impl Unhashable {
    /// `item` has no hash, for `what`.
    pub(crate) fn of(db: &Db, item: ItemId, what: &'static str) -> Unhashable {
        Unhashable { of: Unhashed::item(db, item), what, pending: None }
    }

    /// The error a build stops with: the item by kind and path (or the crate), what could not
    /// be hashed, and for MIR that failed to lower, the error lowering it gave. The item and
    /// crates are taken from their keys in `db`'s snapshot, whichever it is; one the snapshot
    /// no longer has is named by its key, and the message says so.
    #[blaze::renders_positions]
    pub fn describe(&self, db: &Db) -> String {
        // alloc: the message a build stops with, written once at the edge.
        let mut said = String::new();
        match self.of {
            Unhashed::Item(named) => match item_in(db, named) {
                Some(item) => _ = write!(
                    said,
                    "blaze-items: {:?} `{}` has no hash: {}",
                    item.kind(),
                    key::path(db, item),
                    self.what
                ),
                None => _ = write!(
                    said,
                    "blaze-items: the item of key {:x} (not in this snapshot) has no hash: {}",
                    named.0, self.what
                ),
            },
            Unhashed::Crate(krate) => {
                said.push_str("blaze-items: what crate `");
                write_crate(db, krate, &mut said);
                _ = write!(said, "` reads of the crates it loads has no hash: {}", self.what);
            }
        }
        if let Unhashed::Item(named) = self.of
            && let Some(item) = item_in(db, named)
        {
            if self.what == form::Missed::NoMir.what()
                && let Some(error) = form::Cx::new(db).mir_error(item)
            {
                _ = write!(said, " ({error})");
            }
            if self.what == form::Missed::ErrorType.what()
                && let Some(site) = form::Cx::new(db).error_type_site(item)
            {
                _ = write!(said, " ({site})");
            }
        }
        if let Some(pending) = self.pending {
            said.push_str(" (`");
            match self.of {
                Unhashed::Item(named) => match key::crate_of_key(db, &named.0) {
                    Some(it) => _ = write!(said, "{}", it.named(db)),
                    None => _ = write!(said, "the crate of key {:x}", named.0),
                },
                Unhashed::Crate(krate) => write_crate(db, krate, &mut said),
            }
            said.push_str("` was read before the proc macros of `");
            write_crate(db, pending, &mut said);
            said.push_str(
                "` were built, whose calls expand to nothing until they are; a read of it must \
                 wait on that build)",
            );
        }
        said
    }

    /// Whether the miss is MIR that failed to lower.
    pub fn missed_mir(&self) -> bool {
        matches!(self.of, Unhashed::Item(_)) && self.what == form::Missed::NoMir.what()
    }

    /// For an item missed at MIR that failed to lower, the item, the error lowering gave, as
    /// data, and the body it was in: `None` for the item's own body, else the first closure or coroutine in it
    /// that failed. `None` for any other miss. What reads an item's hash can so give a typed
    /// reason where lowering had one (a call of an intrinsic missing from the table is the
    /// caller's `UnknownIntrinsic`) and name the body that failed, not its owner. A closure's or
    /// coroutine's code names its owner item ([`closure_code`], [`coroutine_code`]), so for one
    /// of those this is the owner's first failure, which may be another body's: read its own
    /// `mir_body_for_closure` / `mir_body_for_coroutine` instead.
    pub fn mir_lowering_error<'db>(
        &self,
        db: &'db Db,
    ) -> Option<(ItemId, Option<form::Nested<'db>>, hir_ty::mir::MirLowerError<'db>)> {
        let Unhashed::Item(named) = self.of else { return None };
        if !self.missed_mir() {
            return None;
        }
        let item = item_in(db, named)?;
        let (within, error) = form::Cx::new(db).mir_lowering_error(item)?;
        Some((item, within, error))
    }
}

/// `Ok` once every proc macro `krate`'s items can expand is built
/// ([`form::pending_proc_macro`]); otherwise [`form::Missed::MacroPending`] for `of`, naming
/// the first such proc-macro crate. The one check every read of a crate's half-expanded items
/// goes through: a form's hash, a fact's value, and a job of the crate as it starts
/// ([`macros_built`]). `of` is asked only for the refusal: a key is not made where nothing is
/// refused.
pub(crate) fn macros_built_for(
    db: &Db,
    krate: base_db::Crate,
    of: impl FnOnce() -> Unhashed,
) -> Result<(), Unhashable> {
    match form::pending_proc_macro(db, krate) {
        None => Ok(()),
        Some(pending) => Err(Unhashable {
            of: of(),
            what: form::Missed::MacroPending.what(),
            pending: Some(pending.key(db)),
        }),
    }
}

/// `Ok` once every proc macro `krate` can expand (its own dependencies' and those of every crate
/// it sees) is built. A job of `krate` (its check, its bodies' inference, its codegen) asks it
/// as it starts: a build builds those first, so an error here is a job ordered ahead of a proc
/// macro's build, never a state to go on from (rule 3: nothing of `krate` is inferred,
/// checked or kept on a half-expanded program).
pub fn macros_built(db: &Db, krate: base_db::Crate) -> Result<(), Unhashable> {
    macros_built_for(db, krate, || Unhashed::krate(db, krate))
}

impl fmt::Display for Unhashable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.of {
            Unhashed::Item(named) => {
                write!(f, "the item of key {:x} has no hash: {}", named.0, self.what)
            }
            Unhashed::Crate(krate) => {
                write!(f, "what crate #{} reads has no hash: {}", krate.index(), self.what)
            }
        }
    }
}

impl std::error::Error for Unhashable {}

#[blaze_memo::tracked(returns(copy))]
pub(crate) fn interface_form<'db>(db: &'db Db, id: ItemId) -> form::SubRef<'db> {
    form::Cx::new(db).interface(id).kept(db)
}

#[blaze_memo::tracked(returns(copy))]
pub(crate) fn signature_form<'db>(db: &'db Db, id: ItemId) -> form::SubRef<'db> {
    form::Cx::leak_free(db).signature(id).kept(db)
}

#[blaze_memo::tracked(returns(copy))]
pub(crate) fn body_form<'db>(db: &'db Db, id: ItemId) -> Option<form::SubRef<'db>> {
    form::Cx::new(db).body(id).map(|code| code.sub.kept(db))
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
    fn form(self, db: &Db, id: ItemId) -> form::SubRef<'_> {
        match self {
            Graph::Interface | Graph::Layout | Graph::Deep => interface_form(db, id),
            Graph::Signature => signature_form(db, id),
        }
    }

    /// Appends the refs of `id` in this graph to `out`, in slot order.
    fn refs_into<A: Allocator>(self, db: &Db, id: ItemId, out: &mut Vec<ItemId, A>) {
        out.extend_from_slice(self.form(db, id).refs);
        if self == Graph::Deep
            && let Some(body) = body_form(db, id)
        {
            out.extend_from_slice(body.refs);
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
        missed(db, interface, id)?;
        match self {
            Graph::Interface | Graph::Signature => Ok(interface.digest),
            Graph::Layout => {
                let target = match &id.krate(db).workspace_data(db).target {
                    Ok(target) => target,
                    Err(_) => {
                        return Err(Unhashable::of(db, id, "its crate has no target"));
                    }
                };
                let mut h = Hasher::new(LAYOUT_DOMAIN);
                h.update(b"own")
                    .id(&interface.digest)
                    .part(target.data_layout.as_bytes())
                    .part(arch_name(target.arch).as_bytes());
                // A profile's field order (`hir_ty::layout::field_order`) is an input of the
                // type's layout. Without a plan ranking it this adds nothing, so no hash moves.
                if let ItemId::Adt(adt) = id
                    && let Some(order) = hir_ty::layout::field_order::order_id(db, adt)
                {
                    h.update(b"field order").id(&order);
                }
                Ok(h.finish())
            }
            Graph::Deep => {
                let mut h = Hasher::new(DEEP_DOMAIN);
                h.update(b"own").id(&interface.digest);
                match body_form(db, id) {
                    Some(body) => {
                        missed(db, body, id)?;
                        h.update(&[1]).id(&body.digest)
                    }
                    None => h.update(&[0]),
                };
                Ok(h.finish())
            }
        }
    }

    /// Whether a ref in this graph names its target by key as well as by hash: the signature
    /// graph, what a body's identity names an item by (D51), so that two items alike but for
    /// their names (`struct Meters(u32)` and `struct Feet(u32)`) are two targets to whatever
    /// type-checks against one of them. The interface graph is what codegen reads beside an
    /// item's key (a callee's instance, an ADT's identity, a template's code), where a shape is
    /// all a compiled body depends on; a layout is the same for both; the deep graph is what
    /// inlines.
    fn keyed(self) -> bool {
        matches!(self, Graph::Signature)
    }

    /// The hash a ref outside the component is filled with.
    fn fill(self, db: &Db, to: ItemId) -> Result<ContentId, Unhashable> {
        match (self, to) {
            (Graph::Interface, _) => interface_of(db, to),
            (Graph::Layout, ItemId::Adt(_)) => layout_of(db, to),
            (Graph::Layout, _) => interface_of(db, to),
            (Graph::Deep, _) => deep_of(db, to),
            (Graph::Signature, _) => Ok(keyed(db, to, signature_of(db, to)?)),
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

/// Why `item`'s form `sub` has no hash, if it has none. A form of an item whose crate's proc
/// macros are pending names the proc-macro crate as it is refused (`Unhashable::pending`).
fn missed(db: &Db, sub: form::SubRef<'_>, item: ItemId) -> Result<(), Unhashable> {
    match sub.missed {
        Some(form::Missed::MacroPending) => {
            macros_built_for(db, item.krate(db), || Unhashed::item(db, item))?;
            // Written pending in this revision, and its crate's macros built in it: the form is
            // read with the check that wrote it, so this is not reached.
            Err(Unhashable::of(db, item, form::Missed::MacroPending.what()))
        }
        Some(why) => Err(Unhashable::of(db, item, why.what())),
        None => Ok(()),
    }
}

/// No boundary of the read classifier (`blaze_item_hash::reads`), on purpose: it folds in the
/// interface hash of every item it names, so no one fact stands for a read of it; what it
/// reads is classified one level down, fact by fact.
#[blaze_memo::tracked(returns(copy))]
fn interface_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Interface, id)
}

/// No boundary of the read classifier either, for the same reason: the layout hash folds in
/// the const values it refers to, which the surface's `Layout` row does not cover.
#[blaze_memo::tracked(returns(copy))]
fn layout_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Layout, id)
}

#[blaze_memo::tracked(returns(copy))]
fn deep_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Deep, id)
}

#[blaze_memo::tracked(returns(copy))]
fn signature_of(db: &Db, id: ItemId) -> Result<ContentId, Unhashable> {
    component_hash(db, Graph::Signature, id)
}

#[blaze_memo::tracked(returns(copy))]
fn code_of(db: &Db, id: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let Some(body) = body_form(db, id) else { return Ok(None) };
    let interface = interface_of(db, id)?;
    let mut h = Hasher::new(CODE_DOMAIN);
    h.id(&interface);
    Ok(Some(filled_into(db, &mut h, body, id, Names::Free)?))
}

/// Whether a filled form names each ref by its target's key as well as by its hash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Names {
    /// By hash alone: a code hash, what a template is kept under (what it does).
    Free,
    /// By key and hash (D51): a fact's value, which a body's stored type check is checked by.
    Keyed,
}

/// A body's code form's digest with each ref filled by its interface hash, name-free: a
/// closure's or coroutine's code, as [`code_of`] is an item's.
pub(crate) fn filled(
    db: &Db,
    domain: &'static str,
    sub: form::SubRef<'_>,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    filled_into(db, &mut Hasher::new(domain), sub, owner, Names::Free)
}

/// A form's digest with each ref filled by its key and interface hash: what a fact that spells
/// a form (a closure's captures, a field's type, a constant's type) stands for. A body that read
/// the fact type-checked against the items it names, so another item of the same shape in one's
/// place is another value (D51); what the body reads inside one of them is a fact of its own.
pub(crate) fn named_filled(
    db: &Db,
    domain: &'static str,
    sub: form::SubRef<'_>,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    filled_into(db, &mut Hasher::new(domain), sub, owner, Names::Keyed)
}

fn filled_into(
    db: &Db,
    h: &mut Hasher,
    sub: form::SubRef<'_>,
    owner: ItemId,
    names: Names,
) -> Result<ContentId, Unhashable> {
    missed(db, sub, owner)?;
    // A form other than the item's own (a field's, a closure's captures, an opaque's auto
    // traits) is written without the check `form::Cx::item_form` makes, so it is made here.
    macros_built_for(db, owner.krate(db), || Unhashed::item(db, owner))?;
    h.id(&sub.digest).update(&(sub.refs.len() as u64).to_le_bytes());
    for &to in sub.refs.iter() {
        let hash = interface_of(db, to)?;
        h.id(&match names {
            Names::Free => hash,
            Names::Keyed => keyed(db, to, hash),
        });
    }
    Ok(h.finish())
}

/// A ref to `to` whose hash is `hash`, named by `to`'s key as well (D51): a hash alone says
/// what the target is, not which item it is, and two nominal types of one shape have one
/// hash. A renamed or moved item keeps its own hashes; every hash that names it moves.
fn keyed(db: &Db, to: ItemId, hash: ContentId) -> ContentId {
    let mut h = Hasher::new(KEYED_REF_DOMAIN);
    h.id(&key::item_key(db, to).0).id(&hash);
    h.finish()
}

/// [`filled`] with each ref by its signature hash, for a form a [`form::Cx::leak_free`] wrote:
/// what a body's inference may ask without inferring any body.
pub(crate) fn signature_filled(
    db: &Db,
    domain: &'static str,
    sub: form::SubRef<'_>,
    owner: ItemId,
) -> Result<ContentId, Unhashable> {
    missed(db, sub, owner)?;
    macros_built_for(db, owner.krate(db), || Unhashed::item(db, owner))?;
    let mut h = Hasher::new(domain);
    h.id(&sub.digest).update(&(sub.refs.len() as u64).to_le_bytes());
    for &to in sub.refs.iter() {
        h.id(&keyed(db, to, signature_of(db, to)?));
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
    let at =
        unit.members.binary_search(&root).expect("an item is a member of its component's unit");
    let mut h = Hasher::new(graph.domain());
    h.update(b"member").id(&unit.whole).update(&u64::from(unit.ranks[at]).to_le_bytes());
    Ok(h.finish())
}

/// A component hashed as one unit: its members sorted by id, the hash of the whole, and each
/// member's position in the content order the whole is spelled in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Unit<'db> {
    members: &'db [ItemId],
    whole: ContentId,
    ranks: &'db [u32],
}

/// The unit of the component whose first member (by id) is `first`: asked once per component,
/// not once per member.
#[blaze_memo::tracked(returns(ref))]
pub(crate) fn interface_unit<'db>(db: &'db Db, first: ItemId) -> Result<Unit<'db>, Unhashable> {
    unit(db, Graph::Interface, first)
}

#[blaze_memo::tracked(returns(ref))]
fn layout_unit<'db>(db: &'db Db, first: ItemId) -> Result<Unit<'db>, Unhashable> {
    unit(db, Graph::Layout, first)
}

#[blaze_memo::tracked(returns(ref))]
fn deep_unit<'db>(db: &'db Db, first: ItemId) -> Result<Unit<'db>, Unhashable> {
    unit(db, Graph::Deep, first)
}

#[blaze_memo::tracked(returns(ref))]
fn signature_unit<'db>(db: &'db Db, first: ItemId) -> Result<Unit<'db>, Unhashable> {
    unit(db, Graph::Signature, first)
}

fn unit<'db>(db: &'db Db, graph: Graph, first: ItemId) -> Result<Unit<'db>, Unhashable> {
    let memo = blaze_memo::Database::memo(db);
    // The unit's members, kept in its run's arena.
    let members: &'db [ItemId] = match members(db, graph, first) {
        Members::Alone => blaze_memo::keep_slice(memo, &[first]),
        Members::Table(members) => blaze_memo::keep_slice(memo, members),
        Members::Searched(members) => blaze_memo::keep_slice(memo, &members),
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
            spell_fills(&mut b, &refs[of(at)], &fills[of(at)], |_| (0, None));
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
        let mut rank: Vec<u32, &Frame> = Vec::with_capacity_in(n, frame);
        rank.resize(n, 0);
        for (at, &member) in order.iter().enumerate() {
            rank[member] = u32::try_from(at).expect("blaze-items: a component past u32::MAX items");
        }

        // A ref to a member is its position and, in a keyed graph, the member's key: so two
        // members alike but for their names (ordered by key above) spell the whole apart, and a
        // member renamed moves every member's hash, as a ref from outside to it would.
        b.update(b"cycle").word(n);
        for &at in order.iter() {
            b.id(&owns[at]);
            spell_fills(&mut b, &refs[of(at)], &fills[of(at)], |to| {
                let to = inside(to).expect("a ref without a fill is to a member");
                (rank[to] as usize, graph.keyed().then_some(keys[to].0))
            });
        }
        // Each member's position, kept in the unit's run's arena.
        (b.hash_one(domain), blaze_memo::keep_slice(memo, &rank))
    });
    Ok(Unit { members, whole, ranks })
}

/// The hash of `only`, a component of one item, when none of its refs is to itself, streamed
/// from its forms as [`component_hash`] would spell it; `None` for one that is.
fn alone(db: &Db, graph: Graph, only: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let own = graph.own(db, only)?;
    let interface: &[ItemId] = graph.form(db, only).refs;
    let body: &[ItemId] = match graph {
        Graph::Deep => body_form(db, only).map_or(&[][..], |it| it.refs),
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
    inside: impl Fn(ItemId) -> (usize, Option<ContentId>),
) {
    b.word(refs.len());
    for (&to, fill) in refs.iter().zip(fills) {
        match fill {
            Some(fill) => b.update(&[1]).id(fill),
            None => match inside(to) {
                (at, None) => b.update(&[0]).word(at),
                (at, Some(key)) => b.update(&[2]).word(at).id(&key),
            },
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
    known: Option<Components<'_>>,
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Components<'a> {
    /// Each item searched and its component, [`ALONE`] for an item by itself, sorted by item.
    of: &'a [(ItemId, u32)],
    /// Component `c`'s members are `members[starts[c]..starts[c + 1]]`.
    members: &'a [ItemId],
    starts: &'a [u32],
}

const ALONE: u32 = u32::MAX;

impl<'a> Components<'a> {
    /// The members of `id`'s component, empty for an item by itself; `None` for an item not
    /// searched.
    pub(crate) fn get(self, id: ItemId) -> Option<&'a [ItemId]> {
        let at = self.of.binary_search_by_key(&id, |&(it, _)| it).ok()?;
        match self.of[at].1 {
            ALONE => Some(&[]),
            c => Some(
                &self.members
                    [self.starts[c as usize] as usize..self.starts[c as usize + 1] as usize],
            ),
        }
    }

    pub(crate) fn contains(self, id: ItemId) -> bool {
        self.of.binary_search_by_key(&id, |&(it, _)| it).is_ok()
    }

    /// The table with its columns kept in the current run's arena.
    fn kept<'db>(self, db: &'db Db) -> Components<'db> {
        let memo = blaze_memo::Database::memo(db);
        Components {
            of: blaze_memo::keep_slice(memo, self.of),
            members: blaze_memo::keep_slice(memo, self.members),
            starts: blaze_memo::keep_slice(memo, self.starts),
        }
    }
}

fn components(db: &Db, graph: Graph, krate: base_db::Crate) -> Components<'_> {
    match graph {
        Graph::Deep => deep_components(db, krate),
        Graph::Interface | Graph::Layout | Graph::Signature => {
            unreachable!("only the deep graph is searched a crate at a time (see `members`)")
        }
    }
}

#[blaze_memo::tracked(returns(copy))]
pub(crate) fn deep_components<'db>(db: &'db Db, krate: base_db::Crate) -> Components<'db> {
    find_components(db, Graph::Deep, krate, |found| found.kept(db))
}

/// The refs of an item in the deep graph that stay in its crate: a body edit that calls
/// nothing new gives the same list, and leaves [`deep_components`] as it was.
#[blaze_memo::tracked(returns(copy))]
pub(crate) fn deep_refs<'db>(db: &'db Db, id: ItemId) -> &'db [ItemId] {
    let krate = id.krate(db);
    frame::task(Tag::Other, |frame| {
        let mut refs: Vec<ItemId, &Frame> = Vec::new_in(frame);
        Graph::Deep.refs_into(db, id, &mut refs);
        refs.retain(|&to| Graph::Deep.inside(db, krate, to));
        blaze_memo::keep_slice(blaze_memo::Database::memo(db), &refs)
    })
}

/// Tarjan's algorithm from every item of `krate` not yet reached: linear in the items and refs
/// of the crate, where a search per item is the sum of what each reaches.
#[expect(
    blaze::narrowed_len,
    reason = "a component's start is a u32 index into the crate's cycle members, at most the crate's items, and a crate's items are u32-numbered, so the count fits"
)]
pub(crate) fn find_components<R>(
    db: &Db,
    graph: Graph,
    krate: base_db::Crate,
    then: impl FnOnce(Components<'_>) -> R,
) -> R {
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
        let mut of: Vec<(ItemId, u32), &Frame> = Vec::with_capacity_in(tarjan.index.len(), frame);
        of.extend(tarjan.index.keys().map(|&id| (id, ALONE)));
        of.sort_unstable_by_key(|&(id, _)| id);
        let mut members: Vec<ItemId, &Frame> = Vec::with_capacity_in(found.len(), frame);
        let mut starts: Vec<u32, &Frame> = Vec::with_capacity_in(cycles.len() + 1, frame);
        starts.push(0);
        for (c, &(start, end)) in cycles.iter().enumerate() {
            let c = u32::try_from(c).expect("blaze-items: more than u32::MAX components");
            for &id in &found[start..end] {
                let at = of
                    .binary_search_by_key(&id, |&(it, _)| it)
                    .expect("a member of a component was searched");
                of[at].1 = c;
            }
            members.extend_from_slice(&found[start..end]);
            starts.push(
                u32::try_from(members.len()).expect("blaze-items: a crate past u32::MAX items"),
            );
        }
        then(Components { of: &of, members: &members, starts: &starts })
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
    known: Option<Components<'a>>,
    // alloc: std's map takes no allocator; one per search.
    index: HashMap<ItemId, usize, Global>,
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
        known: Option<Components<'a>>,
        frame: &'f Frame,
    ) -> Self {
        Tarjan {
            db,
            graph,
            krate,
            known,
            index: HashMap::new_in(Global),
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
                self.edges.extend_from_slice(self.graph.form(self.db, node).refs);
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

#[blaze_memo::tracked(returns(copy))]
fn closure_code_of<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).closure_code(closure).sub;
    filled(db, CLOSURE_CODE_DOMAIN, sub.view(), owner_of(db, closure.loc(db)))
}

/// The code hash of one coroutine (an `async` block or fn body, a `gen` block), with its
/// saved locals.
pub fn coroutine_code<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<CodeHash, Unhashable> {
    coroutine_code_of(db, coroutine).map(CodeHash)
}

#[blaze_memo::tracked(returns(copy))]
fn coroutine_code_of<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = form::Cx::new(db).coroutine_code(coroutine).sub;
    filled(db, COROUTINE_CODE_DOMAIN, sub.view(), owner_of(db, coroutine.loc(db)))
}
