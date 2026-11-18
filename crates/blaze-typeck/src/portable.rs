//! The portable type form: what a world's answer and the body's record hold (typeck.md,
//! "`TypeCtx`: what is asked of blaze-ty"). Items by key, generic parameters by index, children
//! before parents in one node column, lists of children in one side column. Becomes blaze-ty's
//! portable type table; what this crate needs of it is exactly this shape.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Malformed};
use crate::hash::{ContentHash, StableHasher};
use crate::ids::{ExprId, FloatTy, FnPtrHeader, IntTy, ItemKey, List, Mutbl, Name, UintTy};
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
}

/// A borrowed portable table: a base (the shared table as it stood, read in place) and the
/// nodes and list entries after it (a body's own, numbered on from the base's lengths; empty
/// for a table read whole). One numbering over both: `PTy(i)` below the base's length is the
/// base's node `i`, from it up the own node `i - base`; a list lies wholly in one part. The
/// parts are private, so no reader can take the base for the whole table.
#[derive(Clone, Copy, Debug)]
pub struct PTys<'a> {
    nodes: &'a [PNode],
    lists: &'a [PTy],
    own_nodes: &'a [PNode],
    own_lists: &'a [PTy],
}

impl<'a> PTys<'a> {
    /// The empty table.
    pub const EMPTY: PTys<'static> = PTys { nodes: &[], lists: &[], own_nodes: &[], own_lists: &[] };

    /// A table held in two columns, nodes and list entries.
    pub const fn flat(nodes: &'a [PNode], lists: &'a [PTy]) -> PTys<'a> {
        PTys { nodes, lists, own_nodes: &[], own_lists: &[] }
    }

    /// The base `base` with `own_nodes` and `own_lists` numbered on after it.
    pub(crate) const fn overlay(base: PTys<'a>, own_nodes: &'a [PNode], own_lists: &'a [PTy]) -> PTys<'a> {
        PTys { nodes: base.nodes, lists: base.lists, own_nodes, own_lists }
    }

    /// The node of `t`, borrowed; `None` past the table.
    #[inline]
    pub fn get(&self, t: PTy) -> Option<&'a PNode> {
        let i = t.0 as usize;
        match i.checked_sub(self.nodes.len()) {
            None => self.nodes.get(i),
            Some(own) => self.own_nodes.get(own),
        }
    }

    /// The number of nodes.
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len() + self.own_nodes.len()
    }

    /// Whether the table has no node.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of nodes in the base (the first own node's index).
    pub fn base_len(&self) -> usize {
        self.nodes.len()
    }

    /// Every node, in index order (`PTy(0)` first).
    pub fn nodes(&self) -> impl Iterator<Item = &'a PNode> + Clone + 'a {
        self.nodes.iter().chain(self.own_nodes.iter())
    }

    /// Every list entry, in the list column's order.
    pub fn lists(&self) -> impl Iterator<Item = &'a PTy> + Clone + 'a {
        self.lists.iter().chain(self.own_lists.iter())
    }

    /// The number of list entries (the list column's length).
    pub fn lists_len(&self) -> usize {
        self.lists.len() + self.own_lists.len()
    }

    /// The list entry at `i` of the list column; `None` past it.
    #[inline]
    pub fn list_slot(&self, i: usize) -> Option<PTy> {
        match i.checked_sub(self.lists.len()) {
            None => self.lists.get(i).copied(),
            Some(own) => self.own_lists.get(own).copied(),
        }
    }

    /// The list entries in `r`; `None` when `r` is not within one part of the column (a list
    /// never straddles the base's end).
    pub fn list_at(&self, r: core::ops::Range<usize>) -> Option<&'a [PTy]> {
        let base = self.lists.len();
        if r.end <= base {
            self.lists.get(r)
        } else if r.start >= base {
            self.own_lists.get(r.start - base..r.end - base)
        } else {
            None
        }
    }

    /// The table's identity while it is borrowed: two views of one table are one identity. For
    /// a memo only; never part of a key, a record or an output. Two overlays of one base
    /// differ in their own parts' addresses (an empty own part has nothing to tell apart).
    pub fn identity(&self) -> (usize, usize) {
        (self.nodes.as_ptr() as usize, self.own_nodes.as_ptr() as usize)
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
/// `ConstValue`'s type too): the one walk order of a record's terms, for converting them
/// (infer/facts.rs) and for hashing them (world.rs `Hashes`).
pub(crate) fn node_children(t: PTys<'_>, n: PNode, out: &mut blaze_own::Vec<u32, blaze_own::Alloc<'_>>) -> Result<(), CheckError> {
    let mut list = |l: List, out: &mut blaze_own::Vec<u32, blaze_own::Alloc<'_>>| -> Result<(), CheckError> {
        for i in l.range() {
            let p = t.list_slot(i).ok_or(CheckError::Malformed(Malformed::TypeOrder { node: i as u32, child: i as u32 }))?;
            out.push(p.0);
        }
        Ok(())
    };
    match n {
        PNode::Bool
        | PNode::Char
        | PNode::Int(_)
        | PNode::Uint(_)
        | PNode::Float(_)
        | PNode::Str
        | PNode::Never
        | PNode::Param(_)
        | PNode::ConstParam(_)
        | PNode::Error
        | PNode::ExAuto(_)
        | PNode::Future(..)
        | PNode::Infer => Ok(()),
        PNode::ConstValue { ty: p, .. } | PNode::Ref(_, p) | PNode::Ptr(_, p) | PNode::Slice(p) => {
            out.push(p.0);
            Ok(())
        }
        PNode::Array(e, len) => {
            out.push(e.0);
            out.push(len.0);
            Ok(())
        }
        PNode::Pat(b, l) => {
            out.push(b.0);
            list(l, out)
        }
        PNode::Tuple(l)
        | PNode::Adt(_, l)
        | PNode::FnDef(_, l)
        | PNode::FnPtr(l, _)
        | PNode::Opaque(_, l)
        | PNode::Dynamic(l)
        | PNode::ExTrait(_, l)
        | PNode::Closure { args: l, .. }
        | PNode::Coroutine { args: l, .. }
        | PNode::Alias { args: l, .. } => list(l, out),
        PNode::ExProj { args, term, .. } => {
            list(args, out)?;
            out.push(term.0);
            Ok(())
        }
    }
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
    let p = PTy(0);
    let l = |l: List| List { start: 0, len: l.len };
    let shape = match *n {
        PNode::Tuple(x) => PNode::Tuple(l(x)),
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
        | PNode::ExAuto(_)) => leaf,
    };
    hash_node(&shape, h);
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
/// `&self`. Implemented by the caller (blaze-ladder over `blaze_mem::AppendVec`).
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
    Owned { nodes: Vec<PNode, Alloc<'a>>, lists: Vec<PTy, Alloc<'a>>, index: OwnIndex<'a> },
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
    index: RefCell<OwnIndex<'a>>,
}

/// [`StableCols`] with its lifetime behind the object.
trait StableOwn {
    fn nodes(&self) -> &[PNode];
    fn lists(&self) -> &[PTy];
    fn find_node(&self, n: &PNode) -> Option<u32>;
    fn find_list(&self, offset: usize, hash: u64, tys: &[PTy]) -> Option<List>;
    fn intern(&self, base: Option<&PortableTys<'_>>, n: PNode) -> Result<PTy, CheckError>;
    fn intern_list(&self, base: Option<&PortableTys<'_>>, hash: u64, tys: &[PTy]) -> Result<List, CheckError>;
    fn list_rows(&self, each: &mut dyn FnMut(List) -> Result<(), CheckError>) -> Result<(), CheckError>;
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

    fn list_rows(&self, each: &mut dyn FnMut(List) -> Result<(), CheckError>) -> Result<(), CheckError> {
        self.index.borrow().list_rows.iter().try_for_each(|&l| each(l))
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
    /// The allocator this table's own part lives in: its maker's, handed down (D102), so a
    /// question asked over the table (the run path's `drop_impl`) allocates its scratch there
    /// and fetches no allocator of its own.
    pub fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// An empty table, interned through `&mut`.
    pub fn new_in(alloc: Alloc<'a>) -> PortableTys<'a> {
        PortableTys { base: None, alloc, own: Own::Owned { nodes: Vec::new_in(alloc), lists: Vec::new_in(alloc), index: OwnIndex::new_in(alloc) } }
    }

    /// An empty table over the caller's [`Stable`] columns (empty when handed in), interned
    /// through `&self` too ([`PortableTys::intern_shared`]); its index in `alloc`.
    pub fn new_stable(alloc: Alloc<'a>, nodes: Box<dyn Stable<PNode> + 'a, Alloc<'a>>, lists: Box<dyn Stable<PTy> + 'a, Alloc<'a>>) -> Result<PortableTys<'a>, CheckError> {
        if !nodes.as_slice().is_empty() || !lists.as_slice().is_empty() {
            let n = u32::try_from(nodes.as_slice().len()).unwrap_or(u32::MAX);
            return Err(CheckError::Malformed(Malformed::Overlay { base: 0, have: n }));
        }
        let cols: Box<dyn StableOwn + 'a, Alloc<'a>> = Box::new_in(StableCols { nodes, lists, index: RefCell::new(OwnIndex::new_in(alloc)) }, alloc);
        Ok(PortableTys { base: None, alloc, own: Own::Stable(cols) })
    }

    /// [`PortableTys::over`] with the own part in [`Stable`] columns ([`PortableTys::new_stable`]).
    pub fn over_stable(base: &'a PortableTys<'a>, alloc: Alloc<'a>, nodes: Box<dyn Stable<PNode> + 'a, Alloc<'a>>, lists: Box<dyn Stable<PTy> + 'a, Alloc<'a>>) -> Result<PortableTys<'a>, CheckError> {
        let mut t = PortableTys::new_stable(alloc, nodes, lists)?;
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

    /// The table, borrowed.
    pub fn view(&self) -> PTys<'_> {
        match self.base {
            Some(b) => PTys::overlay(b.view(), self.own.nodes(), self.own.lists()),
            None => PTys::flat(self.own.nodes(), self.own.lists()),
        }
    }

    /// The own part (the whole table when it has no base) copied into `alloc`, with the base's
    /// lengths: what a body keeps once its table's borrow of the shared table ends. The index
    /// is not kept: a kept table is read, never interned into.
    pub fn keep_in<'k>(&self, alloc: Alloc<'k>) -> Result<OwnTys<'k>, CheckError> {
        let (bn, bl) = self.base_lens();
        let limit = |_| CheckError::Limit(crate::error::Limit::Table);
        let mut nodes = Vec::with_capacity_in(self.own.nodes().len(), alloc);
        nodes.extend_from_slice(self.own.nodes());
        let mut lists = Vec::with_capacity_in(self.own.lists().len(), alloc);
        lists.extend_from_slice(self.own.lists());
        Ok(OwnTys { base_nodes: u32::try_from(bn).map_err(limit)?, base_lists: u32::try_from(bl).map_err(limit)?, nodes, lists })
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

    /// `dynamic`, its rows sorted by `object_pred_key` (`sort`) or, the planted bug, as written.
    fn dynamic_rows(&mut self, preds: &[PTy], sort: bool) -> Result<PTy, CheckError> {
        if preds.len() > MAX_OBJECT_PREDS {
            return Err(CheckError::Limit(crate::error::Limit::ObjectPreds));
        }
        let mut rows = [((0u8, 0u128, 0u64), PTy(0)); MAX_OBJECT_PREDS];
        let mut n = 0;
        for &p in preds {
            let bad = || CheckError::Malformed(crate::error::Malformed::ObjectPreds { node: p.0 });
            let key = object_pred_key(self.view().node(p)?).ok_or_else(bad)?;
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
        let list = self.intern_list(&list[..n])?;
        self.intern(PNode::Dynamic(list))
    }

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
        for &n in self.view().nodes() {
            out.intern(n)?;
        }
        let view = self.view();
        if let Some(b) = self.base {
            b.each_list_row(&mut |l| out.intern_list(view.list(l)?).map(drop))?;
        }
        self.each_list_row(&mut |l| out.intern_list(view.list(l)?).map(drop))?;
        Ok(out)
    }
}

impl<'a> PartialEq for PortableTys<'a> {
    fn eq(&self, other: &PortableTys<'a>) -> bool {
        let (a, b) = (self.view(), other.view());
        a.len() == b.len() && a.lists_len() == b.lists_len() && a.nodes().eq(b.nodes()) && a.lists().eq(b.lists())
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
    nodes: Vec<PNode, Alloc<'a>>,
    lists: Vec<PTy, Alloc<'a>>,
}

impl<'a> OwnTys<'a> {
    /// No types: a body whose types went into another table (`lower_again`).
    pub fn none(alloc: Alloc<'a>) -> OwnTys<'a> {
        OwnTys { base_nodes: 0, base_lists: 0, nodes: Vec::new_in(alloc), lists: Vec::new_in(alloc) }
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
        let (bn, bl) = (self.base_nodes as usize, self.base_lists as usize);
        let (base_nodes, base_lists) = (base.own.nodes(), base.own.lists());
        let (Some(nodes), Some(lists)) = (base_nodes.get(..bn), base_lists.get(..bl)) else {
            let (want, got) = if base_nodes.len() < bn { (self.base_nodes, base_nodes.len()) } else { (self.base_lists, base_lists.len()) };
            return Err(CheckError::Malformed(Malformed::Overlay { base: want, have: have(got) }));
        };
        let (nodes, lists) = if truncate { (nodes, lists) } else { (base_nodes, base_lists) };
        Ok(PTys::overlay(PTys::flat(nodes, lists), &self.nodes, &self.lists))
    }
}
