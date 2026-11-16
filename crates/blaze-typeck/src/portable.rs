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
    /// A closure type (the Reference, "Closure types": each closure expression has a unique
    /// anonymous type): the item whose body holds the expression, the closure's ordinal among
    /// that body's closures in expression order, and the owner's generic arguments in scope
    /// (`Param(0..n)` in its own record). Its signature, kind and captures are facts of the
    /// owner's `Checked::closures`, asked as `Question::Closure` (typeck.md, "A closure's
    /// identity"); never part of the type.
    Closure { owner: ItemKey, ordinal: u32, args: List },
    /// An async block's future, by its expression: its output and its interior
    /// (`TyView::Future`).
    Future(ExprId, PTy, PTy),
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

/// A borrowed portable table.
#[derive(Clone, Copy, Debug)]
pub struct PTys<'a> {
    pub nodes: &'a [PNode],
    pub lists: &'a [PTy],
}

impl<'a> PTys<'a> {
    /// The empty table.
    pub const EMPTY: PTys<'static> = PTys { nodes: &[], lists: &[] };

    /// The node of `t`.
    pub fn node(&self, t: PTy) -> Result<PNode, CheckError> {
        match self.nodes.get(t.0 as usize) {
            Some(n) => Ok(*n),
            None => Err(CheckError::Malformed(Malformed::TypeOrder { node: t.0, child: t.0 })),
        }
    }

    /// The value of the const term `t` when it is a `ConstValue` that fits in a `u64` (an array
    /// length once its item is instantiated); `None` for a parameter, a hole or anything else.
    pub fn value_u64(&self, t: PTy) -> Option<u64> {
        match self.nodes.get(t.0 as usize)? {
            PNode::ConstValue { bits, .. } => u64::try_from(*bits).ok(),
            _ => None,
        }
    }

    /// The children of a list.
    pub fn list(&self, l: List) -> Result<&'a [PTy], CheckError> {
        match self.lists.get(l.range()) {
            Some(s) => Ok(s),
            None => Err(CheckError::Malformed(Malformed::List {
                column: "types.lists",
                start: l.start,
                len: l.len,
            })),
        }
    }
}

impl ContentHash for PTys<'_> {
    fn hash_into(&self, h: &mut StableHasher) {
        h.u64(self.nodes.len() as u64);
        for n in self.nodes {
            hash_node(n, h);
        }
        h.u64(self.lists.len() as u64);
        for t in self.lists {
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
            let p = t.lists.get(i).ok_or(CheckError::Malformed(Malformed::TypeOrder { node: i as u32, child: i as u32 }))?;
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

/// A trait object's impl-lookup shape key: its principal trait, from its rows `l` of `t`
/// (`None`: auto traits only, or a list past the table).
pub fn object_principal(t: PTys<'_>, l: List) -> Option<ItemKey> {
    match t.nodes.get(t.lists.get(l.range())?.first()?.0 as usize)? {
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
pub struct PortableTys<'a> {
    nodes: Vec<PNode, Alloc<'a>>,
    lists: Vec<PTy, Alloc<'a>>,
    node_index: HashMap<PNode, u32, Alloc<'a>>,
    list_index: ListIndex<'a>,
    /// Per interned list, in the order interned: where it is in `lists`.
    list_rows: Vec<List, Alloc<'a>>,
}

impl<'a> PortableTys<'a> {
    /// An empty table.
    pub fn new_in(alloc: Alloc<'a>) -> PortableTys<'a> {
        PortableTys {
            nodes: Vec::new_in(alloc),
            lists: Vec::new_in(alloc),
            node_index: HashMap::new_in(alloc),
            list_index: ListIndex::new_in(alloc),
            list_rows: Vec::new_in(alloc),
        }
    }

    /// The table, borrowed.
    pub fn view(&self) -> PTys<'_> {
        PTys { nodes: &self.nodes, lists: &self.lists }
    }

    /// The node `n`, interned. Its children must already be in the table.
    pub fn intern(&mut self, n: PNode) -> Result<PTy, CheckError> {
        if let Some(&found) = self.node_index.get(&n) {
            return Ok(PTy(found));
        }
        let at = u32::try_from(self.nodes.len())
            .map_err(|_| CheckError::Limit(crate::error::Limit::Table))?;
        self.nodes.push(n);
        self.node_index.insert(n, at);
        Ok(PTy(at))
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
        let mut h = StableHasher::new("ptys.list");
        for t in tys {
            h.u32(t.0);
        }
        let hash = h.finish();
        let (lists, rows) = (&self.lists, &self.list_rows);
        if let Some(found) = self.list_index.find(hash, |id| lists.get(rows[id as usize].range()) == Some(tys)) {
            return Ok(self.list_rows[found as usize]);
        }
        let start = u32::try_from(self.lists.len())
            .map_err(|_| CheckError::Limit(crate::error::Limit::Table))?;
        let len = u32::try_from(tys.len()).map_err(|_| CheckError::Limit(crate::error::Limit::Table))?;
        self.list_index.push(hash)?;
        self.lists.extend_from_slice(tys);
        let list = List { start, len };
        self.list_rows.push(list);
        Ok(list)
    }

    /// The number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// A copy of the table in `alloc`: every node and list at the same index, so a type of
    /// this table is the same type of the copy. A body's lowering extends a copy of the
    /// records' table, so what only a body has (a closure, a future, a hole) is never in a
    /// table a record is answered from (`Malformed::BodyTypeInRecord`).
    pub fn fork_in<'b>(&self, alloc: Alloc<'b>) -> Result<PortableTys<'b>, CheckError> {
        let mut out = PortableTys::new_in(alloc);
        for &n in &self.nodes {
            out.intern(n)?;
        }
        for &l in &self.list_rows {
            out.intern_list(&self.lists[l.range()])?;
        }
        Ok(out)
    }
}

impl<'a> PartialEq for PortableTys<'a> {
    fn eq(&self, other: &PortableTys<'a>) -> bool {
        self.nodes == other.nodes && self.lists == other.lists
    }
}

impl<'a> Eq for PortableTys<'a> {}

impl<'a> core::fmt::Debug for PortableTys<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PortableTys")
            .field("nodes", &&self.nodes[..])
            .field("lists", &&self.lists[..])
            .finish()
    }
}
