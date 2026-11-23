//! The portable type form: what a world's answer and the body's record hold (typeck.md,
//! "`TypeCtx`: what is asked of blaze-ty"). Items by key, generic parameters by index, children
//! before parents in one node column, lists of children in one side column. Becomes blaze-ty's
//! portable type table; what this crate needs of it is exactly this shape.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Malformed};
use crate::hash::{ContentHash, StableHasher};
use crate::ids::{ExprId, FloatTy, FnPtrHeader, IntTy, ItemKey, List, Mutbl, Name, RecordSchema, UintTy};
use crate::intern::ListIndex;
use blaze_hash::HashMap;
use blaze_own::Box;
use core::cell::RefCell;

/// A type of a portable table, by its node's index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PTy(pub u32);

/// One node of a portable type. Its children are `PTy`s of the same table with smaller indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PNode {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    /// Elements in the list column; `()` is the empty tuple.
    Tuple(List),
    /// Structural record: canonical schema hash and field types in sorted name order.
    Record(RecordSchema, List),
    /// Structural union members in canonical order.
    Union(List),
    Ref(Mutbl, PTy),
    Ptr(Mutbl, PTy),
    /// `[elem; len]`: `len` a `ConstValue` of `usize`, a `ConstParam`, or (in a body's types
    /// only) an `Infer` hole.
    Array(PTy, PTy),
    Slice(PTy),
    /// A pattern type `base is PAT` (typeck.md, "Pattern types"): the values of `base` in
    /// `ranges`, a list of `ConstValue` terms in pairs, `start ..= end` each, canonical (sorted,
    /// merged, `start > end` a wrapping range). For a raw pointer the values are `usize`s of its
    /// address (`!null` is `1 ..= usize::MAX`). Equal only to a pattern type with the same base
    /// and ranges; never to its base.
    Pat(PTy, List),
    /// An ADT and its generic arguments.
    Adt(ItemKey, List),
    /// A fn item (or a ctor) and its generic arguments.
    FnDef(ItemKey, List),
    /// A fn pointer: the inputs, then the output last; its ABI and safety.
    FnPtr(List, FnPtrHeader),
    /// A closure (the Reference, "Closure types": each closure expression has its own type):
    /// `owner` the item whose body holds the expression, `ordinal` its index among that body's
    /// closure expressions in `ExprId` order, `args` the owner's generic arguments in scope
    /// (`Param(0..n)` in the owner's own record). Its signature and kind are facts, not
    /// identity: the owner's `Checked::closures[ordinal]`, `Question::Closure` from elsewhere.
    Closure { owner: ItemKey, ordinal: u32, args: List },
    /// An async block's future, by its expression: its output and its interior
    /// (`TyView::Future`).
    Future(ExprId, PTy, PTy),
    /// An async block's future of another body (the sibling of `Closure`): `owner`'s body,
    /// its ordinal among that body's `Expr::AsyncBlock`s in expression order (blaze-build
    /// `coroutines_in`), the owner's generic arguments. Output and interior are facts:
    /// `World::coroutine`.
    Coroutine { owner: ItemKey, ordinal: u32, args: List },
    /// A generic parameter of the item, by its index in the item's generics (parents first;
    /// a trait's `Self` is index 0).
    Param(u32),
    /// `<args[0] as trait<args[1..]>>::name`.
    Alias { trait_: ItemKey, name: Name, args: List },
    /// An opaque type (`impl Trait`) and its arguments.
    Opaque(ItemKey, List),
    /// A hole (`_`): only in types written in the body.
    Infer,
    /// The error type: a type error already reported.
    Error,
    /// A value of a const generic (an array length, a const argument): its type, a `Bool`,
    /// `Char`, `Int` or `Uint` node, and its bits, little-endian as consteval writes them; or,
    /// under `adt_const_params`, an `Adt` node of a fieldless enum and its variant's index
    /// (made only by [`PortableTys::variant_value`]: the enum's key types the index).
    ConstValue { ty: PTy, bits: u128 },
    /// A const generic parameter of the item, by its index in the item's generics (as `Param`).
    ConstParam(u32),
    /// A trait object `dyn ..` (typeck.md, "Trait objects"): its predicates, an `ExTrait`
    /// first if it has a principal, then `ExProj`s by (trait, name), then `ExAuto`s by key,
    /// no repeats: the one form [`PortableTys::dynamic`] makes. Regions are erased.
    Dynamic(List),
    /// A trait object's principal: the trait and its arguments after `Self`.
    ExTrait(ItemKey, List),
    /// A trait object's `name = term` of `trait_` (the principal or a supertrait): the
    /// trait's arguments after `Self`.
    ExProj { trait_: ItemKey, name: Name, args: List, term: PTy },
    /// A trait object's auto trait (`+ Send`).
    ExAuto(ItemKey),
    /// An extern type (`extern { type T; }`, feature `extern_types`; core's `Code`, which
    /// `FnPtr::as_ptr` points to), by its item: equal only to itself; no size and no alignment
    /// (neither `Sized` nor `MetaSized`), so a pointer to it is thin; no fields, no drop glue;
    /// no auto trait unless an impl says so (rustc's `ty::Foreign`). A leaf: no children.
    Foreign(ItemKey),
}

/// A borrowed portable table: a base (the shared table as it stood, read in place) and the
/// nodes and list entries after it (a body's own, numbered on from the base's lengths; empty
/// for a table read whole). One numbering over both: `PTy(i)` below the base's length is the
/// base's node `i`, from it up the own node `i - base`; a list lies wholly in one part. The
/// parts are private, so no reader can take the base for the whole table.
///
/// LIVE (lane VH, 2026-11-17): a view of a table over [`Stable`] columns
/// ([`PortableTys::new_stable`]) also holds the column itself, so a type interned through
/// `&self` AFTER the view was taken is readable through it: a lookup indexes the snapshot as
/// before and reads the column only past the snapshot's end (one never-taken branch for a body
/// that imports nothing). The lengths (`len`, `lists_len`) and the walks (`nodes`, `lists`) are
/// the snapshot's: a later entry is found by its id, never by counting.
#[derive(Clone, Copy, Debug)]
pub struct PTys<'a> {
    nodes: Col<'a, PNode>,
    lists: Col<'a, PTy>,
    record_schemas: Col<'a, (RecordSchema, List)>,
    record_names: Col<'a, Name>,
}

/// One column of a [`PTys`]: the base's part and the own part as they stood (one numbering),
/// and, for a stable table, the column they are a prefix of.
#[derive(Clone, Copy, Debug)]
struct Col<'a, T> {
    base: &'a [T],
    own: &'a [T],
    live: Option<Live<'a, T>>,
}

/// A [`Stable`] column a view reads past its snapshot: its element `i` is the view's entry
/// `from + i` (`from`: the start of the part the column holds, 0 for a table of its own).
/// Entries never move once written ([`Stable`]'s promise), so what it reads stays read.
#[derive(Clone, Copy)]
struct Live<'a, T> {
    col: &'a dyn Stable<T>,
    from: usize,
}

impl<T> core::fmt::Debug for Live<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Live").field("from", &self.from).field("len", &self.col.as_slice().len()).finish()
    }
}

impl<'a, T> Col<'a, T> {
    const EMPTY: Col<'a, T> = Col { base: &[], own: &[], live: None };

    /// The snapshot's length.
    #[inline]
    fn len(&self) -> usize {
        self.base.len() + self.own.len()
    }

    /// The length with the live column as it stands (the snapshot's without one).
    fn len_now(&self) -> usize {
        match &self.live {
            Some(l) => self.len().max(l.from + l.col.as_slice().len()),
            None => self.len(),
        }
    }

    /// THE ONE READ of a column: the entries in `r`, from the base's part, the own part, or,
    /// past the snapshot, the live column; `None` past all of them or across the base's end (a
    /// list never straddles it, and one appended after the snapshot lies wholly past it).
    #[inline]
    fn at(&self, r: core::ops::Range<usize>) -> Option<&'a [T]> {
        let b = self.base.len();
        if r.end <= b {
            return self.base.get(r);
        }
        let own = r.start.checked_sub(b)?..r.end - b;
        if own.end <= self.own.len() {
            return self.own.get(own);
        }
        let live = self.live.as_ref()?;
        let col: &'a dyn Stable<T> = live.col;
        col.as_slice().get(r.start.checked_sub(live.from)?..r.end - live.from)
    }
}

impl<'a> PTys<'a> {
    /// Appends every child in declaration order. A projection's arguments precede its term.
    pub fn children_of(self, node: PNode, out: &mut impl Extend<PTy>) -> Result<(), CheckError> {
        match node {
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) | PNode::Adt(_, l) | PNode::FnDef(_, l) | PNode::FnPtr(l, _)
            | PNode::Opaque(_, l) | PNode::Closure { args: l, .. } | PNode::Coroutine { args: l, .. }
            | PNode::Alias { args: l, .. } | PNode::Dynamic(l) | PNode::ExTrait(_, l) => {
                out.extend(self.list(l)?.iter().copied());
            }
            PNode::Ref(_, t) | PNode::Ptr(_, t) | PNode::Slice(t) | PNode::ConstValue { ty: t, .. } => out.extend([t]),
            PNode::Future(_, a, b) | PNode::Array(a, b) => out.extend([a, b]),
            PNode::Pat(base, ranges) => {
                out.extend([base]);
                out.extend(self.list(ranges)?.iter().copied());
            }
            PNode::ExProj { args, term, .. } => {
                out.extend(self.list(args)?.iter().copied());
                out.extend([term]);
            }
            PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str
            | PNode::Never | PNode::Param(_) | PNode::ConstParam(_) | PNode::Infer | PNode::Error | PNode::ExAuto(_)
            | PNode::Foreign(_) => {}
        }
        Ok(())
    }

    /// The empty table.
    pub const EMPTY: PTys<'static> = PTys {
        nodes: Col::EMPTY,
        lists: Col::EMPTY,
        record_schemas: Col::EMPTY,
        record_names: Col::EMPTY,
    };

    /// A table held in two columns, nodes and list entries.
    pub const fn flat(nodes: &'a [PNode], lists: &'a [PTy]) -> PTys<'a> {
        PTys {
            nodes: Col { base: nodes, own: &[], live: None },
            lists: Col { base: lists, own: &[], live: None },
            record_schemas: Col::EMPTY,
            record_names: Col::EMPTY,
        }
    }

    /// A table held in all four columns, including structural record schemas and field names.
    pub(crate) const fn flat_with_records(
        nodes: &'a [PNode],
        lists: &'a [PTy],
        record_schemas: &'a [(RecordSchema, List)],
        record_names: &'a [Name],
    ) -> PTys<'a> {
        PTys {
            nodes: Col { base: nodes, own: &[], live: None },
            lists: Col { base: lists, own: &[], live: None },
            record_schemas: Col { base: record_schemas, own: &[], live: None },
            record_names: Col { base: record_names, own: &[], live: None },
        }
    }

    /// The base `base` with `own_nodes` and `own_lists` numbered on after it. The base's
    /// snapshot only: a base under an overlay does not grow while the overlay lives.
    pub(crate) const fn overlay(
        base: PTys<'a>,
        own_nodes: &'a [PNode],
        own_lists: &'a [PTy],
        own_record_schemas: &'a [(RecordSchema, List)],
        own_record_names: &'a [Name],
    ) -> PTys<'a> {
        PTys {
            nodes: Col { base: base.nodes.base, own: own_nodes, live: None },
            lists: Col { base: base.lists.base, own: own_lists, live: None },
            record_schemas: Col { base: base.record_schemas.base, own: own_record_schemas, live: None },
            record_names: Col { base: base.record_names.base, own: own_record_names, live: None },
        }
    }

    /// This view reading on past its snapshot in `nodes`, `lists`, `record_schemas` and
    /// `record_names`, the columns its last part is a prefix of: the own part of an overlay
    /// (`over`), else the whole table.
    fn with_live(
        mut self,
        over: bool,
        nodes: &'a dyn Stable<PNode>,
        lists: &'a dyn Stable<PTy>,
        record_schemas: &'a dyn Stable<(RecordSchema, List)>,
        record_names: &'a dyn Stable<Name>,
    ) -> PTys<'a> {
        let (nf, lf, rsf, rnf) = if over {
            (self.nodes.base.len(), self.lists.base.len(), self.record_schemas.base.len(), self.record_names.base.len())
        } else {
            (0, 0, 0, 0)
        };
        self.nodes.live = Some(Live { col: nodes, from: nf });
        self.lists.live = Some(Live { col: lists, from: lf });
        self.record_schemas.live = Some(Live { col: record_schemas, from: rsf });
        self.record_names.live = Some(Live { col: record_names, from: rnf });
        self
    }

    /// The node of `t`, borrowed; `None` past the table.
    #[inline]
    pub fn get(&self, t: PTy) -> Option<&'a PNode> {
        let i = t.0 as usize;
        self.nodes.at(i..i + 1)?.first()
    }

    /// The number of nodes in the snapshot (a live view may read later ones by id).
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the table has no node.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of nodes in the base (the first own node's index).
    pub fn base_len(&self) -> usize {
        self.nodes.base.len()
    }

    /// Every node of the snapshot, in index order (`PTy(0)` first).
    pub fn nodes(&self) -> impl Iterator<Item = &'a PNode> + Clone + 'a {
        self.nodes.base.iter().chain(self.nodes.own.iter())
    }

    /// Every list entry of the snapshot, in the list column's order.
    pub fn lists(&self) -> impl Iterator<Item = &'a PTy> + Clone + 'a {
        self.lists.base.iter().chain(self.lists.own.iter())
    }

    /// The number of list entries in the snapshot (the list column's length).
    pub fn lists_len(&self) -> usize {
        self.lists.len()
    }

    /// The number of nodes now: the snapshot's, or a live view's column's as it stands (for a
    /// bound over what an id may reach; never a count to walk, which is the snapshot's).
    pub fn len_now(&self) -> usize {
        self.nodes.len_now()
    }

    /// [`PTys::len_now`] for the list entries.
    pub fn lists_len_now(&self) -> usize {
        self.lists.len_now()
    }

    /// The list entry at `i` of the list column; `None` past it.
    #[inline]
    pub fn list_slot(&self, i: usize) -> Option<PTy> {
        self.lists.at(i..i + 1)?.first().copied()
    }

    /// The list entries in `r`; `None` when `r` is not within one part of the column (a list
    /// never straddles the base's end).
    pub fn list_at(&self, r: core::ops::Range<usize>) -> Option<&'a [PTy]> {
        self.lists.at(r)
    }

    /// The table's identity while it is borrowed: two views of one table are one identity. For
    /// a memo only; never part of a key, a record or an output. Two overlays of one base
    /// differ in their own parts' addresses (an empty own part has nothing to tell apart). A
    /// live view's later entries are of the same table (they never change an earlier entry),
    /// so a memo by identity stays true while it grows.
    pub fn identity(&self) -> (usize, usize) {
        (self.nodes.base.as_ptr() as usize, self.nodes.own.as_ptr() as usize)
    }

    /// The node of `t`.
    pub fn node(&self, t: PTy) -> Result<PNode, CheckError> {
        match self.get(t) {
            Some(n) => Ok(*n),
            None => Err(CheckError::Malformed(Malformed::TypeOrder { node: t.0, child: t.0 })),
        }
    }

    /// The value of the const term `t` when it is a `ConstValue` that fits in a `u64` (an array
    /// length once its item is instantiated); `None` for a parameter, a hole or anything else.
    pub fn value_u64(&self, t: PTy) -> Option<u64> {
        match self.get(t)? {
            PNode::ConstValue { bits, .. } => u64::try_from(*bits).ok(),
            _ => None,
        }
    }

    /// The children of a list.
    pub fn list(&self, l: List) -> Result<&'a [PTy], CheckError> {
        match self.list_at(l.range()) {
            Some(s) => Ok(s),
            None => Err(CheckError::Malformed(Malformed::List {
                column: "types.lists",
                start: l.start,
                len: l.len,
            })),
        }
    }

    fn find_record_schema_list(&self, schema: RecordSchema) -> Option<List> {
        if let Some(&(_, list)) = self.record_schemas.own.iter().find(|&&(s, _)| s == schema) {
            return Some(list);
        }
        if let Some(live) = self.record_schemas.live
            && let Some(&(_, list)) = live.col.as_slice().iter().find(|&&(s, _)| s == schema)
        {
            return Some(list);
        }
        if let Some(&(_, list)) = self.record_schemas.base.iter().find(|&&(s, _)| s == schema) {
            return Some(list);
        }
        None
    }

    /// Canonical sorted field names registered for `schema`, if present in this table.
    pub fn record_names(&self, schema: RecordSchema) -> Option<&'a [Name]> {
        let list = self.find_record_schema_list(schema)?;
        if list.len == 0 {
            return Some(&[]);
        }
        self.record_names.at(list.range())
    }

    /// Canonical field index of `name` in `schema`, if registered.
    pub fn record_field_index(&self, schema: RecordSchema, name: Name) -> Option<u32> {
        let names = self.record_names(schema)?;
        names.iter().position(|&n| n == name).and_then(|i| u32::try_from(i).ok())
    }

    /// Every registered `(RecordSchema, &[Name])` in this view (base then own).
    pub fn record_schemas(&self) -> impl Iterator<Item = (RecordSchema, &'a [Name])> + 'a {
        let this = *self;
        self.record_schemas.base.iter().chain(self.record_schemas.own.iter()).filter_map(move |&(s, l)| {
            let names = if l.len == 0 { &[][..] } else { this.record_names.at(l.range())? };
            Some((s, names))
        })
    }

    /// Number of record schema rows in the snapshot.
    pub fn record_schemas_len(&self) -> usize {
        self.record_schemas.len()
    }

    /// Number of record field name entries in the snapshot.
    pub fn record_names_len(&self) -> usize {
        self.record_names.len()
    }

    /// Every `(RecordSchema, List)` row in the snapshot (base then own).
    pub fn record_schema_rows(&self) -> impl Iterator<Item = &'a (RecordSchema, List)> + Clone + 'a {
        self.record_schemas.base.iter().chain(self.record_schemas.own.iter())
    }

    /// Every record field name entry in the snapshot (base then own).
    pub fn record_names_iter(&self) -> impl Iterator<Item = &'a Name> + Clone + 'a {
        self.record_names.base.iter().chain(self.record_names.own.iter())
    }
}

/// Hashed as one table (base then own), so an overlay hashes as the copy it stands for.
impl ContentHash for PTys<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(self.len() as u64);
        for n in self.nodes() {
            hash_node(n, h);
        }
        h.u64(self.lists_len() as u64);
        for t in self.lists() {
            h.u32(t.0);
        }
    }
}

impl ContentHash for PTy {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u32(self.0);
    }
}

fn hash_list(l: List, h: &mut StableHasher) {
    h.u32(l.start);
    h.u32(l.len);
}

/// Pushes the children of `n` in `t` onto `out` in order (a list's terms in order; a
/// `ConstValue`'s type too; a `Future` skipped, as record conversion refuses it): the one walk
/// order of a record's terms, for converting them (infer/facts.rs) and for hashing them
/// (world.rs `Hashes`).
pub(crate) fn node_children(t: PTys<'_>, n: PNode, out: &mut blaze_own::Vec<u32, blaze_own::Alloc<'_>>) -> Result<(), CheckError> {
    if matches!(n, PNode::Future(..)) {
        return Ok(());
    }
    struct Ids<'a, 'b>(&'a mut Vec<u32, Alloc<'b>>);
    impl Extend<PTy> for Ids<'_, '_> {
        fn extend<I: IntoIterator<Item = PTy>>(&mut self, iter: I) {
            self.0.extend(iter.into_iter().map(|p| p.0));
        }
    }
    t.children_of(n, &mut Ids(out))
}

/// The content of one node, its children by index.
pub(crate) fn hash_node(n: &PNode, h: &mut StableHasher) {
    match *n {
        PNode::Bool => h.tag(0),
        PNode::Char => h.tag(1),
        PNode::Int(i) => {
            h.tag(2);
            h.tag(i as u8);
        }
        PNode::Uint(u) => {
            h.tag(3);
            h.tag(u as u8);
        }
        PNode::Float(f) => {
            h.tag(4);
            h.tag(f as u8);
        }
        PNode::Str => h.tag(5),
        PNode::Never => h.tag(6),
        PNode::Tuple(l) => {
            h.tag(7);
            hash_list(l, h);
        }
        PNode::Ref(m, t) => {
            h.tag(8);
            h.tag(m as u8);
            h.u32(t.0);
        }
        PNode::Ptr(m, t) => {
            h.tag(9);
            h.tag(m as u8);
            h.u32(t.0);
        }
        PNode::Array(t, n) => {
            h.tag(10);
            h.u32(t.0);
            h.u32(n.0);
        }
        PNode::Slice(t) => {
            h.tag(11);
            h.u32(t.0);
        }
        PNode::Pat(t, l) => {
            h.tag(24);
            h.u32(t.0);
            hash_list(l, h);
        }
        PNode::Adt(k, l) => {
            h.tag(12);
            h.u128(k.0);
            hash_list(l, h);
        }
        PNode::FnDef(k, l) => {
            h.tag(13);
            h.u128(k.0);
            hash_list(l, h);
        }
        PNode::FnPtr(l, hdr) => {
            h.tag(14);
            hash_list(l, h);
            h.u32(u32::from(hdr.bits()));
        }
        PNode::Closure { owner, ordinal, args } => {
            h.tag(15);
            h.u128(owner.0);
            h.u32(ordinal);
            hash_list(args, h);
        }
        PNode::Param(i) => {
            h.tag(16);
            h.u32(i);
        }
        PNode::Alias { trait_, name, args } => {
            h.tag(17);
            h.u128(trait_.0);
            h.u64(name.0);
            hash_list(args, h);
        }
        PNode::Opaque(k, l) => {
            h.tag(18);
            h.u128(k.0);
            hash_list(l, h);
        }
        PNode::Infer => h.tag(19),
        PNode::Error => h.tag(20),
        PNode::Coroutine { owner, ordinal, args } => {
            h.tag(29);
            h.u128(owner.0);
            h.u32(ordinal);
            hash_list(args, h);
        }
        PNode::Future(e, output, interior) => {
            h.tag(21);
            h.u32(e.0);
            h.u32(output.0);
            h.u32(interior.0);
        }
        PNode::ConstValue { ty, bits } => {
            h.tag(22);
            h.u32(ty.0);
            h.u128(bits);
        }
        PNode::ConstParam(i) => {
            h.tag(23);
            h.u32(i);
        }
        PNode::Dynamic(l) => {
            h.tag(25);
            hash_list(l, h);
        }
        PNode::ExTrait(k, l) => {
            h.tag(26);
            h.u128(k.0);
            hash_list(l, h);
        }
        PNode::ExProj { trait_, name, args, term } => {
            h.tag(27);
            h.u128(trait_.0);
            h.u64(name.0);
            hash_list(args, h);
            h.u32(term.0);
        }
        PNode::ExAuto(k) => {
            h.tag(28);
            h.u128(k.0);
        }
        PNode::Foreign(k) => {
            h.tag(30);
            h.u128(k.0);
        }
        PNode::Record(s, l) => {
            h.tag(31);
            h.u64(s.0);
            hash_list(l, h);
        }
        PNode::Union(l) => {
            h.tag(32);
            hash_list(l, h);
        }
    }
}

/// A node with its handles cleared: every handle to a term becomes `PTy(0)` and every list starts
/// at 0 (its length kept), so the shape carries what the node is and not where its children sit
/// in its table.
pub fn node_shape(n: PNode) -> PNode {
    let p = PTy(0);
    let l = |l: List| List { start: 0, len: l.len };
    match n {
        PNode::Tuple(x) => PNode::Tuple(l(x)),
        PNode::Record(s, x) => PNode::Record(s, l(x)),
        PNode::Union(x) => PNode::Union(l(x)),
        PNode::Ref(m, _) => PNode::Ref(m, p),
        PNode::Ptr(m, _) => PNode::Ptr(m, p),
        PNode::Array(..) => PNode::Array(p, p),
        PNode::Slice(_) => PNode::Slice(p),
        PNode::Pat(_, x) => PNode::Pat(p, l(x)),
        PNode::Adt(k, x) => PNode::Adt(k, l(x)),
        PNode::FnDef(k, x) => PNode::FnDef(k, l(x)),
        PNode::FnPtr(x, hdr) => PNode::FnPtr(l(x), hdr),
        PNode::Closure { owner, ordinal, args } => PNode::Closure { owner, ordinal, args: l(args) },
        PNode::Future(e, ..) => PNode::Future(e, p, p),
        PNode::Coroutine { owner, ordinal, args } => PNode::Coroutine { owner, ordinal, args: l(args) },
        PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: l(args) },
        PNode::Opaque(k, x) => PNode::Opaque(k, l(x)),
        PNode::ConstValue { bits, .. } => PNode::ConstValue { ty: p, bits },
        PNode::Dynamic(x) => PNode::Dynamic(l(x)),
        PNode::ExTrait(k, x) => PNode::ExTrait(k, l(x)),
        PNode::ExProj { trait_, name, args, .. } => PNode::ExProj { trait_, name, args: l(args), term: p },
        leaf @ (PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Param(_)
        | PNode::Infer
        | PNode::Error
        | PNode::ConstParam(_)
        | PNode::ExAuto(_)
        | PNode::Foreign(_)) => leaf,
    }
}

/// The content of one node WITHOUT where its children sit in its table: every handle to a
/// term is `PTy(0)` and every list starts at 0 (its length kept), so two tables that interned
/// the same terms in different orders give one hash. The children's own digests are hashed
/// after it, in [`node_children`]'s order (world.rs `Hashes::term`). A read-list digest takes
/// this form, never [`hash_node`]'s: a store's table numbers terms in the order its run met
/// them, which is not content (the `front2_records` test's body 1, 2026-11-16: `Box::leak`'s
/// signature equal over records and over solved names, its input at `PTy(478)` in one store
/// and `PTy(486)` in the other).
pub(crate) fn hash_node_shape(n: &PNode, h: &mut StableHasher) {
    hash_node(&node_shape(*n), h);
}

/// A trait object's impl-lookup shape key: its principal trait, from its rows `l` of `t`
/// (`None`: auto traits only, or a list past the table).
pub fn object_principal(t: PTys<'_>, l: List) -> Option<ItemKey> {
    match t.get(*t.list(l).ok()?.first()?)? {
        PNode::ExTrait(k, _) => Some(*k),
        _ => None,
    }
}

/// The most predicates one trait object holds (a principal, its bindings, its auto traits).
pub const MAX_OBJECT_PREDS: usize = 32;

/// The order of a trait object's predicates (`PNode::Dynamic`): the principal, then bindings
/// by (trait, name), then auto traits by key. `None`: not a predicate node.
pub fn object_pred_key(n: PNode) -> Option<(u8, u128, u64)> {
    match n {
        PNode::ExTrait(..) => Some((0, 0, 0)),
        PNode::ExProj { trait_, name, .. } => Some((1, trait_.0, name.0)),
        PNode::ExAuto(k) => Some((2, k.0, 0)),
        _ => None,
    }
}

/// The most ranges one pattern type holds; core writes at most two (`..0 | 1..`).
pub const MAX_PATTERN_RANGES: usize = 16;

/// The canonical form of a pattern's ranges over `width` bits (`PortableTys::pattern`): each
/// `start ..= end`, `start > end` wrapping past the maximum; returned sorted by start, merged
/// where they overlap or touch, a run ending at the maximum and one starting at zero merged
/// into one wrapping range (put first), with the count of runs. One set of values has one
/// form. `width` is 1 to 128; more than `MAX_PATTERN_RANGES` ranges is `Limit::PatternRanges`.
pub fn canonical_ranges(
    width: u32,
    ranges: &[(u128, u128)],
) -> Result<([(u128, u128); MAX_PATTERN_RANGES], usize), CheckError> {
    let limit = || CheckError::Limit(crate::error::Limit::PatternRanges);
    if width == 0 || width > 128 || ranges.len() > MAX_PATTERN_RANGES {
        return Err(limit());
    }
    let max = if width == 128 { u128::MAX } else { (1u128 << width) - 1 };
    // A wrapping range splits at the maximum, so every run here is `start <= end`.
    let mut split = [(0u128, 0u128); 2 * MAX_PATTERN_RANGES];
    let mut n = 0;
    for &(start, end) in ranges {
        let (start, end) = (start & max, end & max);
        if start <= end {
            split[n] = (start, end);
            n += 1;
        } else {
            split[n] = (start, max);
            split[n + 1] = (0, end);
            n += 2;
        }
    }
    let split = &mut split[..n];
    split.sort_unstable();
    let mut merged = [(0u128, 0u128); 2 * MAX_PATTERN_RANGES];
    let mut m = 0;
    for &(start, end) in split.iter() {
        // Sorted by start, so a run touches the last one exactly when it starts at most one
        // past its end (and the last one reaching the maximum covers everything after it).
        if m > 0 && (merged[m - 1].1 == max || start <= merged[m - 1].1 + 1) {
            if end > merged[m - 1].1 {
                merged[m - 1].1 = end;
            }
        } else {
            merged[m] = (start, end);
            m += 1;
        }
    }
    if m >= 2 && merged[0].0 == 0 && merged[m - 1].1 == max {
        merged[0] = (merged[m - 1].0, merged[0].1);
        m -= 1;
    }
    if m > MAX_PATTERN_RANGES {
        return Err(limit());
    }
    let mut out = [(0u128, 0u128); MAX_PATTERN_RANGES];
    out[..m].copy_from_slice(&merged[..m]);
    Ok((out, m))
}

/// An owned portable table, hash-consed: equal structure is an equal `PTy`, and an equal list
/// is an equal `List`, so the table's bytes depend on structure and the order of first use
/// only (rule 25).
///
/// A body's table is an OVERLAY ([`PortableTys::over`]): the shared table, borrowed and read in
/// place, then the body's own nodes and lists numbered on after it. Making one copies nothing;
/// interning looks in the shared table's index first, then the overlay's own. The borrow keeps
/// the shared table unchanged while the overlay lives, and what the body keeps after it is only
/// its own part ([`PortableTys::keep_in`], [`OwnTys`]), so a body-only type never enters a
/// table a record is answered from (`Malformed::BodyTypeInRecord`), and a body's ids and bytes
/// depend only on the shared table as it stood when the body began.
///
/// TWO FORMS of the own part (lane VE, 2026-11-17), chosen by the maker:
/// - [`PortableTys::new_in`] / [`PortableTys::over`]: owned columns, interned through `&mut`;
/// - [`PortableTys::new_stable`] / [`PortableTys::over_stable`]: [`Stable`] columns the caller
///   hands in (rule 75: the memory they take is the caller's to name), interned through `&self`
///   as well ([`PortableTys::intern_shared`]): a body's world interns a head it imports on the
///   spot while earlier answers still hold `PTys` views, each a shorter prefix of columns that
///   never move. No `unsafe` here: the `Stable` impl promises its own addresses.
pub struct PortableTys<'a> {
    /// The shared table this one extends (an overlay); `None`: a table of its own.
    base: Option<&'a PortableTys<'a>>,
    /// The allocator of the index and the owned columns.
    alloc: Alloc<'a>,
    own: Own<'a>,
}

/// Append-only storage whose elements never move once written: a reference from
/// [`Stable::as_slice`] stays valid, reading what it read, while later `extend`s run through
/// `&self`. Implemented by the caller (blaze-ladder front2 rows.rs `StableColumn`, in the
/// allocator it is handed).
pub trait Stable<T> {
    /// Appends `values` and returns the index of the first; [`Full`] at the storage's cap (or
    /// when its memory cannot be had), appending nothing.
    fn extend(&self, values: &[T]) -> Result<u32, Full>;
    /// Every element appended so far.
    fn as_slice(&self) -> &[T];
}

/// A [`Stable`] column at its cap: interned as `Limit::Table`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

/// A table's own nodes and list entries in one of its two forms.
enum Own<'a> {
    Owned {
        nodes: Vec<PNode, Alloc<'a>>,
        lists: Vec<PTy, Alloc<'a>>,
        record_schemas: Vec<(RecordSchema, List), Alloc<'a>>,
        record_names: Vec<Name, Alloc<'a>>,
        index: OwnIndex<'a>,
    },
    /// Behind a box of a trait object: the index's cell is invariant in its lifetime, and the
    /// object keeps the table covariant (the check below).
    Stable(Box<dyn StableOwn + 'a, Alloc<'a>>),
}

/// What finds an own node or list by content.
struct OwnIndex<'a> {
    /// Own nodes by content, to their own index (`i`, not the `PTy`).
    node_index: HashMap<PNode, u32, Alloc<'a>>,
    list_index: ListIndex<'a>,
    /// Per own interned list, in the order interned: where it is in the whole list column.
    list_rows: Vec<List, Alloc<'a>>,
}

impl<'a> OwnIndex<'a> {
    fn new_in(alloc: Alloc<'a>) -> OwnIndex<'a> {
        OwnIndex { node_index: HashMap::new_in(alloc), list_index: ListIndex::new_in(alloc), list_rows: Vec::new_in(alloc) }
    }

    /// The own list equal to `tys` (hash `hash`), its entries `offset` on in the whole column.
    fn find_list(&self, lists: &[PTy], offset: usize, hash: u64, tys: &[PTy]) -> Option<List> {
        let rows = &self.list_rows;
        let own_range = |l: List| l.start as usize - offset..l.start as usize - offset + l.len as usize;
        let found = self.list_index.find(hash, |id| lists.get(own_range(rows[id as usize])) == Some(tys))?;
        Some(rows[found as usize])
    }
}

/// The `&self` form's own part: its columns and its index in a cell (borrowed only inside one
/// intern, never across a return).
struct StableCols<'a> {
    nodes: Box<dyn Stable<PNode> + 'a, Alloc<'a>>,
    lists: Box<dyn Stable<PTy> + 'a, Alloc<'a>>,
    record_schemas: Box<dyn Stable<(RecordSchema, List)> + 'a, Alloc<'a>>,
    record_names: Box<dyn Stable<Name> + 'a, Alloc<'a>>,
    index: RefCell<OwnIndex<'a>>,
}

/// [`StableCols`] with its lifetime behind the object.
trait StableOwn {
    fn nodes(&self) -> &[PNode];
    fn lists(&self) -> &[PTy];
    fn record_schemas(&self) -> &[(RecordSchema, List)];
    fn record_names(&self) -> &[Name];
    fn find_node(&self, n: &PNode) -> Option<u32>;
    fn find_list(&self, offset: usize, hash: u64, tys: &[PTy]) -> Option<List>;
    fn intern(&self, base: Option<&PortableTys<'_>>, n: PNode) -> Result<PTy, CheckError>;
    fn intern_list(&self, base: Option<&PortableTys<'_>>, hash: u64, tys: &[PTy]) -> Result<List, CheckError>;
    fn register_record_schema(&self, base: Option<&PortableTys<'_>>, schema: RecordSchema, sorted_names: &[Name]) -> Result<(), Full>;
    fn list_rows(&self, each: &mut dyn FnMut(List) -> Result<(), CheckError>) -> Result<(), CheckError>;
    /// The columns themselves, for a live view ([`PTys`]).
    fn node_col(&self) -> &dyn Stable<PNode>;
    fn list_col(&self) -> &dyn Stable<PTy>;
    fn record_schema_col(&self) -> &dyn Stable<(RecordSchema, List)>;
    fn record_name_col(&self) -> &dyn Stable<Name>;
}

fn table_full(_: Full) -> CheckError {
    CheckError::Limit(crate::error::Limit::Table)
}

impl StableOwn for StableCols<'_> {
    fn nodes(&self) -> &[PNode] {
        self.nodes.as_slice()
    }

    fn lists(&self) -> &[PTy] {
        self.lists.as_slice()
    }

    fn record_schemas(&self) -> &[(RecordSchema, List)] {
        self.record_schemas.as_slice()
    }

    fn record_names(&self) -> &[Name] {
        self.record_names.as_slice()
    }

    fn find_node(&self, n: &PNode) -> Option<u32> {
        self.index.borrow().node_index.get(n).copied()
    }

    fn find_list(&self, offset: usize, hash: u64, tys: &[PTy]) -> Option<List> {
        self.index.borrow().find_list(self.lists.as_slice(), offset, hash, tys)
    }

    fn intern(&self, base: Option<&PortableTys<'_>>, n: PNode) -> Result<PTy, CheckError> {
        let mut index = self.index.borrow_mut();
        intern_node(base, &mut index, self.nodes.as_slice().len(), n, |n| self.nodes.extend(&[n]).map(drop).map_err(table_full))
    }

    fn intern_list(&self, base: Option<&PortableTys<'_>>, hash: u64, tys: &[PTy]) -> Result<List, CheckError> {
        let mut index = self.index.borrow_mut();
        match place_list(base, &index, self.lists.as_slice(), hash, tys)? {
            Ok(found) => Ok(found),
            Err(new) => {
                self.lists.extend(tys).map_err(table_full)?;
                file_list(&mut index, hash, new)
            }
        }
    }

    fn register_record_schema(&self, base: Option<&PortableTys<'_>>, schema: RecordSchema, sorted_names: &[Name]) -> Result<(), Full> {
        let _guard = self.index.borrow_mut();
        if let Some(b) = base
            && b.has_record_schema(schema)
        {
            return Ok(());
        }
        if self.record_schemas.as_slice().iter().any(|&(s, _)| s == schema) {
            return Ok(());
        }
        let base_n = base.map_or(0, |b| b.own.record_names().len());
        let len = u32::try_from(sorted_names.len()).map_err(|_| Full)?;
        let list = if sorted_names.is_empty() {
            List::EMPTY
        } else {
            let start = u32::try_from(base_n + self.record_names.as_slice().len()).map_err(|_| Full)?;
            self.record_names.extend(sorted_names)?;
            List { start, len }
        };
        self.record_schemas.extend(&[(schema, list)])?;
        Ok(())
    }

    fn list_rows(&self, each: &mut dyn FnMut(List) -> Result<(), CheckError>) -> Result<(), CheckError> {
        self.index.borrow().list_rows.iter().try_for_each(|&l| each(l))
    }

    fn node_col(&self) -> &dyn Stable<PNode> {
        &*self.nodes
    }

    fn list_col(&self) -> &dyn Stable<PTy> {
        &*self.lists
    }

    fn record_schema_col(&self) -> &dyn Stable<(RecordSchema, List)> {
        &*self.record_schemas
    }

    fn record_name_col(&self) -> &dyn Stable<Name> {
        &*self.record_names
    }
}

/// `n` interned in an own part (`index`, its `own_len` nodes, `push` appending one) over
/// `base`: found in the base, else in the own part, else appended.
fn intern_node(base: Option<&PortableTys<'_>>, index: &mut OwnIndex<'_>, own_len: usize, n: PNode, push: impl FnOnce(PNode) -> Result<(), CheckError>) -> Result<PTy, CheckError> {
    if let Some(b) = base
        && let Some(found) = b.find_node(&n)
    {
        return Ok(PTy(found));
    }
    let base_n = base.map_or(0, PortableTys::len);
    if let Some(&found) = index.node_index.get(&n) {
        return Ok(PTy(found + base_n as u32));
    }
    let limit = |_| CheckError::Limit(crate::error::Limit::Table);
    let own = u32::try_from(own_len).map_err(limit)?;
    let at = u32::try_from(base_n + own_len).map_err(limit)?;
    push(n)?;
    index.node_index.insert(n, own);
    Ok(PTy(at))
}

/// Where the list `tys` (hash `hash`, not empty) is in an own part (`index`, its `lists`)
/// over `base`: `Ok` found, `Err` the list it is once its entries are appended
/// ([`file_list`]), as [`intern_node`] does for a node.
fn place_list(base: Option<&PortableTys<'_>>, index: &OwnIndex<'_>, lists: &[PTy], hash: u64, tys: &[PTy]) -> Result<Result<List, List>, CheckError> {
    if let Some(b) = base
        && let Some(found) = b.find_list(hash, tys)
    {
        return Ok(Ok(found));
    }
    let base_l = base.map_or(0, |b| b.own.lists().len());
    if let Some(found) = index.find_list(lists, base_l, hash, tys) {
        return Ok(Ok(found));
    }
    let limit = |_| CheckError::Limit(crate::error::Limit::Table);
    let start = u32::try_from(base_l + lists.len()).map_err(limit)?;
    let len = u32::try_from(tys.len()).map_err(limit)?;
    Ok(Err(List { start, len }))
}

/// The index hash of the list `tys`.
fn list_hash(tys: &[PTy]) -> u64 {
    let mut h = StableHasher::new("ptys.list");
    for t in tys {
        h.u32(t.0);
    }
    h.finish()
}

/// Files the new list `list` (hash `hash`) in `index` once its entries are appended.
fn file_list(index: &mut OwnIndex<'_>, hash: u64, list: List) -> Result<List, CheckError> {
    index.list_index.push(hash)?;
    index.list_rows.push(list);
    Ok(list)
}

impl<'a> Own<'a> {
    fn nodes(&self) -> &[PNode] {
        match self {
            Own::Owned { nodes, .. } => nodes,
            Own::Stable(s) => s.nodes(),
        }
    }

    fn lists(&self) -> &[PTy] {
        match self {
            Own::Owned { lists, .. } => lists,
            Own::Stable(s) => s.lists(),
        }
    }

    fn record_schemas(&self) -> &[(RecordSchema, List)] {
        match self {
            Own::Owned { record_schemas, .. } => record_schemas,
            Own::Stable(s) => s.record_schemas(),
        }
    }

    fn record_names(&self) -> &[Name] {
        match self {
            Own::Owned { record_names, .. } => record_names,
            Own::Stable(s) => s.record_names(),
        }
    }
}

/// `&PortableTys<'long>` is a `&PortableTys<'short>`: an overlay borrows a longer-lived shared
/// table at the overlay's own lifetime. Fails to compile if the table stops being covariant.
const _: fn() = || {
    fn covariant<'s>(t: &'s PortableTys<'static>) -> &'s PortableTys<'s> {
        t
    }
    let _ = covariant;
};

impl<'a> PortableTys<'a> {
    /// Instantiates a recorded type in this table. Replacements are in the caller's scope,
    /// so even a replacement that contains a parameter is returned without descending into it.
    pub fn subst(&mut self, root: PTy, args: &[PTy]) -> Result<PTy, CheckError> {
        let alloc = self.alloc();
        let mut mapped = HashMap::new_in(alloc);
        let mut stack = Vec::new_in(alloc);
        let mut children = Vec::new_in(alloc);
        stack.push((root, false));
        while let Some((at, expanded)) = stack.pop() {
            if mapped.contains_key(&at) {
                continue;
            }
            let view = self.view();
            let node = view.node(at)?;
            if let PNode::Param(index) | PNode::ConstParam(index) = node {
                let site = crate::error::ParamSite::PortableSubstitution { root, node: at };
                let &arg = args.get(index as usize).ok_or(CheckError::Malformed(Malformed::Param {
                    index, args: args.len() as u32, site,
                }))?;
                view.node(arg)?;
                mapped.insert(at, arg);
                continue;
            }
            children.clear();
            view.children_of(node, &mut children)?;
            if children.is_empty() {
                mapped.insert(at, at);
                continue;
            }
            if !expanded {
                stack.push((at, true));
                for &child in children.iter().rev() {
                    if child.0 >= at.0 {
                        return Err(CheckError::Malformed(Malformed::TypeOrder { node: at.0, child: child.0 }));
                    }
                    stack.push((child, false));
                }
                continue;
            }
            let mut changed = false;
            for child in children.iter_mut() {
                let replacement = *mapped.get(child).ok_or(CheckError::Malformed(Malformed::TypeOrder { node: at.0, child: child.0 }))?;
                changed |= *child != replacement;
                *child = replacement;
            }
            let value = if changed { self.rebuild(node, &children)? } else { at };
            mapped.insert(at, value);
        }
        mapped.get(&root).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }))
    }

    /// Copies `root` from `src` into this table. When `args` is `Some(repl)`, each `Param(i)`
    /// and `ConstParam(i)` is replaced by `repl[i]` (already in `self`, not descended into);
    /// when `args` is `None`, parameters are copied verbatim.
    pub fn copy_from(&mut self, src: PTys<'_>, root: PTy, args: Option<&[PTy]>) -> Result<PTy, CheckError> {
        let alloc = self.alloc();
        let mut mapped = HashMap::new_in(alloc);
        let mut stack = Vec::new_in(alloc);
        let mut children = Vec::new_in(alloc);
        stack.push((root, false));
        while let Some((at, expanded)) = stack.pop() {
            if mapped.contains_key(&at) {
                continue;
            }
            let node = src.node(at)?;
            if let Some(repl) = args
                && let PNode::Param(index) | PNode::ConstParam(index) = node
            {
                let site = crate::error::ParamSite::PortableSubstitution { root, node: at };
                let count = u32::try_from(repl.len()).unwrap_or(u32::MAX);
                let &arg = usize::try_from(index).ok().and_then(|i| repl.get(i)).ok_or(CheckError::Malformed(Malformed::Param {
                    index,
                    args: count,
                    site,
                }))?;
                self.view().node(arg)?;
                mapped.insert(at, arg);
                continue;
            }
            if let PNode::Record(schema, _) = node
                && let Some(names) = src.record_names(schema)
            {
                self.register_record_schema(schema, names).map_err(table_full)?;
            }
            children.clear();
            src.children_of(node, &mut children)?;
            if children.is_empty() {
                let value = self.rebuild(node, &children)?;
                mapped.insert(at, value);
                continue;
            }
            if !expanded {
                stack.push((at, true));
                for &child in children.iter().rev() {
                    if child.0 >= at.0 {
                        return Err(CheckError::Malformed(Malformed::TypeOrder { node: at.0, child: child.0 }));
                    }
                    stack.push((child, false));
                }
                continue;
            }
            for child in children.iter_mut() {
                let replacement = *mapped.get(child).ok_or(CheckError::Malformed(Malformed::TypeOrder { node: at.0, child: child.0 }))?;
                *child = replacement;
            }
            let value = self.rebuild(node, &children)?;
            mapped.insert(at, value);
        }
        mapped.get(&root).copied().ok_or(CheckError::Malformed(Malformed::TypeOrder { node: root.0, child: root.0 }))
    }

    /// Reassembles a node from the children returned by `PTys::children_of`, in that order.
    fn rebuild(&mut self, node: PNode, children: &[PTy]) -> Result<PTy, CheckError> {
        let node = match node {
            PNode::ConstValue { bits, .. } => PNode::ConstValue { ty: children[0], bits },
            PNode::Tuple(_) => PNode::Tuple(self.intern_list(children)?),
            PNode::Record(s, _) => PNode::Record(s, self.intern_list(children)?),
            PNode::Union(_) => PNode::Union(self.intern_list(children)?),
            PNode::Ref(m, _) => PNode::Ref(m, children[0]),
            PNode::Ptr(m, _) => PNode::Ptr(m, children[0]),
            PNode::Array(..) => PNode::Array(children[0], children[1]),
            PNode::Slice(_) => PNode::Slice(children[0]),
            PNode::Pat(..) => PNode::Pat(children[0], self.intern_list(&children[1..])?),
            PNode::Adt(k, _) => PNode::Adt(k, self.intern_list(children)?),
            PNode::FnDef(k, _) => PNode::FnDef(k, self.intern_list(children)?),
            PNode::FnPtr(_, h) => PNode::FnPtr(self.intern_list(children)?, h),
            PNode::Closure { owner, ordinal, .. } => PNode::Closure { owner, ordinal, args: self.intern_list(children)? },
            PNode::Future(e, ..) => PNode::Future(e, children[0], children[1]),
            PNode::Coroutine { owner, ordinal, .. } => PNode::Coroutine { owner, ordinal, args: self.intern_list(children)? },
            PNode::Alias { trait_, name, .. } => PNode::Alias { trait_, name, args: self.intern_list(children)? },
            PNode::Opaque(k, _) => PNode::Opaque(k, self.intern_list(children)?),
            PNode::Dynamic(_) => PNode::Dynamic(self.intern_list(children)?),
            PNode::ExTrait(k, _) => PNode::ExTrait(k, self.intern_list(children)?),
            PNode::ExProj { trait_, name, args, .. } => {
                let n = args.len as usize;
                PNode::ExProj { trait_, name, args: self.intern_list(&children[..n])?, term: children[n] }
            }
            PNode::Bool | PNode::Char | PNode::Int(_) | PNode::Uint(_) | PNode::Float(_) | PNode::Str
            | PNode::Never | PNode::Param(_) | PNode::ConstParam(_) | PNode::Infer | PNode::Error | PNode::ExAuto(_)
            | PNode::Foreign(_) => node,
        };
        self.intern(node)
    }

    /// The allocator this table's own part lives in: its maker's, handed down (D102), so a
    /// question asked over the table (the run path's `drop_impl`) allocates its scratch there
    /// and fetches no allocator of its own.
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// An empty table, interned through `&mut`.
    pub fn new_in(alloc: Alloc<'a>) -> PortableTys<'a> {
        PortableTys {
            base: None,
            alloc,
            own: Own::Owned {
                nodes: Vec::new_in(alloc),
                lists: Vec::new_in(alloc),
                record_schemas: Vec::new_in(alloc),
                record_names: Vec::new_in(alloc),
                index: OwnIndex::new_in(alloc),
            },
        }
    }

    /// An empty table over the caller's [`Stable`] columns (empty when handed in), interned
    /// through `&self` too ([`PortableTys::intern_shared`]); its index in `alloc`.
    pub fn new_stable(
        alloc: Alloc<'a>,
        nodes: Box<dyn Stable<PNode> + 'a, Alloc<'a>>,
        lists: Box<dyn Stable<PTy> + 'a, Alloc<'a>>,
        record_schemas: Box<dyn Stable<(RecordSchema, List)> + 'a, Alloc<'a>>,
        record_names: Box<dyn Stable<Name> + 'a, Alloc<'a>>,
    ) -> Result<PortableTys<'a>, CheckError> {
        if !nodes.as_slice().is_empty()
            || !lists.as_slice().is_empty()
            || !record_schemas.as_slice().is_empty()
            || !record_names.as_slice().is_empty()
        {
            let n = u32::try_from(nodes.as_slice().len()).unwrap_or(u32::MAX);
            return Err(CheckError::Malformed(Malformed::Overlay { base: 0, have: n }));
        }
        let cols: Box<dyn StableOwn + 'a, Alloc<'a>> = Box::new_in(
            StableCols { nodes, lists, record_schemas, record_names, index: RefCell::new(OwnIndex::new_in(alloc)) },
            alloc,
        );
        Ok(PortableTys { base: None, alloc, own: Own::Stable(cols) })
    }

    /// [`PortableTys::over`] with the own part in [`Stable`] columns ([`PortableTys::new_stable`]).
    pub fn over_stable(
        base: &'a PortableTys<'a>,
        alloc: Alloc<'a>,
        nodes: Box<dyn Stable<PNode> + 'a, Alloc<'a>>,
        lists: Box<dyn Stable<PTy> + 'a, Alloc<'a>>,
        record_schemas: Box<dyn Stable<(RecordSchema, List)> + 'a, Alloc<'a>>,
        record_names: Box<dyn Stable<Name> + 'a, Alloc<'a>>,
    ) -> Result<PortableTys<'a>, CheckError> {
        let mut t = PortableTys::new_stable(alloc, nodes, lists, record_schemas, record_names)?;
        t.set_base(base)?;
        Ok(t)
    }

    fn set_base(&mut self, base: &'a PortableTys<'a>) -> Result<(), CheckError> {
        if base.base.is_some() {
            let n = u32::try_from(base.len()).unwrap_or(u32::MAX);
            return Err(CheckError::Malformed(Malformed::Overlay { base: n, have: n }));
        }
        self.base = Some(base);
        Ok(())
    }

    /// The own node `n`'s own index (a table of its own: its `PTy`), if interned.
    fn find_node(&self, n: &PNode) -> Option<u32> {
        match &self.own {
            Own::Owned { index, .. } => index.node_index.get(n).copied(),
            Own::Stable(s) => s.find_node(n),
        }
    }

    /// Every interned list of the whole table (the base's, then the own), in the order
    /// interned: what [`PortableTys::fork_in`] re-interns, and what the checked-body record
    /// (crate::checked_record) keeps so a decode lays each list at the same index.
    pub(crate) fn list_rows_in<'b>(&self, alloc: Alloc<'b>) -> Result<Vec<List, Alloc<'b>>, CheckError> {
        let mut rows = Vec::new_in(alloc);
        if let Some(b) = self.base {
            b.each_list_row(&mut |l| {
                rows.push(l);
                Ok(())
            })?;
        }
        self.each_list_row(&mut |l| {
            rows.push(l);
            Ok(())
        })?;
        Ok(rows)
    }

    /// Each own interned list, in the order interned.
    fn each_list_row(&self, each: &mut dyn FnMut(List) -> Result<(), CheckError>) -> Result<(), CheckError> {
        match &self.own {
            Own::Owned { index, .. } => index.list_rows.iter().try_for_each(|&l| each(l)),
            Own::Stable(s) => s.list_rows(each),
        }
    }

    /// The own list equal to `tys` (hash `hash`), if interned; for a table of its own (a base).
    fn find_list(&self, hash: u64, tys: &[PTy]) -> Option<List> {
        let offset = self.base_lens().1;
        match &self.own {
            Own::Owned { lists, index, .. } => index.find_list(lists, offset, hash, tys),
            Own::Stable(s) => s.find_list(offset, hash, tys),
        }
    }

    /// A body's table: `base` read in place, its own types in `alloc` after it. O(1): nothing
    /// of `base` is copied or hashed. `base` must be a table of its own (one level of overlay).
    pub fn over(base: &'a PortableTys<'a>, alloc: Alloc<'a>) -> Result<PortableTys<'a>, CheckError> {
        let mut t = PortableTys::new_in(alloc);
        t.set_base(base)?;
        Ok(t)
    }

    /// The base's node and list-entry counts (0, 0 for a table of its own).
    fn base_lens(&self) -> (usize, usize) {
        match self.base {
            Some(b) => (b.own.nodes().len(), b.own.lists().len()),
            None => (0, 0),
        }
    }

    /// The table, borrowed. Over [`Stable`] columns the view is LIVE ([`PTys`]): what is
    /// interned through `&self` after it is readable through it by id.
    pub fn view(&self) -> PTys<'_> {
        let snap = match self.base {
            Some(b) => PTys::overlay(
                b.view(),
                self.own.nodes(),
                self.own.lists(),
                self.own.record_schemas(),
                self.own.record_names(),
            ),
            None => PTys::flat_with_records(
                self.own.nodes(),
                self.own.lists(),
                self.own.record_schemas(),
                self.own.record_names(),
            ),
        };
        match &self.own {
            Own::Owned { .. } => snap,
            Own::Stable(s) => snap.with_live(
                self.base.is_some(),
                s.node_col(),
                s.list_col(),
                s.record_schema_col(),
                s.record_name_col(),
            ),
        }
    }

    fn has_record_schema(&self, schema: RecordSchema) -> bool {
        if let Some(b) = self.base
            && b.has_record_schema(schema)
        {
            return true;
        }
        self.own.record_schemas().iter().any(|&(s, _)| s == schema)
    }

    /// Registers `schema -> sorted_names` in this table (idempotent across base and own).
    pub fn register_record_schema(&mut self, schema: RecordSchema, sorted_names: &[Name]) -> Result<(), Full> {
        match &mut self.own {
            Own::Owned { record_schemas, record_names, .. } => {
                if let Some(b) = self.base
                    && b.has_record_schema(schema)
                {
                    return Ok(());
                }
                if record_schemas.iter().any(|&(s, _)| s == schema) {
                    return Ok(());
                }
                let base_n = self.base.map_or(0, |b| b.own.record_names().len());
                let len = u32::try_from(sorted_names.len()).map_err(|_| Full)?;
                let list = if sorted_names.is_empty() {
                    List::EMPTY
                } else {
                    let start = u32::try_from(base_n + record_names.len()).map_err(|_| Full)?;
                    record_names.extend_from_slice(sorted_names);
                    List { start, len }
                };
                record_schemas.push((schema, list));
                Ok(())
            }
            Own::Stable(s) => s.register_record_schema(self.base, schema, sorted_names),
        }
    }

    /// Registers `schema -> sorted_names` through a shared borrow, for a table over [`Stable`]
    /// columns.
    pub fn register_record_schema_shared(&self, schema: RecordSchema, sorted_names: &[Name]) -> Result<(), CheckError> {
        match &self.own {
            Own::Owned { .. } => Err(CheckError::Malformed(Malformed::SharedIntern)),
            Own::Stable(s) => s.register_record_schema(self.base, schema, sorted_names).map_err(table_full),
        }
    }

    /// Copies every registered record schema from `src` into this table.
    pub fn copy_record_schemas_from(&mut self, src: PTys<'_>) -> Result<(), Full> {
        for (schema, names) in src.record_schemas() {
            self.register_record_schema(schema, names)?;
        }
        Ok(())
    }

    /// Copies every registered record schema from `src` through a shared borrow.
    pub fn copy_record_schemas_from_shared(&self, src: PTys<'_>) -> Result<(), CheckError> {
        for (schema, names) in src.record_schemas() {
            self.register_record_schema_shared(schema, names)?;
        }
        Ok(())
    }

    /// The own part (the whole table when it has no base) copied into `alloc`, with the base's
    /// lengths: what a body keeps once its table's borrow of the shared table ends. The index
    /// is not kept: a kept table is read, never interned into.
    pub fn keep_in<'k>(&self, alloc: Alloc<'k>) -> Result<OwnTys<'k>, CheckError> {
        let (bn, bl) = self.base_lens();
        let (brs, brn) = match self.base {
            Some(b) => (b.own.record_schemas().len(), b.own.record_names().len()),
            None => (0, 0),
        };
        let limit = |_| CheckError::Limit(crate::error::Limit::Table);
        let mut nodes = Vec::with_capacity_in(self.own.nodes().len(), alloc);
        nodes.extend_from_slice(self.own.nodes());
        let mut lists = Vec::with_capacity_in(self.own.lists().len(), alloc);
        lists.extend_from_slice(self.own.lists());
        let mut record_schemas = Vec::with_capacity_in(self.own.record_schemas().len(), alloc);
        record_schemas.extend_from_slice(self.own.record_schemas());
        let mut record_names = Vec::with_capacity_in(self.own.record_names().len(), alloc);
        record_names.extend_from_slice(self.own.record_names());
        Ok(OwnTys {
            base_nodes: u32::try_from(bn).map_err(limit)?,
            base_lists: u32::try_from(bl).map_err(limit)?,
            base_record_schemas: u32::try_from(brs).map_err(limit)?,
            base_record_names: u32::try_from(brn).map_err(limit)?,
            nodes,
            lists,
            record_schemas,
            record_names,
            whole: false,
        })
    }

    /// The node `n`, interned. Its children must already be in the table.
    pub fn intern(&mut self, n: PNode) -> Result<PTy, CheckError> {
        match &mut self.own {
            Own::Owned { nodes, index, .. } => intern_node(self.base, index, nodes.len(), n, |n| {
                nodes.push(n);
                Ok(())
            }),
            Own::Stable(s) => s.intern(self.base, n),
        }
    }

    /// [`PortableTys::intern`] through a shared borrow, for a table over [`Stable`] columns:
    /// views taken before it stay valid (a shorter prefix of the same columns). A table with
    /// owned columns ([`PortableTys::new_in`]) refuses it: `Malformed::SharedIntern`.
    pub fn intern_shared(&self, n: PNode) -> Result<PTy, CheckError> {
        match &self.own {
            Own::Owned { .. } => Err(CheckError::Malformed(Malformed::SharedIntern)),
            Own::Stable(s) => s.intern(self.base, n),
        }
    }

    /// The `usize` value `n` (an array length), interned with its type.
    pub fn usize_value(&mut self, n: u64) -> Result<PTy, CheckError> {
        let ty = self.intern(PNode::Uint(crate::ids::UintTy::Usize))?;
        self.intern(PNode::ConstValue { ty, bits: u128::from(n) })
    }

    /// The value of the unit variant `variant` (its index, as `Def::Ctor` counts) of the enum
    /// `adt`, which has no generics: a const argument of an enum type (the pinned library,
    /// core/src/intrinsics/mod.rs: `pub unsafe fn atomic_xadd<T: Copy, U: Copy, const ORD:
    /// AtomicOrdering>(dst: *mut T, src: U) -> T;`, with `AtomicOrdering` deriving
    /// `ConstParamTy`). Two values are equal when their enum and variant are.
    pub fn variant_value(&mut self, adt: ItemKey, variant: u32) -> Result<PTy, CheckError> {
        let none = self.intern_list(&[])?;
        let ty = self.intern(PNode::Adt(adt, none))?;
        self.intern(PNode::ConstValue { ty, bits: u128::from(variant) })
    }

    /// `[elem; n]`, interned with its length.
    pub fn array_n(&mut self, elem: PTy, n: u64) -> Result<PTy, CheckError> {
        let len = self.usize_value(n)?;
        self.intern(PNode::Array(elem, len))
    }

    /// The pattern type `base is ranges` in its canonical form (typeck.md, "Pattern types").
    /// Each range is `start ..= end` over the base's bits (`width` of them, truncated two's
    /// complement for a signed base, a `usize` address for a raw pointer); `start > end` wraps,
    /// so a signed `-5 ..= 5` is passed as its bits and means what it says. The ranges are
    /// sorted, merged when they overlap or touch, and a run ending at the maximum merged with one
    /// starting at zero into one wrapping range, so one set of values has one node. The values
    /// are `ConstValue`s of `value_ty` (the base, or `usize` for a pointer).
    pub fn pattern(
        &mut self,
        base: PTy,
        value_ty: PTy,
        width: u32,
        ranges: &[(u128, u128)],
    ) -> Result<PTy, CheckError> {
        let (runs, n) = canonical_ranges(width, ranges)?;
        let mut terms = [PTy(0); 2 * MAX_PATTERN_RANGES];
        for (i, (start, end)) in runs[..n].iter().copied().enumerate() {
            terms[2 * i] = self.intern(PNode::ConstValue { ty: value_ty, bits: start })?;
            terms[2 * i + 1] = self.intern(PNode::ConstValue { ty: value_ty, bits: end })?;
        }
        let list = self.intern_list(&terms[..2 * n])?;
        self.intern(PNode::Pat(base, list))
    }

    /// The trait object over `preds` (`ExTrait`, `ExProj` and `ExAuto` nodes of this table, in
    /// any order, repeats allowed) in its one form (`PNode::Dynamic`): the Reference, "Trait
    /// object types": the order and repetition of auto traits do not make another type.
    pub fn dynamic(&mut self, preds: &[PTy]) -> Result<PTy, CheckError> {
        self.dynamic_rows(preds, true)
    }

    /// The planted bug of `suite::dyn_objects::control_object_order`: rows kept as written.
    #[cfg(test)]
    pub(crate) fn dynamic_unsorted(&mut self, preds: &[PTy]) -> Result<PTy, CheckError> {
        self.dynamic_rows(preds, false)
    }

    /// [`PortableTys::dynamic`] through a shared borrow (as [`PortableTys::intern_shared`]): a
    /// head decoded into a body's table while the body's check reads it.
    pub fn dynamic_shared(&self, preds: &[PTy]) -> Result<PTy, CheckError> {
        let (list, n) = object_rows(self.view(), preds, true)?;
        let list = self.intern_list_shared(&list[..n])?;
        self.intern_shared(PNode::Dynamic(list))
    }

    /// `dynamic`, its rows sorted by `object_pred_key` (`sort`) or, the planted bug, as written.
    fn dynamic_rows(&mut self, preds: &[PTy], sort: bool) -> Result<PTy, CheckError> {
        let (list, n) = object_rows(self.view(), preds, sort)?;
        let list = self.intern_list(&list[..n])?;
        self.intern(PNode::Dynamic(list))
    }
}

/// A trait object's rows in its one order (`sort`; the planted bug: as written), repeats
/// dropped: the list `PNode::Dynamic` holds, its first `n` entries.
fn object_rows(view: PTys<'_>, preds: &[PTy], sort: bool) -> Result<([PTy; MAX_OBJECT_PREDS], usize), CheckError> {
    if preds.len() > MAX_OBJECT_PREDS {
        return Err(CheckError::Limit(crate::error::Limit::ObjectPreds));
    }
    let mut rows = [((0u8, 0u128, 0u64), PTy(0)); MAX_OBJECT_PREDS];
    let mut n = 0;
    for &p in preds {
        let bad = || CheckError::Malformed(crate::error::Malformed::ObjectPreds { node: p.0 });
        let key = object_pred_key(view.node(p)?).ok_or_else(bad)?;
        // Insertion by key; an equal key is a repeat (one row per auto trait) unless the rows
        // differ: two principals are two non-auto traits ("There may not be more than one
        // non-auto trait"), two bindings of one (trait, name) to two terms are E0719.
        let mut at = n;
        while sort && at > 0 && rows[at - 1].0 > key {
            at -= 1;
        }
        let same = match sort {
            true => (at > 0 && rows[at - 1].0 == key).then(|| at - 1),
            false => rows[..n].iter().position(|r| r.0 == key),
        };
        if let Some(j) = same {
            if key.0 != 2 && rows[j].1 != p {
                return Err(bad());
            }
            continue;
        }
        let mut i = n;
        while i > at {
            rows[i] = rows[i - 1];
            i -= 1;
        }
        rows[at] = (key, p);
        n += 1;
    }
    let mut list = [PTy(0); MAX_OBJECT_PREDS];
    for i in 0..n {
        list[i] = rows[i].1;
    }
    Ok((list, n))
}

impl<'a> PortableTys<'a> {
    /// The list `tys`, interned.
    pub fn intern_list(&mut self, tys: &[PTy]) -> Result<List, CheckError> {
        if tys.is_empty() {
            return Ok(List::EMPTY);
        }
        let hash = list_hash(tys);
        match &mut self.own {
            Own::Owned { lists, index, .. } => match place_list(self.base, index, lists, hash, tys)? {
                Ok(found) => Ok(found),
                Err(new) => {
                    lists.extend_from_slice(tys);
                    file_list(index, hash, new)
                }
            },
            Own::Stable(s) => s.intern_list(self.base, hash, tys),
        }
    }

    /// [`PortableTys::intern_list`] through a shared borrow (as [`PortableTys::intern_shared`]).
    pub fn intern_list_shared(&self, tys: &[PTy]) -> Result<List, CheckError> {
        if tys.is_empty() {
            return Ok(List::EMPTY);
        }
        match &self.own {
            Own::Owned { .. } => Err(CheckError::Malformed(Malformed::SharedIntern)),
            Own::Stable(s) => s.intern_list(self.base, list_hash(tys), tys),
        }
    }

    /// The number of nodes (the base's and the own).
    pub fn len(&self) -> usize {
        self.base_lens().0 + self.own.nodes().len()
    }

    /// A copy of the whole table (base and own) in `alloc`, as a table of its own: every node
    /// and list at the same index, so a type of this table is the same type of the copy. The
    /// back half's one table starts as a copy of the records' table (program.rs), once.
    pub fn fork_in<'b>(&self, alloc: Alloc<'b>) -> Result<PortableTys<'b>, CheckError> {
        let mut out = PortableTys::new_in(alloc);
        let view = self.view();
        for &n in view.nodes() {
            out.intern(n)?;
        }
        if let Some(b) = self.base {
            b.each_list_row(&mut |l| out.intern_list(view.list(l)?).map(drop))?;
        }
        self.each_list_row(&mut |l| out.intern_list(view.list(l)?).map(drop))?;
        out.copy_record_schemas_from(view).map_err(table_full)?;
        Ok(out)
    }
}

impl<'a> PartialEq for PortableTys<'a> {
    fn eq(&self, other: &PortableTys<'a>) -> bool {
        let (a, b) = (self.view(), other.view());
        a.len() == b.len()
            && a.lists_len() == b.lists_len()
            && a.record_schemas_len() == b.record_schemas_len()
            && a.record_names_len() == b.record_names_len()
            && a.nodes().eq(b.nodes())
            && a.lists().eq(b.lists())
            && a.record_schema_rows().eq(b.record_schema_rows())
            && a.record_names_iter().eq(b.record_names_iter())
    }
}

impl<'a> Eq for PortableTys<'a> {}

impl<'a> core::fmt::Debug for PortableTys<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (bn, bl) = self.base_lens();
        f.debug_struct("PortableTys")
            .field("base_nodes", &bn)
            .field("base_lists", &bl)
            .field("nodes", &self.own.nodes())
            .field("lists", &self.own.lists())
            .field("record_schemas", &self.own.record_schemas())
            .field("record_names", &self.own.record_names())
            .finish()
    }
}

/// What a body keeps of its table once the borrow of the shared table ends: its own nodes and
/// list entries, and how long the shared table was when the body began. Read with the shared
/// table passed again ([`OwnTys::view`]); the shared table only grows, so its first
/// `base_nodes` nodes still mean what they meant, and the ids it gained since are never read
/// through this body (its own ids occupy that range).
pub struct OwnTys<'a> {
    base_nodes: u32,
    base_lists: u32,
    base_record_schemas: u32,
    base_record_names: u32,
    nodes: Vec<PNode, Alloc<'a>>,
    lists: Vec<PTy, Alloc<'a>>,
    record_schemas: Vec<(RecordSchema, List), Alloc<'a>>,
    record_names: Vec<Name, Alloc<'a>>,
    /// The body's types are the base's own (lowered straight into it): read it whole.
    whole: bool,
}

impl<'a> OwnTys<'a> {
    /// No types: a body whose types went into another table (`lower_again`).
    pub fn none(alloc: Alloc<'a>) -> OwnTys<'a> {
        OwnTys {
            base_nodes: 0,
            base_lists: 0,
            base_record_schemas: 0,
            base_record_names: 0,
            nodes: Vec::new_in(alloc),
            lists: Vec::new_in(alloc),
            record_schemas: Vec::new_in(alloc),
            record_names: Vec::new_in(alloc),
            whole: false,
        }
    }

    /// A body lowered straight into the table it is read over (the seeded path: one body's
    /// private store, its table over [`Stable`] columns, no overlay): [`OwnTys::view`] is
    /// that table's own view, LIVE ([`PTys`]), so one table numbers every type once and a
    /// head imported into it during the check is read through a view taken before.
    pub fn in_base(alloc: Alloc<'a>) -> OwnTys<'a> {
        OwnTys { whole: true, ..OwnTys::none(alloc) }
    }

    /// The number of own nodes.
    pub fn own_len(&self) -> usize {
        self.nodes.len()
    }

    /// The body's table: `base`'s first nodes and list entries as they were when the body
    /// began, then the body's own. Refused (`Malformed::Overlay`) when `base` is shorter than
    /// that or is itself an overlay.
    pub fn view<'s>(&'s self, base: &'s PortableTys<'_>) -> Result<PTys<'s>, CheckError> {
        self.view_over(base, true)
    }

    /// The planted bug of the overlay test: the base read at its CURRENT length, so an id the
    /// shared table gained after the body began shadows the body's own type at that id.
    #[cfg(test)]
    pub(crate) fn view_untruncated<'s>(&'s self, base: &'s PortableTys<'_>) -> Result<PTys<'s>, CheckError> {
        self.view_over(base, false)
    }

    fn view_over<'s>(&'s self, base: &'s PortableTys<'_>, truncate: bool) -> Result<PTys<'s>, CheckError> {
        let have = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        if base.base.is_some() {
            return Err(CheckError::Malformed(Malformed::Overlay { base: self.base_nodes, have: have(base.len()) }));
        }
        if self.whole {
            return Ok(base.view());
        }
        let (bn, bl) = (self.base_nodes as usize, self.base_lists as usize);
        let (brs, brn) = (usize::try_from(self.base_record_schemas).unwrap_or(usize::MAX), usize::try_from(self.base_record_names).unwrap_or(usize::MAX));
        let (base_nodes, base_lists) = (base.own.nodes(), base.own.lists());
        let (base_record_schemas, base_record_names) = (base.own.record_schemas(), base.own.record_names());
        let (Some(nodes), Some(lists), Some(rs), Some(rn)) = (
            base_nodes.get(..bn),
            base_lists.get(..bl),
            base_record_schemas.get(..brs),
            base_record_names.get(..brn),
        ) else {
            let (want, got) = if base_nodes.len() < bn {
                (self.base_nodes, base_nodes.len())
            } else if base_lists.len() < bl {
                (self.base_lists, base_lists.len())
            } else if base_record_schemas.len() < brs {
                (self.base_record_schemas, base_record_schemas.len())
            } else {
                (self.base_record_names, base_record_names.len())
            };
            return Err(CheckError::Malformed(Malformed::Overlay { base: want, have: have(got) }));
        };
        let (nodes, lists, rs, rn) = if truncate {
            (nodes, lists, rs, rn)
        } else {
            (base_nodes, base_lists, base_record_schemas, base_record_names)
        };
        Ok(PTys::overlay(
            PTys::flat_with_records(nodes, lists, rs, rn),
            &self.nodes,
            &self.lists,
            &self.record_schemas,
            &self.record_names,
        ))
    }
}
