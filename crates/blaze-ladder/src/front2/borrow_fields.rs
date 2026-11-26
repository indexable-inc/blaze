//! Two facts of an ADT the borrow check's lowering asks (blaze-flow-lower `Facts`), lane B11:
//!
//! - [`freeze_field_regions`]: each field's regions (`Facts::field_regions`), taken from the
//!   field's REGION COLUMN in the ADT's head (D148: `RegionFacts::field`, laid by piece L1b's
//!   `adt_regions`), never read again from its text. THE RULE (the Reference, "Generic
//!   parameters", "Lifetime elision"): a field's type is its declared type at the value's
//!   arguments, so each lifetime it writes IS one of the ADT's lifetime parameters
//!   (`Region::Param(i)`, declaration order: an ADT has no container and elides nothing, E0106),
//!   or `'static`; a lifetime a fn pointer's binder binds is `Region::Late`; each type parameter
//!   it names stands for every position of that argument. The column is in the one counter's
//!   pre-order (blaze-typeck regions.rs `region_arity`), which is the lowering's position order
//!   (`npos`) at every node the lowering gives positions to; a trait object's one position is
//!   its own slot, and the slots of its traits (and a fn item's arguments) are no position of
//!   the lowering's, so [`field_row`] steps over them by the same counter. A column that does
//!   not line up with the walk of the field's erased type is refused by name, never indexed.
//! - [`copy_args_of`]: which type arguments the ADT's one `Copy` impl needs to be `Copy`
//!   (`Facts::copy_args`), read from the impl's own clauses: `T: Copy` asks it; `T: Sized`
//!   asks nothing the ADT's own well-formedness does not (when the ADT bounds that parameter by
//!   `Sized` too); `T: Clone` beside `T: Copy` asks nothing more (`Copy: Clone`). Any other
//!   clause, a self type that is not the ADT over distinct bare parameters, or not exactly one
//!   impl: no row (`None`: every argument must be `Copy`, the derive's rule).
//!
//! An unknown fact is `None`, never a guess (rule 3); what each answer was read from is in the
//! ADT's frozen head and the reads made while freezing it (rule 21).

use blaze_flow_lower::FieldRegion;
use blaze_lower::{View, form as lf, head};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;
use blaze_typeck::region_facts::RegionFacts;
use blaze_typeck::regions::{Arity, Region, RegionError, Unknown, region_arity};

use crate::front2::borrow_reads::{
    drop_adt_read, field_alias_blanket_read, field_alias_super_read, inst_drop_adt_read, inst_drop_impl_read,
    inst_drop_trait_read,
};
use crate::front2::buckets::shape_of;
use crate::front2::check::{Records, answer_impls};
use crate::front2::heads::{Store, Want};
use crate::front2::keys::{Keys, checker_of};
use crate::front2::reads::Read;
use crate::front2::source::NamesSource;
use crate::front2::tables::view::BodyHeadFacts;
use crate::front2::types::{HeadsArity, Why};

/// A field with no row in `at` (its regions not known).
pub const NO_ROW: (u32, u32) = (u32::MAX, 0);

/// The most nodes one field's type may have walked (rule 18): past it, no row.
const MOST_NODES: u32 = 4096;

/// The declaration-order count is independent of whether the richer field lifetime analysis
/// succeeds. Recursive references use this fact as soon as the ADT's generics have been made.
pub(crate) fn declared_lifetimes(v: &View<'_>) -> Result<u32, Why> {
    let mut count = 0;
    for &row in v.rows(v.list(head::GENERICS))? {
        count += u32::from(matches!(lf::GenericParam::decode(row)?, lf::GenericParam::Lifetime { .. }));
    }
    Ok(count)
}

/// Build the region rows alongside the erased fields from the very same lowered declaration,
/// each from the field's column in the head of `adt` (its checker key: `RegionFacts::field`,
/// filed by heads.rs from `adt_regions` before this is called). Flatten variant fields in
/// declaration order. A known empty field and an unread field have distinct entries,
/// including after serialization. A field whose column is not filed (the ADT's regions were
/// refused or not made) or whose type has a position no [`FieldRegion`] names has no row; a
/// column that does not line up with its field's type is refused by name ([`Unlaid::Shape`]).
#[expect(clippy::too_many_arguments, reason = "the head's names, keys, store, checker key, lowered syntax, kind, erased fields and allocator")]
pub(crate) fn freeze_field_regions<'n, 'a, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    store: &Store<'n>,
    adt: tc::ItemKey,
    v: &View<'_>,
    kind: tc::AdtKind,
    fields: &[tc::Field],
    alloc: Alloc<'a>,
) -> Result<(u32, AVec<'a, (u32, u32)>, AVec<'a, FieldRegion>), Why> {
    let lifetimes = declared_lifetimes(v)?;
    // Each field's (variant, field within it, written type), in the erased fields' order: the
    // key its column is filed under (heads_regions.rs `FieldAt`; a struct's and a union's
    // fields are variant 0's).
    let mut written: AVec<'a, (u32, u32, lf::TypeId)> = AVec::new_in(alloc);
    let rows = v.rows(v.list(head::FIELDS))?;
    let key = |i: usize| u32::try_from(i).map_err(|_| Why::Unsupported("the ADT has more fields than a region column is keyed by", blaze_surface::Range::new(0, v.item().len() as u32)));
    if kind == tc::AdtKind::Enum {
        for (vi, &row) in rows.iter().enumerate() {
            for (fi, &f) in v.rows(lf::Variant::decode(row)?.fields)?.iter().enumerate() {
                written.push((key(vi)?, key(fi)?, lf::Field::decode(f)?.ty));
            }
        }
    } else {
        for (fi, &f) in rows.iter().enumerate() {
            written.push((0, key(fi)?, lf::Field::decode(f)?.ty));
        }
    }
    if written.len() != fields.len() {
        return Err(Why::Unsupported("the ADT's erased fields differ in count from its declared fields", blaze_surface::Range::new(0, v.item().len() as u32)));
    }
    let tys = store.tys.view();
    // The one region counter's facts: each declared count the names' (D155), what the column
    // was laid by in every store.
    let arity = HeadsArity { heads: &store.maps, opaques: &[], names };
    let reader = store.regions.reader(store.work_alloc());
    let mut at = AVec::with_capacity_in(fields.len(), alloc);
    let mut list = AVec::new_in(alloc);
    for (field, &(variant, index, written_ty)) in fields.iter().zip(written.iter()) {
        let start = list.len();
        let Ok(col) = reader.field(adt, variant, index) else {
            at.push(NO_ROW);
            continue;
        };
        if col.ty() != field.ty {
            return Err(Why::Unsupported("a field's region column is not laid over the field's erased type", v.site(v.type_range(written_ty)?)?));
        }
        match field_row(tys, field.ty, &arity, col.as_slice(), lifetimes, &mut list, alloc) {
            Ok(()) => {
                if list.len() >= u32::MAX as usize {
                    return Err(Why::Unsupported("the ADT has too many field region positions for its frozen head", v.site(v.type_range(written_ty)?)?));
                }
                at.push((start as u32, (list.len() - start) as u32));
            }
            Err(Unlaid::Unnamed) => {
                list.truncate(start);
                at.push(NO_ROW);
            }
            // A head the counter needs and the store has not made: ask for it (the head is
            // made again after it), as for any head a head names.
            Err(Unlaid::Count(RegionError::NotKnown(n))) => {
                let item = match n.what {
                    Unknown::AdtLifetimes(k) | Unknown::OpaqueCaptures(k) | Unknown::TraitLifetimes(k) => k,
                    Unknown::AliasLifetimes { trait_, .. } => trait_,
                };
                let lib = keys.library_in(names, item).ok_or_else(|| Why::Unsupported(
                    "an item a field's type names has no library key for its lifetime count",
                    blaze_surface::Range::new(0, v.item().len() as u32),
                ))?;
                return Err(Why::Need(lib));
            }
            Err(Unlaid::Count(_)) => {
                return Err(Why::Unsupported("a field's type could not be counted by the region counter", v.site(v.type_range(written_ty)?)?));
            }
            Err(Unlaid::Shape(what)) => return Err(Why::Unsupported(what, v.site(v.type_range(written_ty)?)?)),
        }
    }
    Ok((lifetimes, at, list))
}

/// Why [`field_row`] gave no row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unlaid {
    /// The type has a position no [`FieldRegion`] names (a projection, an opaque type, a body
    /// type, an inference or error node), or passed [`MOST_NODES`]: no row.
    Unnamed,
    /// The region counter could not count a node (a fact not filed, the table unreadable).
    Count(RegionError),
    /// The column does not line up with the type (its length, or a lifetime the ADT does not
    /// declare): a broken head, refused by name.
    Shape(&'static str),
}

impl From<tc::regions::NotKnown> for Unlaid {
    fn from(n: tc::regions::NotKnown) -> Unlaid {
        Unlaid::Count(RegionError::NotKnown(n))
    }
}

/// The next slot of the column as the field's region: `'static`; the ADT's lifetime `i` (of
/// `lifetimes`); a lifetime bound by a fn pointer of the field's type, numbered within the
/// field by first appearance of its `(binder, var)` (`late`), so two binders' variables are
/// two existentials.
fn take<'a>(col: &[Region], taken: &mut usize, lifetimes: u32, late: &mut AVec<'a, (u32, u32)>, out: &mut AVec<'a, FieldRegion>) -> Result<(), Unlaid> {
    let &r = col.get(*taken).ok_or(Unlaid::Shape("a field's region column has fewer slots than its type has positions"))?;
    *taken += 1;
    out.push(match r {
        Region::Static => FieldRegion::Static,
        Region::Param(i) if i < lifetimes => FieldRegion::Lifetime(i),
        Region::Param(_) => return Err(Unlaid::Shape("a field's region column names a lifetime parameter its ADT does not declare")),
        Region::Late { binder, var } => {
            let n = match late.iter().position(|&x| x == (binder, var)) {
                Some(n) => n,
                None => {
                    late.push((binder, var));
                    late.len() - 1
                }
            };
            FieldRegion::Bound(u32::try_from(n).map_err(|_| Unlaid::Unnamed)?)
        }
    });
    Ok(())
}

/// Walks the field's erased type `root` in the lowering's position order (pre-order, a node's
/// own positions before its children's, children left to right; an ADT's lifetimes before its
/// arguments), the order the column `col` is laid in: each node's own slots (the one counter,
/// `region_arity`) become its positions' regions, each type parameter `p` pushes `Arg(p)`,
/// onto `out`. A trait object's one position is its own slot; its traits' slots and a fn
/// item's arguments' are no position of the lowering's and are stepped over by the counter
/// (`slots`). Every slot must be taken, none past the column.
fn field_row<'a, F: Arity + ?Sized>(tys: tc::PTys<'_>, root: tc::PTy, arity: &F, col: &[Region], lifetimes: u32, out: &mut AVec<'a, FieldRegion>, alloc: Alloc<'a>) -> Result<(), Unlaid> {
    let mut stack: AVec<'a, tc::PTy> = AVec::new_in(alloc);
    let mut late: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
    stack.push(root);
    let (mut taken, mut steps) = (0usize, 0u32);
    let table = |e| Unlaid::Count(RegionError::Table(e));
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return Err(Unlaid::Unnamed);
        }
        let node = tys.node(t).map_err(table)?;
        // Nodes whose positions no `FieldRegion` names, before the counter is asked of them.
        // `PNode::Alias` IS laid in pre-order (matching `RegionSlots` for unnormalized aliases
        // such as `Hit::root: &'b K::Root` and `Lazy::started: Option<Started<'a, F::Start, ...>>`),
        // while `FieldAliasGuard` in `borrows.rs` suppresses any field whose alias may normalize
        // in a specific body (preventing both `FieldRegionsUnaligned` and unsound slot remapping).
        if matches!(
            node,
            tc::PNode::Opaque(..)
                | tc::PNode::Closure { .. }
                | tc::PNode::Future(..)
                | tc::PNode::Coroutine { .. }
                | tc::PNode::Infer
                | tc::PNode::Error
        ) {
            return Err(Unlaid::Unnamed);
        }
        for _ in 0..region_arity(t, node, arity)? {
            take(col, &mut taken, lifetimes, &mut late, out)?;
        }
        match node {
            tc::PNode::Ref(_, inner) | tc::PNode::Ptr(_, inner) | tc::PNode::Slice(inner) | tc::PNode::Pat(inner, _) | tc::PNode::Array(inner, _) => stack.push(inner),
            // Pre-order child walk matching `RegionCol` / `RegionSlots` (`blaze_borrow::Columns`).
            tc::PNode::Tuple(l)
            | tc::PNode::Record(_, l)
            | tc::PNode::Union(l)
            | tc::PNode::FnPtr(l, _)
            | tc::PNode::Adt(_, l)
            | tc::PNode::Alias { args: l, .. }
            | tc::PNode::Dynamic(l)
            | tc::PNode::FnDef(_, l)
            | tc::PNode::ExTrait(_, l) => {
                for &a in tys.list(l).map_err(table)?.iter().rev() {
                    stack.push(a);
                }
            }
            tc::PNode::ExProj { args, term, .. } => {
                stack.push(term);
                for &a in tys.list(args).map_err(table)?.iter().rev() {
                    stack.push(a);
                }
            }
            tc::PNode::Param(p) => out.push(FieldRegion::Arg(p)),
            tc::PNode::Bool
            | tc::PNode::Char
            | tc::PNode::Int(_)
            | tc::PNode::Uint(_)
            | tc::PNode::Float(_)
            | tc::PNode::Str
            | tc::PNode::Never
            | tc::PNode::ConstValue { .. }
            | tc::PNode::ConstParam(_)
            | tc::PNode::ExAuto(_)
            | tc::PNode::Opaque(..)
            | tc::PNode::Closure { .. }
            | tc::PNode::Future(..)
            | tc::PNode::Coroutine { .. }
            | tc::PNode::Infer
            | tc::PNode::Error
            // An extern type has no generics, so no region position.
            | tc::PNode::Foreign(_) => {}
        }
    }
    if taken != col.len() {
        return Err(Unlaid::Shape("a field's region column has more slots than its type has positions"));
    }
    Ok(())
}

/// Which of `adt`'s arguments its one `Copy` impl needs to be `Copy`, pushed onto `need` (one
/// per argument of the record's list); `Some((start, len))` into `need`, `None` when not read
/// (nothing left pushed). `copy_answered` is the selection's `Copy` answer: only a YES is read.
pub fn copy_args_of<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    langs: super::BorrowLangs,
    adt: tc::ItemKey,
    copy_answered: Option<bool>,
    need: &mut AVec<'a, bool>,
    alloc: Alloc<'a>,
) -> Option<(u32, u32)> {
    if copy_answered != Some(true) {
        return None;
    }
    let copy = langs.copy?;
    let impls = answer_impls(r, copy, tc::Shape::Adt(adt))?;
    let &[imp] = &impls[..] else { return None };
    if !r.store.impls.contains_key(&imp) {
        let lib = r.keys.library_in(r.names, imp)?;
        if !super::ensure_one(r, Want::Impl(lib)) {
            return None;
        }
    }
    let start = need.len();
    let read = read_copy_impl(&*r.store, langs, adt, imp, need, alloc);
    if read.is_none() {
        need.truncate(start);
        return None;
    }
    Some((start as u32, (need.len() - start) as u32))
}

fn read_copy_impl<'a>(store: &Store<'_>, langs: super::BorrowLangs, adt: tc::ItemKey, imp: tc::ItemKey, need: &mut AVec<'a, bool>, alloc: Alloc<'a>) -> Option<()> {
    let copy = langs.copy?;
    let tys = store.tys.view();
    let head = store.impls.get(&imp)?;
    let tc::PNode::Adt(k, l) = tys.node(head.self_ty).ok()? else { return None };
    if k != adt {
        return None;
    }
    let args = tys.list(l).ok()?;
    // Per argument: the impl's type or const parameter it is (distinct).
    let mut param_of: AVec<'a, Option<u32>> = AVec::new_in(alloc);
    let mut const_of: AVec<'a, Option<u32>> = AVec::new_in(alloc);
    for &a in args {
        match tys.node(a).ok()? {
            tc::PNode::Param(p) => {
                if param_of.contains(&Some(p)) {
                    return None;
                }
                param_of.push(Some(p));
                const_of.push(None);
            }
            tc::PNode::ConstParam(cp) => {
                param_of.push(None);
                const_of.push(Some(cp));
            }
            tc::PNode::ConstValue { .. } => {
                param_of.push(None);
                const_of.push(None);
            }
            _ => return None,
        }
    }
    let start = need.len();
    for _ in args {
        need.push(false);
    }
    let mut cloned: AVec<'a, usize> = AVec::new_in(alloc);
    let [sized, meta_sized, pointee_sized, destruct] = langs.relates_nothing;
    let sized = sized.or_else(|| store.sized.map(checker_of));
    let adt_preds = store.preds.get(&adt)?;
    // Lazily populated mapping from impl lifetime param index to ADT lifetime param index,
    // read from `imp`'s self_ty region column when `imp` has a `RegionOutlives` clause.
    let mut lt_map: Option<AVec<'a, Option<u32>>> = None;
    for c in store.preds.get(&imp)?.clauses.iter() {
        if let tc::PClause::RegionOutlives { long, short } = *c {
            let map = match &mut lt_map {
                Some(m) => m,
                slot @ None => {
                    let reader = store.regions.reader(store.work_alloc());
                    let adt_lts = usize::try_from(store.adts.get(&adt)?.lifetimes).ok()?;
                    let (self_col, _) = reader.impl_header(imp).ok()?;
                    let self_slice = self_col.as_slice();
                    if self_slice.len() < adt_lts {
                        return None;
                    }
                    let mut m: AVec<'a, Option<u32>> = AVec::new_in(alloc);
                    for (j, &r) in self_slice.get(..adt_lts)?.iter().enumerate() {
                        if let Region::Param(imp_p) = r {
                            let pi = usize::try_from(imp_p).ok()?;
                            if m.len() <= pi {
                                m.resize(pi + 1, None);
                            }
                            if let Some(entry) = m.get_mut(pi) {
                                if entry.is_some() {
                                    // Same impl lifetime bound twice on the ADT: cannot invert uniquely.
                                    return None;
                                }
                                *entry = u32::try_from(j).ok();
                            }
                        }
                    }
                    slot.insert(m)
                }
            };
            let map_reg = |r: Region| -> Option<Region> {
                match r {
                    Region::Static => Some(Region::Static),
                    Region::Param(p) => usize::try_from(p).ok().and_then(|pi| map.get(pi).copied().flatten()).map(Region::Param),
                    Region::Late { .. } => None,
                }
            };
            let adt_long = map_reg(long)?;
            let adt_short = map_reg(short)?;
            if adt_long == Region::Static
                || adt_long == adt_short
                || adt_preds.clauses.iter().any(|ac| {
                    matches!(*ac, tc::PClause::RegionOutlives { long: al, short: as_ } if al == adt_long && as_ == adt_short)
                })
            {
                continue;
            }
            return None;
        }
        if let tc::PClause::TypeOutlives { ty, region } = *c {
            // A `T: 'r` clause already required by the ADT's own predicates (e.g.
            // `struct ReflectVtable<T: 'static, A>` with `impl<T: 'static, A> Copy`) is
            // implied by well-formedness of any value of the ADT.
            let implied = tys
                .node(ty)
                .ok()
                .and_then(|n| match n {
                    tc::PNode::Param(p) => param_of.iter().position(|&q| q == Some(p)),
                    _ => None,
                })
                .is_some_and(|j| {
                    adt_preds.clauses.iter().any(|ac| {
                        matches!(
                            *ac,
                            tc::PClause::TypeOutlives { ty: aty, region: ar }
                                if ar == region && matches!(tys.node(aty), Ok(tc::PNode::Param(q)) if q as usize == j)
                        )
                    })
                });
            if implied {
                continue;
            }
            return None;
        }
        let tc::PClause::Trait { trait_, args: cargs } = *c else { return None };
        // `PointeeSized` and `Destruct` hold for every type and ask nothing of the arguments.
        if Some(trait_) == pointee_sized || Some(trait_) == destruct {
            continue;
        }
        // Any trait clause already required by the ADT's own predicates (with impl params mapped
        // to the ADT's) is implied by well-formedness of any value of the ADT.
        if trait_clause_in_preds(tys, trait_, cargs, &param_of, &const_of, &adt_preds.clauses) {
            continue;
        }
        let &[self_ty] = tys.list(cargs).ok()? else { return None };
        let tc::PNode::Param(p) = tys.node(self_ty).ok()? else { return None };
        let j = param_of.iter().position(|&q| q == Some(p))?;
        if trait_ == copy {
            need[start + j] = true;
        } else if Some(trait_) == meta_sized {
            // `Sized: MetaSized`: if the ADT bounds param `j` by `Sized`, `MetaSized` holds.
            let bounded = sized.is_some_and(|s| adt_param_has_trait(tys, &adt_preds.clauses, s, j));
            if !bounded {
                return None;
            }
        } else if langs.clone == Some(trait_) {
            cloned.push(j);
        } else {
            return None;
        }
    }
    // `T: Clone` asks nothing more only beside `T: Copy`.
    if cloned.iter().any(|&j| !need[start + j]) {
        return None;
    }
    Some(())
}

fn adt_param_has_trait(tys: tc::PTys<'_>, clauses: &[tc::PClause], want_trait: tc::ItemKey, j: usize) -> bool {
    let Ok(j_u32) = u32::try_from(j) else { return false };
    clauses.iter().any(|ac| match *ac {
        tc::PClause::Trait { trait_, args } if trait_ == want_trait => match tys.list(args) {
            Ok(&[t]) => matches!(tys.node(t), Ok(tc::PNode::Param(q)) if q == j_u32),
            _ => false,
        },
        _ => false,
    })
}

fn trait_clause_in_preds(
    tys: tc::PTys<'_>,
    want_trait: tc::ItemKey,
    want_args: tc::List,
    param_of: &[Option<u32>],
    const_of: &[Option<u32>],
    adt_clauses: &[tc::PClause],
) -> bool {
    let Ok(w_list) = tys.list(want_args) else { return false };
    for ac in adt_clauses {
        let tc::PClause::Trait { trait_, args } = *ac else { continue };
        if trait_ != want_trait {
            continue;
        }
        let Ok(a_list) = tys.list(args) else { continue };
        if w_list.len() != a_list.len() {
            continue;
        }
        let mut budget = 64u32;
        if w_list.iter().zip(a_list.iter()).all(|(&w, &a)| mapped_pty_eq(tys, w, a, param_of, const_of, &mut budget)) {
            return true;
        }
    }
    false
}

fn mapped_pty_eq(
    tys: tc::PTys<'_>,
    imp_ty: tc::PTy,
    adt_ty: tc::PTy,
    param_of: &[Option<u32>],
    const_of: &[Option<u32>],
    budget: &mut u32,
) -> bool {
    let mut stack = [(tc::PTy(0), tc::PTy(0)); 64];
    let mut top = 1usize;
    if let Some(first) = stack.first_mut() {
        *first = (imp_ty, adt_ty);
    }
    let push = |stack: &mut [(tc::PTy, tc::PTy); 64], top: &mut usize, pair: (tc::PTy, tc::PTy)| -> bool {
        let Some(slot) = stack.get_mut(*top) else { return false };
        *slot = pair;
        *top += 1;
        true
    };
    while top > 0 {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        top -= 1;
        let Some(&(it, at)) = stack.get(top) else { return false };
        let (Ok(in_), Ok(an)) = (tys.node(it), tys.node(at)) else { return false };
        match (in_, an) {
            (tc::PNode::Param(p), tc::PNode::Param(q)) => {
                if usize::try_from(q).ok().and_then(|qi| param_of.get(qi)).copied() != Some(Some(p)) {
                    return false;
                }
            }
            (tc::PNode::ConstParam(p), tc::PNode::ConstParam(q)) => {
                if usize::try_from(q).ok().and_then(|qi| const_of.get(qi)).copied() != Some(Some(p)) {
                    return false;
                }
            }
            (tc::PNode::Adt(k1, l1), tc::PNode::Adt(k2, l2)) if k1 == k2 => {
                let (Ok(a), Ok(b)) = (tys.list(l1), tys.list(l2)) else { return false };
                if a.len() != b.len() {
                    return false;
                }
                for (&x, &y) in a.iter().zip(b.iter()) {
                    if !push(&mut stack, &mut top, (x, y)) {
                        return false;
                    }
                }
            }
            (tc::PNode::Tuple(l1), tc::PNode::Tuple(l2)) => {
                let (Ok(a), Ok(b)) = (tys.list(l1), tys.list(l2)) else { return false };
                if a.len() != b.len() {
                    return false;
                }
                for (&x, &y) in a.iter().zip(b.iter()) {
                    if !push(&mut stack, &mut top, (x, y)) {
                        return false;
                    }
                }
            }
            (tc::PNode::Ref(m1, i1), tc::PNode::Ref(m2, i2)) if m1 == m2 => {
                if !push(&mut stack, &mut top, (i1, i2)) {
                    return false;
                }
            }
            (tc::PNode::Ptr(m1, i1), tc::PNode::Ptr(m2, i2)) if m1 == m2 => {
                if !push(&mut stack, &mut top, (i1, i2)) {
                    return false;
                }
            }
            (tc::PNode::Slice(i1), tc::PNode::Slice(i2)) => {
                if !push(&mut stack, &mut top, (i1, i2)) {
                    return false;
                }
            }
            (tc::PNode::Array(i1, c1), tc::PNode::Array(i2, c2)) => {
                if !push(&mut stack, &mut top, (c1, c2)) || !push(&mut stack, &mut top, (i1, i2)) {
                    return false;
                }
            }
            (a, b)
                if a == b
                    && !matches!(
                        a,
                        tc::PNode::Param(_)
                            | tc::PNode::ConstParam(_)
                            | tc::PNode::Adt(..)
                            | tc::PNode::Tuple(_)
                            | tc::PNode::Ref(..)
                            | tc::PNode::Ptr(..)
                            | tc::PNode::Slice(_)
                            | tc::PNode::Array(..)
                    ) => {}
            _ => return false,
        }
    }
    true
}

/// The most ADTs one structural `needs_drop` walk visits (rule 18): past it, the root is taken
/// as unconditionally needing drop (conservative, never unsound).
const MOST_DROP_ADTS: usize = 256;
const MOST_DROP_NODES: u32 = 4096;

/// Whether `adt` implements `Copy` unconditionally (for every well-formed instantiation, so
/// `Copy` implies no `Drop` impl and no dropping fields, rustc E0184/E0204).
fn is_uncond_copy<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    langs: super::BorrowLangs,
    adt: tc::ItemKey,
    copy_answered: Option<bool>,
    scratch: &mut AVec<'a, bool>,
    alloc: Alloc<'a>,
) -> bool {
    scratch.clear();
    let Some((start, len)) = copy_args_of(r, langs, adt, copy_answered, scratch, alloc) else {
        return false;
    };
    let s = start as usize;
    let e = s.saturating_add(len as usize);
    scratch.get(s..e).is_some_and(|need| need.iter().all(|&b| !b))
}

/// Structural drop summary for an ADT ([`drop_facts_of`]).
pub(super) struct DropFactsSummary {
    pub(super) needs_drop: bool,
    pub(super) drop_args: Option<(u32, u32)>,
    pub(super) drop_strict: Option<(u32, u32)>,
    pub(super) drop_lifetimes: Option<(u32, u32)>,
}

fn all_param_bits(generics: u32) -> u64 {
    if generics >= 64 { u64::MAX } else { (1u64 << generics) - 1 }
}

/// Bitmask of `adt`'s generic arguments (`0..generics`) that are NOT marked `#[may_dangle]` on
/// `adt`'s unique `Drop` impl (RFC 1327): bit `j` set means dropping `adt` may inspect arbitrary
/// lifetimes inside argument `j`. Conservative `all_param_bits(generics)` when unread.
fn drop_impl_strict_mask<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    langs: super::BorrowLangs,
    adt: tc::ItemKey,
    generics: u32,
) -> u64 {
    let all = all_param_bits(generics);
    if generics == 0 || generics > 64 {
        return all;
    }
    let Some(drop_tr) = langs.drop else { return all };
    let Some(impls) = answer_impls(r, drop_tr, tc::Shape::Adt(adt)) else { return all };
    let &[imp] = &impls[..] else { return all };
    if !r.store.impls.contains_key(&imp) {
        let Some(lib) = r.keys.library_in(r.names, imp) else { return all };
        if !super::ensure_one(r, Want::Impl(lib)) {
            return all;
        }
    }
    let Some(lib) = r.keys.library_in(r.names, imp) else { return all };
    let Some(item) = r.names.signature_text_of(lib) else { return all };
    if !item.text.contains("may_dangle") {
        return all;
    }
    let Some(krate) = r.names.crate_of(lib) else { return all };
    let edition = r.names.edition_of(krate).unwrap_or(blaze_surface::Edition::E2024);
    let Ok(md_mask) = blaze_file_items::impl_may_dangle_params(item.text, edition) else {
        return all;
    };
    if md_mask == 0 {
        return all;
    }
    let tys = r.store.tys.view();
    let Some(head) = r.store.impls.get(&imp) else { return all };
    let Ok(tc::PNode::Adt(k, l)) = tys.node(head.self_ty) else { return all };
    if k != adt {
        return all;
    }
    let Ok(args) = tys.list(l) else { return all };
    let mut strict = all;
    for (j, &a) in args.iter().enumerate() {
        if j < 64
            && let Ok(tc::PNode::Param(p)) = tys.node(a)
            && p < 64
            && (md_mask & (1u64 << p)) != 0
        {
            strict &= !(1u64 << j);
        }
    }
    strict
}

/// Collects every `PNode::Param(p)` appearing anywhere inside `root` (even behind references or
/// pointers) into `strict_params` (RFC 1327: a strict `Drop` impl can inspect any lifetime inside
/// its non-`#[may_dangle]` argument).
fn collect_strict_params(
    tys: tc::PTys<'_>,
    root: tc::PTy,
    generics: u32,
    strict_params: &mut u64,
    scratch: &mut AVec<'_, tc::PTy>,
) -> bool {
    scratch.clear();
    scratch.push(root);
    let mut steps = 0u32;
    while let Some(ty) = scratch.pop() {
        steps += 1;
        if steps > MOST_DROP_NODES {
            *strict_params = all_param_bits(generics);
            return false;
        }
        match tys.node(ty) {
            Ok(tc::PNode::Param(p)) => {
                if p < generics && p < 64 {
                    *strict_params |= 1u64 << p;
                }
            }
            Ok(
                tc::PNode::Ref(_, i)
                | tc::PNode::Ptr(_, i)
                | tc::PNode::Slice(i)
                | tc::PNode::Array(i, _)
                | tc::PNode::Pat(i, _),
            ) => scratch.push(i),
            Ok(
                tc::PNode::Tuple(l)
                | tc::PNode::Record(_, l)
                | tc::PNode::Union(l)
                | tc::PNode::FnPtr(l, _)
                | tc::PNode::Adt(_, l)
                | tc::PNode::Opaque(_, l)
                | tc::PNode::FnDef(_, l)
                | tc::PNode::Dynamic(l)
                | tc::PNode::ExTrait(_, l),
            ) => {
                if let Ok(es) = tys.list(l) {
                    scratch.extend_from_slice(es);
                }
            }
            Ok(tc::PNode::Alias { args, .. }) => {
                if let Ok(es) = tys.list(args) {
                    scratch.extend_from_slice(es);
                }
            }
            _ => {}
        }
    }
    true
}

/// Marks every `Region::Param(lp)` with `lp < lifetimes` appearing in `slice` into `mask`.
fn mark_region_params(slice: &[Region], lifetimes: u32, mask: &mut u64) {
    for &r in slice {
        if let Region::Param(lp) = r
            && lp < lifetimes
            && lp < 64
        {
            *mask |= 1u64 << lp;
        }
    }
}

/// Counts total region slots occupied by `root` in pre-order (matching `RegionCol`), using
/// `region_arity` per node. Returns `None` if any node cannot be counted or exceeds `MOST_DROP_NODES`.
fn subtree_region_count(tys: tc::PTys<'_>, root: tc::PTy, arity: &impl Arity, scratch: &mut AVec<'_, tc::PTy>) -> Option<usize> {
    scratch.clear();
    scratch.push(root);
    let mut total = 0usize;
    let mut steps = 0u32;
    while let Some(t) = scratch.pop() {
        steps += 1;
        if steps > MOST_DROP_NODES {
            return None;
        }
        let node = tys.node(t).ok()?;
        let own = usize::try_from(region_arity(t, node, arity).ok()?).ok()?;
        total = total.checked_add(own)?;
        match node {
            tc::PNode::Ref(_, i) | tc::PNode::Ptr(_, i) | tc::PNode::Slice(i) | tc::PNode::Pat(i, _) | tc::PNode::Array(i, _) => scratch.push(i),
            tc::PNode::Tuple(l)
            | tc::PNode::Record(_, l)
            | tc::PNode::Union(l)
            | tc::PNode::FnPtr(l, _)
            | tc::PNode::Adt(_, l)
            | tc::PNode::Alias { args: l, .. }
            | tc::PNode::Dynamic(l)
            | tc::PNode::FnDef(_, l)
            | tc::PNode::Opaque(_, l)
            | tc::PNode::ExTrait(_, l) => {
                scratch.extend_from_slice(tys.list(l).ok()?);
            }
            tc::PNode::ExProj { args, term, .. } => {
                scratch.push(term);
                scratch.extend_from_slice(tys.list(args).ok()?);
            }
            _ => {}
        }
    }
    Some(total)
}

/// Structural drop summary for `adt`: `needs_drop`, `drop_args`, `drop_strict` (RFC 1327), and
/// `drop_lifetimes` (which declared lifetime parameters of `adt` are actually observed by drop).
/// Computes a bounded fixed point over ADTs reachable from `adt`'s fields in dropping positions
/// and files a `BorrowAdt` (`DROP_ADT_DETAIL`) read row for each nested ADT inspected (rule 21).
#[expect(clippy::too_many_arguments, reason = "the ADT's already-queried Copy/Drop facts and output buffers")]
pub(super) fn drop_facts_of<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    langs: super::BorrowLangs,
    adt: tc::ItemKey,
    adt_drop_impl: Option<bool>,
    adt_uncond_copy: bool,
    need: &mut AVec<'a, bool>,
    strict_need: &mut AVec<'a, bool>,
    lt_need: &mut AVec<'a, bool>,
    reads: &mut AVec<'a, Read>,
    alloc: Alloc<'a>,
) -> DropFactsSummary {
    let is_inert = |k: tc::ItemKey| langs.manually_drop == Some(k) || langs.phantom_data == Some(k);
    if is_inert(adt) || adt_uncond_copy {
        let (generics, lifetimes) = BodyHeadFacts::new(&*r.store, r.names).adt(adt).map_or((0, 0), |row| (row.generics, row.lifetimes));
        let (needs_drop, drop_args) = push_drop_mask(generics, 0, need);
        let drop_strict = push_bool_mask(generics, 0, strict_need);
        let drop_lifetimes = if lifetimes <= 64 { push_bool_mask(lifetimes, 0, lt_need) } else { None };
        return DropFactsSummary { needs_drop, drop_args, drop_strict, drop_lifetimes };
    }
    let Some(root_row) = BodyHeadFacts::new(&*r.store, r.names).adt(adt) else {
        return DropFactsSummary { needs_drop: true, drop_args: None, drop_strict: None, drop_lifetimes: None };
    };
    let root_generics = root_row.generics;
    let root_lifetimes = root_row.lifetimes;
    if adt_drop_impl == Some(true) {
        let strict_mask = drop_impl_strict_mask(r, langs, adt, root_generics);
        let drop_strict = if root_generics <= 64 { push_bool_mask(root_generics, strict_mask, strict_need) } else { None };
        return DropFactsSummary { needs_drop: true, drop_args: None, drop_strict, drop_lifetimes: None };
    }
    if adt_drop_impl != Some(false) {
        return DropFactsSummary { needs_drop: true, drop_args: None, drop_strict: None, drop_lifetimes: None };
    }
    if root_row.kind == tc::AdtKind::Union {
        let (needs_drop, drop_args) = push_drop_mask(root_generics, 0, need);
        let drop_strict = push_bool_mask(root_generics, 0, strict_need);
        let drop_lifetimes = if root_lifetimes <= 64 { push_bool_mask(root_lifetimes, 0, lt_need) } else { None };
        return DropFactsSummary { needs_drop, drop_args, drop_strict, drop_lifetimes };
    }
    if root_generics > 64 {
        return DropFactsSummary { needs_drop: true, drop_args: None, drop_strict: None, drop_lifetimes: None };
    }

    // Discover reachable ADTs in dropping positions (BFS, bounded by MOST_DROP_ADTS).
    struct DropNode {
        key: tc::ItemKey,
        fixed: Option<(bool, u64)>,
        generics: u32,
        lifetimes: u32,
        uncond: bool,
        params: u64,
        has_drop_impl: bool,
        strict_params: u64,
        drop_lts: u64,
    }
    let mut nodes: AVec<'a, DropNode> = AVec::new_in(alloc);
    nodes.push(DropNode {
        key: adt,
        fixed: None,
        generics: root_generics,
        lifetimes: root_lifetimes,
        uncond: false,
        params: 0,
        has_drop_impl: false,
        strict_params: 0,
        drop_lts: 0,
    });
    let mut cursor = 0usize;
    let mut capped = false;
    let mut field_tys: AVec<'a, (tc::PTy, u32, u32)> = AVec::new_in(alloc);
    let mut field_spans: AVec<'a, (usize, usize)> = AVec::new_in(alloc);
    let mut ty_stack: AVec<'a, tc::PTy> = AVec::new_in(alloc);
    let mut lt_stack: AVec<'a, (tc::PTy, usize, usize)> = AVec::new_in(alloc);
    let mut strict_stack: AVec<'a, tc::PTy> = AVec::new_in(alloc);
    let mut copy_scratch: AVec<'a, bool> = AVec::new_in(alloc);

    while let Some(key) = nodes.get(cursor).map(|n| n.key) {
        let is_root = cursor == 0;
        let inert = is_inert(key);
        if !is_root && !r.store.adts.contains_key(&key) {
            if let Some(lib) = r.keys.library_in(r.names, key) {
                super::ensure_one(r, Want::Adt(lib));
            }
        }
        let uncond_copy = if is_root {
            adt_uncond_copy
        } else if inert {
            false
        } else {
            let copy_impl = super::has_impl(r, langs.copy, key).ok();
            is_uncond_copy(r, langs, key, copy_impl, &mut copy_scratch, alloc)
        };
        let drop_impl = if is_root {
            adt_drop_impl
        } else if inert || uncond_copy || langs.drop.is_none() {
            Some(false)
        } else {
            super::has_impl(r, langs.drop, key).ok()
        };
        let (made, union, generics, lifetimes) = match BodyHeadFacts::new(&*r.store, r.names).adt(key) {
            Some(row) => (true, row.kind == tc::AdtKind::Union, row.generics, row.lifetimes),
            None => (false, false, 0, 0),
        };
        let impl_strict = if drop_impl == Some(true) {
            drop_impl_strict_mask(r, langs, key, generics)
        } else if !made || generics > 64 || drop_impl.is_none() {
            all_param_bits(generics)
        } else {
            0
        };
        let fields_start = field_tys.len();
        let mut fhash = tc::StableHasher::new("blaze-ladder front2 borrow drop fields v3");
        fhash.u64(impl_strict);
        fhash.u32(lifetimes);
        if let Some(row) = BodyHeadFacts::new(&*r.store, r.names).adt(key) {
            let tys = r.store.tys.view();
            let reader = r.store.regions.reader(r.store.work_alloc());
            fhash.u32(u32::try_from(row.fields.len()).unwrap_or(u32::MAX));
            for (vi, v) in row.variants.iter().enumerate() {
                for (fi, f) in row.fields.get(v.fields.range()).unwrap_or(&[]).iter().enumerate() {
                    let v_idx = u32::try_from(vi).unwrap_or(u32::MAX);
                    let f_idx = u32::try_from(fi).unwrap_or(u32::MAX);
                    field_tys.push((f.ty, v_idx, f_idx));
                    let mut hb = MOST_DROP_NODES;
                    hash_pty_for_drop(tys, f.ty, &mut fhash, &mut hb, &mut ty_stack);
                    if lifetimes > 0 {
                        match reader.field(key, v_idx, f_idx) {
                            Ok(col) if col.ty() == f.ty => {
                                fhash.tag(1);
                                fhash.u32(u32::try_from(col.as_slice().len()).unwrap_or(u32::MAX));
                                for &reg in col.as_slice() {
                                    match reg {
                                        Region::Static => fhash.tag(0),
                                        Region::Param(p) => {
                                            fhash.tag(1);
                                            fhash.u32(p);
                                        }
                                        Region::Late { binder, var } => {
                                            fhash.tag(2);
                                            fhash.u32(binder);
                                            fhash.u32(var);
                                        }
                                    }
                                }
                            }
                            _ => fhash.tag(0),
                        }
                    }
                }
            }
        }
        let fields_end = field_tys.len();
        field_spans.push((fields_start, fields_end));
        if !is_root {
            reads.push(drop_adt_read(key.0, made, union, inert, uncond_copy, drop_impl, generics, fhash.finish()));
        }

        let fixed = if inert || uncond_copy {
            Some((false, 0))
        } else if !made || generics > 64 || drop_impl != Some(false) {
            Some((true, 0))
        } else if union {
            Some((false, 0))
        } else {
            None
        };
        let init_drop_lts = if inert || uncond_copy || union {
            0
        } else if !made || lifetimes > 64 || drop_impl != Some(false) {
            all_param_bits(lifetimes)
        } else {
            0
        };
        if let Some(node) = nodes.get_mut(cursor) {
            node.generics = generics;
            node.lifetimes = lifetimes;
            node.fixed = fixed;
            node.has_drop_impl = drop_impl == Some(true);
            node.strict_params = impl_strict;
            node.drop_lts = init_drop_lts;
            if let Some((u, p)) = fixed {
                node.uncond = u;
                node.params = p;
            }
        }
        if fixed.is_none() {
            // Enqueue ADTs mentioned in this node's fields (stopping at Ref / Ptr).
            let tys = r.store.tys.view();
            ty_stack.clear();
            if let Some(slice) = field_tys.get(fields_start..fields_end) {
                for &(fty, _, _) in slice {
                    ty_stack.push(fty);
                }
            }
            let mut steps = 0u32;
            while let Some(t) = ty_stack.pop() {
                steps += 1;
                if steps > MOST_DROP_NODES {
                    capped = true;
                    break;
                }
                match tys.node(t) {
                    Ok(tc::PNode::Tuple(l)) => {
                        if let Ok(es) = tys.list(l) {
                            ty_stack.extend_from_slice(es);
                        }
                    }
                    Ok(tc::PNode::Array(elem, _) | tc::PNode::Pat(elem, _)) => ty_stack.push(elem),
                    Ok(tc::PNode::Adt(nested, args)) => {
                        if !nodes.iter().any(|n| n.key == nested) {
                            if nodes.len() >= MOST_DROP_ADTS {
                                capped = true;
                                break;
                            }
                            nodes.push(DropNode {
                                key: nested,
                                fixed: None,
                                generics: 0,
                                lifetimes: 0,
                                uncond: false,
                                params: 0,
                                has_drop_impl: false,
                                strict_params: 0,
                                drop_lts: 0,
                            });
                        }
                        if let Ok(es) = tys.list(args) {
                            ty_stack.extend_from_slice(es);
                        }
                    }
                    _ => {}
                }
            }
        }
        cursor += 1;
    }

    if capped {
        return DropFactsSummary { needs_drop: true, drop_args: None, drop_strict: None, drop_lifetimes: None };
    }

    // Monotone fixed point over `nodes`: each pass only sets `uncond = true` or sets bits in
    // `params` / `strict_params` / `drop_lts`.
    let tys = r.store.tys.view();
    let arity = HeadsArity { heads: &r.store.maps, opaques: &[], names: r.names };
    let reader = r.store.regions.reader(r.store.work_alloc());
    let max_iters = nodes.len().saturating_add(2);
    for _ in 0..max_iters {
        let mut changed = false;
        for idx in 0..nodes.len() {
            let Some((node_key, fixed, generics, lifetimes, old_uncond, old_params, old_strict, old_lts)) =
                nodes.get(idx).map(|n| (n.key, n.fixed, n.generics, n.lifetimes, n.uncond, n.params, n.strict_params, n.drop_lts))
            else {
                continue;
            };
            if fixed.is_some() {
                continue;
            }
            let Some(&(fs, fe)) = field_spans.get(idx) else { continue };
            let mut uncond = old_uncond;
            let mut params = old_params;
            let mut strict_params = old_strict;
            let mut drop_lts = old_lts;
            ty_stack.clear();
            if let Some(slice) = field_tys.get(fs..fe) {
                for &(fty, _, _) in slice {
                    ty_stack.push(fty);
                }
            }
            let mut steps = 0u32;
            while let Some(t) = ty_stack.pop() {
                steps += 1;
                if steps > MOST_DROP_NODES {
                    uncond = true;
                    strict_params = all_param_bits(generics);
                    break;
                }
                match tys.node(t) {
                    Ok(
                        tc::PNode::Bool
                        | tc::PNode::Char
                        | tc::PNode::Int(_)
                        | tc::PNode::Uint(_)
                        | tc::PNode::Float(_)
                        | tc::PNode::Never
                        | tc::PNode::Ptr(..)
                        | tc::PNode::Ref(..)
                        | tc::PNode::FnDef(..)
                        | tc::PNode::FnPtr(..)
                        | tc::PNode::ConstValue { .. }
                        | tc::PNode::ConstParam(_),
                    ) => {}
                    Ok(tc::PNode::Param(p)) => {
                        if p < generics && p < 64 {
                            params |= 1u64 << p;
                        } else {
                            uncond = true;
                        }
                    }
                    Ok(tc::PNode::Tuple(l)) => match tys.list(l) {
                        Ok(es) => ty_stack.extend_from_slice(es),
                        Err(_) => {
                            uncond = true;
                            strict_params = all_param_bits(generics);
                            break;
                        }
                    },
                    Ok(tc::PNode::Array(elem, _) | tc::PNode::Pat(elem, _)) => ty_stack.push(elem),
                    Ok(tc::PNode::Adt(nested, args)) => {
                        let Some(target) = nodes.iter().find(|n| n.key == nested) else {
                            uncond = true;
                            collect_strict_params(tys, t, generics, &mut strict_params, &mut strict_stack);
                            continue;
                        };
                        let target_uncond = target.uncond;
                        let target_params = target.params;
                        let target_strict = target.strict_params;
                        let target_has_drop = target.has_drop_impl;
                        if target_uncond {
                            uncond = true;
                        }
                        let Ok(es) = tys.list(args) else {
                            uncond = true;
                            strict_params = all_param_bits(generics);
                            break;
                        };
                        for (j, &arg) in es.iter().enumerate() {
                            let is_strict = j >= 64 || (target_strict & (1u64 << j)) != 0;
                            if is_strict {
                                collect_strict_params(tys, arg, generics, &mut strict_params, &mut strict_stack);
                            } else if target_has_drop || target_uncond || (target_params & (1u64 << j)) != 0 {
                                // `#[may_dangle]` on `Drop` or structural glue: recurse into `arg`.
                                ty_stack.push(arg);
                            }
                        }
                    }
                    _ => {
                        uncond = true;
                        collect_strict_params(tys, t, generics, &mut strict_params, &mut strict_stack);
                    }
                }
            }
            // Per-lifetime drop liveness: walk each field in lockstep with its region column,
            // stopping at non-dropping types (`&T`, `*const T`, `fn(..)`, scalars, inert args).
            if lifetimes > 0 && lifetimes <= 64 && drop_lts != all_param_bits(lifetimes) {
                for &(fty, v_idx, f_idx) in field_tys.get(fs..fe).unwrap_or(&[]) {
                    if drop_lts == all_param_bits(lifetimes) {
                        break;
                    }
                    let Ok(col) = reader.field(node_key, v_idx, f_idx) else {
                        drop_lts = all_param_bits(lifetimes);
                        break;
                    };
                    if col.ty() != fty {
                        drop_lts = all_param_bits(lifetimes);
                        break;
                    }
                    let regs = col.as_slice();
                    lt_stack.clear();
                    lt_stack.push((fty, 0, regs.len()));
                    let mut lt_steps = 0u32;
                    while let Some((t, c_start, c_len)) = lt_stack.pop() {
                        lt_steps += 1;
                        if lt_steps > MOST_DROP_NODES {
                            drop_lts = all_param_bits(lifetimes);
                            break;
                        }
                        let Some(sub_regs) = regs.get(c_start..c_start + c_len) else {
                            drop_lts = all_param_bits(lifetimes);
                            break;
                        };
                        match tys.node(t) {
                            Ok(
                                tc::PNode::Bool
                                | tc::PNode::Char
                                | tc::PNode::Int(_)
                                | tc::PNode::Uint(_)
                                | tc::PNode::Float(_)
                                | tc::PNode::Never
                                | tc::PNode::Ptr(..)
                                | tc::PNode::Ref(..)
                                | tc::PNode::FnDef(..)
                                | tc::PNode::FnPtr(..)
                                | tc::PNode::ConstValue { .. }
                                | tc::PNode::ConstParam(_)
                                | tc::PNode::Param(_),
                            ) => {}
                            Ok(tc::PNode::Array(elem, _) | tc::PNode::Pat(elem, _)) => {
                                lt_stack.push((elem, c_start, c_len));
                            }
                            Ok(tc::PNode::Tuple(l)) => {
                                let Ok(es) = tys.list(l) else {
                                    mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                                    continue;
                                };
                                let mut cur = c_start;
                                let mut ok = true;
                                for &el in es {
                                    let Some(n) = subtree_region_count(tys, el, &arity, &mut strict_stack) else {
                                        ok = false;
                                        break;
                                    };
                                    if cur + n > c_start + c_len {
                                        ok = false;
                                        break;
                                    }
                                    lt_stack.push((el, cur, n));
                                    cur += n;
                                }
                                if !ok {
                                    mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                                }
                            }
                            Ok(tc::PNode::Adt(nested, args)) => {
                                let Some(target) = nodes.iter().find(|n| n.key == nested) else {
                                    mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                                    continue;
                                };
                                let Ok(t_lts) = usize::try_from(target.lifetimes) else {
                                    mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                                    continue;
                                };
                                if t_lts > c_len {
                                    mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                                    continue;
                                }
                                for j in 0..t_lts {
                                    if (j >= 64 || (target.drop_lts & (1u64 << j)) != 0)
                                        && let Some(&Region::Param(lp)) = regs.get(c_start + j)
                                        && lp < lifetimes
                                        && lp < 64
                                    {
                                        drop_lts |= 1u64 << lp;
                                    }
                                }
                                let Ok(es) = tys.list(args) else {
                                    mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                                    continue;
                                };
                                let mut cur = c_start + t_lts;
                                let mut ok = true;
                                for (j, &arg) in es.iter().enumerate() {
                                    let Some(n) = subtree_region_count(tys, arg, &arity, &mut strict_stack) else {
                                        ok = false;
                                        break;
                                    };
                                    if cur + n > c_start + c_len {
                                        ok = false;
                                        break;
                                    }
                                    let is_strict = j >= 64 || (target.strict_params & (1u64 << j)) != 0;
                                    if is_strict {
                                        if let Some(arg_regs) = regs.get(cur..cur + n) {
                                            mark_region_params(arg_regs, lifetimes, &mut drop_lts);
                                        }
                                    } else if target.has_drop_impl || target.uncond || (target.params & (1u64 << j)) != 0 {
                                        lt_stack.push((arg, cur, n));
                                    }
                                    cur += n;
                                }
                                if !ok {
                                    mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                                }
                            }
                            _ => {
                                mark_region_params(sub_regs, lifetimes, &mut drop_lts);
                            }
                        }
                    }
                }
            }
            if (uncond != old_uncond || params != old_params || strict_params != old_strict || drop_lts != old_lts)
                && let Some(node) = nodes.get_mut(idx)
            {
                node.uncond = uncond;
                node.params = params;
                node.strict_params = strict_params;
                node.drop_lts = drop_lts;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    match nodes.first() {
        Some(root) => {
            let (needs_drop, drop_args) = if !root.uncond {
                push_drop_mask(root_generics, root.params, need)
            } else {
                (true, None)
            };
            let drop_strict = push_bool_mask(root_generics, root.strict_params, strict_need);
            let drop_lifetimes = if root.lifetimes <= 64 { push_bool_mask(root.lifetimes, root.drop_lts, lt_need) } else { None };
            DropFactsSummary { needs_drop, drop_args, drop_strict, drop_lifetimes }
        }
        None => DropFactsSummary { needs_drop: true, drop_args: None, drop_strict: None, drop_lifetimes: None },
    }
}

fn push_bool_mask(generics: u32, mask: u64, out: &mut AVec<'_, bool>) -> Option<(u32, u32)> {
    let start = u32::try_from(out.len()).ok()?;
    for j in 0..generics {
        out.push(j < 64 && (mask & (1u64 << j)) != 0);
    }
    Some((start, generics))
}

fn push_drop_mask(generics: u32, mask: u64, need: &mut AVec<'_, bool>) -> (bool, Option<(u32, u32)>) {
    match push_bool_mask(generics, mask, need) {
        Some(span) => (false, Some(span)),
        None => (true, None),
    }
}

fn hash_pty_for_drop(tys: tc::PTys<'_>, root: tc::PTy, h: &mut tc::StableHasher, budget: &mut u32, stack: &mut AVec<'_, tc::PTy>) {
    stack.clear();
    stack.push(root);
    while let Some(ty) = stack.pop() {
        if *budget == 0 {
            h.tag(0xff);
            stack.clear();
            return;
        }
        *budget -= 1;
        match tys.node(ty) {
            Ok(tc::PNode::Bool) => h.tag(1),
            Ok(tc::PNode::Char) => h.tag(2),
            Ok(tc::PNode::Int(_)) => h.tag(3),
            Ok(tc::PNode::Uint(_)) => h.tag(4),
            Ok(tc::PNode::Float(_)) => h.tag(5),
            Ok(tc::PNode::Never) => h.tag(6),
            Ok(tc::PNode::Ptr(..)) => h.tag(7),
            Ok(tc::PNode::Ref(..)) => h.tag(8),
            Ok(tc::PNode::FnDef(..)) => h.tag(9),
            Ok(tc::PNode::FnPtr(..)) => h.tag(10),
            Ok(tc::PNode::ConstValue { .. }) => h.tag(11),
            Ok(tc::PNode::ConstParam(cp)) => {
                h.tag(12);
                h.u32(cp);
            }
            Ok(tc::PNode::Param(p)) => {
                h.tag(13);
                h.u32(p);
            }
            Ok(tc::PNode::Tuple(l)) => {
                h.tag(14);
                if let Ok(es) = tys.list(l) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::Array(elem, _)) => {
                h.tag(15);
                stack.push(elem);
            }
            Ok(tc::PNode::Pat(elem, _)) => {
                h.tag(16);
                stack.push(elem);
            }
            Ok(tc::PNode::Adt(k, args)) => {
                h.tag(17);
                h.u128(k.0);
                if let Ok(es) = tys.list(args) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            _ => h.tag(18),
        }
    }
}

/// Bounds for instantiated structural drop-glue evaluation ([`InstDropEval`], rule 18).
const MOST_INST_DROP_STEPS: u32 = 16384;
const MOST_INST_DROP_DEPTH: u32 = 32;
const MOST_INST_NORM_DEPTH: u32 = 16;
const MOST_INST_DROP_CANDIDATES: usize = 2048;

#[derive(Clone, Copy)]
struct InstAdtInfo {
    made: bool,
    is_union: bool,
    inert: bool,
    uncond_copy: bool,
    drop_impl: Option<bool>,
}

enum ImplMatch<'a> {
    Selected { imp: tc::ItemKey, subst: AVec<'a, tc::PTy>, trait_arg_count: usize },
    None,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

/// Bounded, sound instantiated drop-glue evaluator for concrete `PTy`s whose ADT summary was
/// conservatively `needs_drop && !drop_impl` (e.g. `Flatten<...>`, `Split<'_, &str>`,
/// `SplitWhitespace<'_>` whose fields hold `PNode::Alias`).
///
/// Substitutes ADT generic parameters into fields, normalizes `PNode::Alias` projections through
/// `front2`'s impl index with strict `where`-clause verification (disqualifying non-matching
/// blanket impls only when a clause is proven false), and proves `Some(false)` ONLY when every
/// dropping field has zero drop glue. Returns `Some(true)` when any field or explicit `Drop` impl
/// drops, or `None` when uncertain/generic — never unsoundly returning `Some(false)`.
pub(super) struct InstDropEval<'a> {
    tys: tc::PortableTys<'a>,
    memo: AVec<'a, (tc::PTy, Option<bool>)>,
    active: AVec<'a, tc::PTy>,
    active_goals: AVec<'a, (tc::ItemKey, tc::PTy)>,
    adt_cache: AVec<'a, (tc::ItemKey, InstAdtInfo)>,
    read_adts: AVec<'a, (u128, u64)>,
    read_impls: AVec<'a, (u128, u64)>,
    read_queries: AVec<'a, (u128, u64, u64)>,
    alloc: Alloc<'a>,
}

impl<'a> InstDropEval<'a> {
    pub(super) fn new_in(alloc: Alloc<'a>) -> InstDropEval<'a> {
        InstDropEval {
            tys: tc::PortableTys::new_in(alloc),
            memo: AVec::new_in(alloc),
            active: AVec::new_in(alloc),
            active_goals: AVec::new_in(alloc),
            adt_cache: AVec::new_in(alloc),
            read_adts: AVec::new_in(alloc),
            read_impls: AVec::new_in(alloc),
            read_queries: AVec::new_in(alloc),
            alloc,
        }
    }

    /// Evaluates whether the body type `ty` (in `body_tys`) needs drop glue.
    pub(super) fn eval_body_ty<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        body_tys: tc::PTys<'_>,
        ty: tc::PTy,
    ) -> Option<bool> {
        let mut budget = MOST_INST_DROP_STEPS;
        let root = self.copy_from_table(body_tys, ty, None, 0, &mut budget)?;
        self.eval_inst_ty(r, langs, root, 0, &mut budget)
    }

    /// Flushes deduplicated Rule 21 read rows for every ADT, impl, and trait query inspected.
    pub(super) fn push_reads(&self, reads: &mut AVec<'a, Read>) {
        for &(adt, digest) in self.read_adts.iter() {
            reads.push(inst_drop_adt_read(adt, digest));
        }
        for &(imp, digest) in self.read_impls.iter() {
            reads.push(inst_drop_impl_read(imp, digest));
        }
        if !self.read_queries.is_empty() {
            let mut sorted: AVec<'a, (u128, u64, u64)> = AVec::with_capacity_in(self.read_queries.len(), self.alloc);
            sorted.extend_from_slice(&self.read_queries);
            sorted.sort_unstable();
            sorted.dedup();
            let mut i = 0;
            while i < sorted.len() {
                let tr = sorted[i].0;
                let mut h = tc::StableHasher::new("blaze-ladder front2 inst drop trait queries v1");
                while i < sorted.len() && sorted[i].0 == tr {
                    h.u64(sorted[i].1);
                    h.u64(sorted[i].2);
                    i += 1;
                }
                reads.push(inst_drop_trait_read(tr, h.finish()));
            }
        }
    }

    fn copy_from_table(
        &mut self,
        src: tc::PTys<'_>,
        ty: tc::PTy,
        subst: Option<&[tc::PTy]>,
        depth: u32,
        budget: &mut u32,
    ) -> Option<tc::PTy> {
        if depth > MOST_INST_DROP_DEPTH || *budget == 0 {
            return None;
        }
        *budget -= 1;
        let node = src.node(ty).ok()?;
        let mapped = match node {
            tc::PNode::Param(p) => match subst {
                Some(args) => return usize::try_from(p).ok().and_then(|pi| args.get(pi).copied()),
                None => tc::PNode::Param(p),
            },
            tc::PNode::ConstParam(p) => match subst {
                Some(args) => return usize::try_from(p).ok().and_then(|pi| args.get(pi).copied()),
                None => tc::PNode::ConstParam(p),
            },
            tc::PNode::Bool
            | tc::PNode::Char
            | tc::PNode::Int(_)
            | tc::PNode::Uint(_)
            | tc::PNode::Float(_)
            | tc::PNode::Str
            | tc::PNode::Never
            | tc::PNode::Infer
            | tc::PNode::Error
            | tc::PNode::ExAuto(_)
            | tc::PNode::Foreign(_) => node,
            tc::PNode::Ref(m, i) => tc::PNode::Ref(m, self.copy_from_table(src, i, subst, depth + 1, budget)?),
            tc::PNode::Ptr(m, i) => tc::PNode::Ptr(m, self.copy_from_table(src, i, subst, depth + 1, budget)?),
            tc::PNode::Slice(i) => tc::PNode::Slice(self.copy_from_table(src, i, subst, depth + 1, budget)?),
            tc::PNode::Array(elem, len) => tc::PNode::Array(
                self.copy_from_table(src, elem, subst, depth + 1, budget)?,
                self.copy_from_table(src, len, subst, depth + 1, budget)?,
            ),
            tc::PNode::Pat(base, ranges) => tc::PNode::Pat(
                self.copy_from_table(src, base, subst, depth + 1, budget)?,
                self.copy_list_from_table(src, ranges, subst, depth + 1, budget)?,
            ),
            tc::PNode::Tuple(l) => tc::PNode::Tuple(self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::Record(s, l) => tc::PNode::Record(s, self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::Union(l) => tc::PNode::Union(self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::Adt(k, l) => tc::PNode::Adt(k, self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::FnDef(k, l) => tc::PNode::FnDef(k, self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::FnPtr(l, hdr) => tc::PNode::FnPtr(self.copy_list_from_table(src, l, subst, depth + 1, budget)?, hdr),
            tc::PNode::Closure { owner, ordinal, args } => tc::PNode::Closure {
                owner,
                ordinal,
                args: self.copy_list_from_table(src, args, subst, depth + 1, budget)?,
            },
            tc::PNode::Future(e, o, i) => tc::PNode::Future(
                e,
                self.copy_from_table(src, o, subst, depth + 1, budget)?,
                self.copy_from_table(src, i, subst, depth + 1, budget)?,
            ),
            tc::PNode::Coroutine { owner, ordinal, args } => tc::PNode::Coroutine {
                owner,
                ordinal,
                args: self.copy_list_from_table(src, args, subst, depth + 1, budget)?,
            },
            tc::PNode::Alias { trait_, name, args } => tc::PNode::Alias {
                trait_,
                name,
                args: self.copy_list_from_table(src, args, subst, depth + 1, budget)?,
            },
            tc::PNode::Opaque(k, l) => tc::PNode::Opaque(k, self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::ConstValue { ty: ct, bits } => tc::PNode::ConstValue {
                ty: self.copy_from_table(src, ct, subst, depth + 1, budget)?,
                bits,
            },
            tc::PNode::Dynamic(l) => tc::PNode::Dynamic(self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::ExTrait(k, l) => tc::PNode::ExTrait(k, self.copy_list_from_table(src, l, subst, depth + 1, budget)?),
            tc::PNode::ExProj { trait_, name, args, term } => tc::PNode::ExProj {
                trait_,
                name,
                args: self.copy_list_from_table(src, args, subst, depth + 1, budget)?,
                term: self.copy_from_table(src, term, subst, depth + 1, budget)?,
            },
        };
        self.tys.intern(mapped).ok()
    }

    fn copy_list_from_table(
        &mut self,
        src: tc::PTys<'_>,
        list: tc::List,
        subst: Option<&[tc::PTy]>,
        depth: u32,
        budget: &mut u32,
    ) -> Option<tc::List> {
        let items = src.list(list).ok()?;
        let mut out: AVec<'a, tc::PTy> = AVec::with_capacity_in(items.len(), self.alloc);
        for &it in items {
            out.push(self.copy_from_table(src, it, subst, depth, budget)?);
        }
        self.tys.intern_list(&out).ok()
    }

    fn adt_info<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        adt: tc::ItemKey,
    ) -> InstAdtInfo {
        if let Some(&(_, info)) = self.adt_cache.iter().find(|&&(k, _)| k == adt) {
            return info;
        }
        let inert = langs.manually_drop == Some(adt) || langs.phantom_data == Some(adt);
        if !r.store.adts.contains_key(&adt)
            && let Some(lib) = r.keys.library_in(r.names, adt)
        {
            super::ensure_one(r, Want::Adt(lib));
        }
        let uncond_copy = if inert {
            false
        } else {
            let mut scratch: AVec<'a, bool> = AVec::new_in(self.alloc);
            let copy_impl = super::has_impl(r, langs.copy, adt).ok();
            is_uncond_copy(r, langs, adt, copy_impl, &mut scratch, self.alloc)
        };
        let drop_impl = if inert || uncond_copy || langs.drop.is_none() {
            Some(false)
        } else {
            super::has_impl(r, langs.drop, adt).ok()
        };
        let (made, is_union, generics) = match BodyHeadFacts::new(&*r.store, r.names).adt(adt) {
            Some(row) => (true, row.kind == tc::AdtKind::Union, row.generics),
            None => (false, false, 0),
        };
        let mut h = tc::StableHasher::new("blaze-ladder front2 inst drop adt v1");
        h.tag(u8::from(made));
        h.tag(u8::from(is_union));
        h.tag(u8::from(inert));
        h.tag(u8::from(uncond_copy));
        match drop_impl {
            None => h.tag(0),
            Some(false) => h.tag(1),
            Some(true) => h.tag(2),
        }
        h.u32(generics);
        if let Some(row) = BodyHeadFacts::new(&*r.store, r.names).adt(adt) {
            let stys = r.store.tys.view();
            let mut stack: AVec<'a, tc::PTy> = AVec::new_in(self.alloc);
            h.u32(u32::try_from(row.fields.len()).unwrap_or(u32::MAX));
            for f in row.fields.iter() {
                let mut hb = MOST_DROP_NODES;
                hash_pty_structural(stys, f.ty, &mut h, &mut hb, &mut stack);
            }
        }
        let digest = h.finish();
        if !self.read_adts.iter().any(|&(k, _)| k == adt.0) {
            self.read_adts.push((adt.0, digest));
        }
        let info = InstAdtInfo { made, is_union, inert, uncond_copy, drop_impl };
        self.adt_cache.push((adt, info));
        info
    }

    fn eval_inst_ty<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        ty: tc::PTy,
        depth: u32,
        budget: &mut u32,
    ) -> Option<bool> {
        if depth > MOST_INST_DROP_DEPTH || *budget == 0 {
            return None;
        }
        *budget -= 1;
        if let Some(&(_, res)) = self.memo.iter().find(|&&(t, _)| t == ty) {
            return res;
        }
        if self.active.scan_for(&ty).is_some() {
            return None;
        }
        self.active.push(ty);
        let res = self.eval_inst_ty_inner(r, langs, ty, depth, budget);
        self.active.pop();
        if res.is_some() {
            self.memo.push((ty, res));
        }
        res
    }

    fn eval_inst_ty_inner<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        ty: tc::PTy,
        depth: u32,
        budget: &mut u32,
    ) -> Option<bool> {
        let node = self.tys.view().node(ty).ok()?;
        match node {
            tc::PNode::Bool
            | tc::PNode::Char
            | tc::PNode::Int(_)
            | tc::PNode::Uint(_)
            | tc::PNode::Float(_)
            | tc::PNode::Never
            | tc::PNode::Ptr(..)
            | tc::PNode::Ref(..)
            | tc::PNode::FnDef(..)
            | tc::PNode::FnPtr(..)
            | tc::PNode::ConstValue { .. }
            | tc::PNode::ConstParam(_) => Some(false),
            tc::PNode::Array(elem, _) | tc::PNode::Pat(elem, _) => {
                self.eval_inst_ty(r, langs, elem, depth + 1, budget)
            }
            tc::PNode::Tuple(l) | tc::PNode::Record(_, l) | tc::PNode::Union(l) => {
                let elems = self.copy_own_list(l)?;
                let mut any_unknown = false;
                for &el in elems.iter() {
                    match self.eval_inst_ty(r, langs, el, depth + 1, budget) {
                        Some(true) => return Some(true),
                        Some(false) => {}
                        None => any_unknown = true,
                    }
                }
                if any_unknown { None } else { Some(false) }
            }
            tc::PNode::Alias { .. } => {
                let norm = self.normalize_alias(r, langs, ty, 0, budget)?;
                if norm == ty {
                    return None;
                }
                self.eval_inst_ty(r, langs, norm, depth + 1, budget)
            }
            tc::PNode::Adt(adt, args_list) => {
                let info = self.adt_info(r, langs, adt);
                if info.inert || info.uncond_copy {
                    return Some(false);
                }
                if info.drop_impl == Some(true) {
                    return Some(true);
                }
                if info.drop_impl != Some(false) || !info.made {
                    return None;
                }
                if info.is_union {
                    return Some(false);
                }
                let args = self.copy_own_list(args_list)?;
                // Snapshot field types from `r.store.tys` into `self.tys` with `args` substituted
                // before recursing (which borrows `r` mutably).
                let mut field_insts: AVec<'a, Option<tc::PTy>> = AVec::new_in(self.alloc);
                {
                    let Some(row) = BodyHeadFacts::new(&*r.store, r.names).adt(adt) else {
                        return None;
                    };
                    let stys = r.store.tys.view();
                    field_insts.reserve(row.fields.len());
                    for f in row.fields.iter() {
                        field_insts.push(self.copy_from_table(stys, f.ty, Some(&args), 0, budget));
                    }
                }
                let mut any_unknown = false;
                for &fi in field_insts.iter() {
                    let Some(fty) = fi else {
                        any_unknown = true;
                        continue;
                    };
                    match self.eval_inst_ty(r, langs, fty, depth + 1, budget) {
                        Some(true) => return Some(true),
                        Some(false) => {}
                        None => any_unknown = true,
                    }
                }
                if any_unknown { None } else { Some(false) }
            }
            _ => None,
        }
    }

    fn copy_own_list(&self, list: tc::List) -> Option<AVec<'a, tc::PTy>> {
        let slice = self.tys.view().list(list).ok()?;
        let mut out = AVec::with_capacity_in(slice.len(), self.alloc);
        out.extend_from_slice(slice);
        Some(out)
    }

    /// Recursively normalizes any `PNode::Alias` anywhere inside `ty` (bounded).
    fn normalize_deep<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        ty: tc::PTy,
        norm_depth: u32,
        budget: &mut u32,
    ) -> Option<tc::PTy> {
        if norm_depth > MOST_INST_NORM_DEPTH || *budget == 0 {
            return Some(ty);
        }
        *budget -= 1;
        let node = self.tys.view().node(ty).ok()?;
        match node {
            tc::PNode::Alias { .. } => {
                match self.normalize_alias(r, langs, ty, norm_depth, budget) {
                    Some(norm) => Some(norm),
                    None => Some(ty),
                }
            }
            tc::PNode::Ref(m, i) => {
                let ni = self.normalize_deep(r, langs, i, norm_depth + 1, budget)?;
                if ni == i { Some(ty) } else { self.tys.intern(tc::PNode::Ref(m, ni)).ok() }
            }
            tc::PNode::Ptr(m, i) => {
                let ni = self.normalize_deep(r, langs, i, norm_depth + 1, budget)?;
                if ni == i { Some(ty) } else { self.tys.intern(tc::PNode::Ptr(m, ni)).ok() }
            }
            tc::PNode::Slice(i) => {
                let ni = self.normalize_deep(r, langs, i, norm_depth + 1, budget)?;
                if ni == i { Some(ty) } else { self.tys.intern(tc::PNode::Slice(ni)).ok() }
            }
            tc::PNode::Array(e, c) => {
                let ne = self.normalize_deep(r, langs, e, norm_depth + 1, budget)?;
                if ne == e { Some(ty) } else { self.tys.intern(tc::PNode::Array(ne, c)).ok() }
            }
            tc::PNode::Pat(b, ranges) => {
                let nb = self.normalize_deep(r, langs, b, norm_depth + 1, budget)?;
                if nb == b { Some(ty) } else { self.tys.intern(tc::PNode::Pat(nb, ranges)).ok() }
            }
            tc::PNode::Tuple(l) => {
                let nl = self.normalize_list(r, langs, l, norm_depth + 1, budget)?;
                if nl == l { Some(ty) } else { self.tys.intern(tc::PNode::Tuple(nl)).ok() }
            }
            tc::PNode::Record(s, l) => {
                let nl = self.normalize_list(r, langs, l, norm_depth + 1, budget)?;
                if nl == l { Some(ty) } else { self.tys.intern(tc::PNode::Record(s, nl)).ok() }
            }
            tc::PNode::Union(l) => {
                let nl = self.normalize_list(r, langs, l, norm_depth + 1, budget)?;
                if nl == l { Some(ty) } else { self.tys.intern(tc::PNode::Union(nl)).ok() }
            }
            tc::PNode::Adt(k, l) => {
                let nl = self.normalize_list(r, langs, l, norm_depth + 1, budget)?;
                if nl == l { Some(ty) } else { self.tys.intern(tc::PNode::Adt(k, nl)).ok() }
            }
            tc::PNode::FnDef(k, l) => {
                let nl = self.normalize_list(r, langs, l, norm_depth + 1, budget)?;
                if nl == l { Some(ty) } else { self.tys.intern(tc::PNode::FnDef(k, nl)).ok() }
            }
            tc::PNode::FnPtr(l, hdr) => {
                let nl = self.normalize_list(r, langs, l, norm_depth + 1, budget)?;
                if nl == l { Some(ty) } else { self.tys.intern(tc::PNode::FnPtr(nl, hdr)).ok() }
            }
            _ => Some(ty),
        }
    }

    fn normalize_list<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        list: tc::List,
        norm_depth: u32,
        budget: &mut u32,
    ) -> Option<tc::List> {
        let items = self.copy_own_list(list)?;
        let mut changed = false;
        let mut out: AVec<'a, tc::PTy> = AVec::with_capacity_in(items.len(), self.alloc);
        for &it in items.iter() {
            let nit = self.normalize_deep(r, langs, it, norm_depth, budget)?;
            if nit != it {
                changed = true;
            }
            out.push(nit);
        }
        if !changed { Some(list) } else { self.tys.intern_list(&out).ok() }
    }

    fn normalize_alias<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        ty: tc::PTy,
        norm_depth: u32,
        budget: &mut u32,
    ) -> Option<tc::PTy> {
        if norm_depth > MOST_INST_NORM_DEPTH || *budget == 0 {
            return None;
        }
        *budget -= 1;
        let tc::PNode::Alias { trait_, name, args } = self.tys.view().node(ty).ok()? else {
            return None;
        };
        let raw_args = self.copy_own_list(args)?;
        let mut norm_args: AVec<'a, tc::PTy> = AVec::with_capacity_in(raw_args.len(), self.alloc);
        for &a in raw_args.iter() {
            norm_args.push(self.normalize_deep(r, langs, a, norm_depth + 1, budget)?);
        }
        let &self_ty = norm_args.first()?;
        // Builtin `FnOnce::Output` on `FnPtr` or `FnDef`.
        if langs.fn_traits.contains(&Some(trait_)) && name == tc::Name::of("Output") {
            match self.tys.view().node(self_ty).ok()? {
                tc::PNode::FnPtr(l, _) => {
                    let list = self.tys.view().list(l).ok()?;
                    return list.last().copied();
                }
                tc::PNode::FnDef(fn_key, fn_args) => {
                    let fn_args_vec = self.copy_own_list(fn_args)?;
                    let out_ty = r.store.fn_output(fn_key).or_else(|| r.store.fns.get(&fn_key).map(|f| f.output))?;
                    let stys = r.store.tys.view();
                    let inst = self.copy_from_table(stys, out_ty, Some(&fn_args_vec), 0, budget)?;
                    return self.normalize_deep(r, langs, inst, norm_depth + 1, budget);
                }
                _ => {}
            }
        }
        let ImplMatch::Selected { imp, subst, trait_arg_count } = self.select_impl(r, langs, trait_, &norm_args, norm_depth + 1, budget) else {
            return None;
        };
        let assoc_ty = {
            let head = r.store.impls.get(&imp)?;
            head.assoc.iter().find(|&&(n, _)| n == name).map(|&(_, t)| t)?
        };
        let mut full_subst: AVec<'a, tc::PTy> = AVec::with_capacity_in(subst.len() + norm_args.len(), self.alloc);
        full_subst.extend_from_slice(&subst);
        if let Some(gat_args) = norm_args.get(1 + trait_arg_count..) {
            full_subst.extend_from_slice(gat_args);
        }
        let stys = r.store.tys.view();
        let inst = self.copy_from_table(stys, assoc_ty, Some(&full_subst), 0, budget)?;
        self.normalize_deep(r, langs, inst, norm_depth + 1, budget)
    }

    fn record_impl_read(&mut self, store: &Store<'_>, imp: tc::ItemKey) {
        if self.read_impls.iter().any(|&(k, _)| k == imp.0) {
            return;
        }
        let mut h = tc::StableHasher::new("blaze-ladder front2 inst drop impl v1");
        if let Some(head) = store.impls.get(&imp) {
            h.tag(1);
            h.u32(head.generics);
            h.tag(match head.polarity {
                tc::ImplPolarity::Positive => 0,
                tc::ImplPolarity::Negative => 1,
                tc::ImplPolarity::Reservation => 2,
            });
            let stys = store.tys.view();
            let mut stack: AVec<'a, tc::PTy> = AVec::new_in(self.alloc);
            let mut hb = MOST_DROP_NODES;
            hash_pty_structural(stys, head.self_ty, &mut h, &mut hb, &mut stack);
            if let Ok(ta) = stys.list(head.trait_args.list()) {
                h.u32(u32::try_from(ta.len()).unwrap_or(u32::MAX));
                for &t in ta {
                    let mut hb = MOST_DROP_NODES;
                    hash_pty_structural(stys, t, &mut h, &mut hb, &mut stack);
                }
            }
            h.u32(u32::try_from(head.assoc.len()).unwrap_or(u32::MAX));
            for &(name, ty) in head.assoc.iter() {
                h.u64(name.0);
                let mut hb = MOST_DROP_NODES;
                hash_pty_structural(stys, ty, &mut h, &mut hb, &mut stack);
            }
            if let Some(preds) = store.preds.get(&imp) {
                h.tag(1);
                h.u32(u32::try_from(preds.clauses.len()).unwrap_or(u32::MAX));
                for c in preds.clauses.iter() {
                    match *c {
                        tc::PClause::Trait { trait_, args } => {
                            h.tag(1);
                            h.u128(trait_.0);
                            if let Ok(al) = stys.list(args) {
                                h.u32(u32::try_from(al.len()).unwrap_or(u32::MAX));
                                for &a in al {
                                    let mut hb = MOST_DROP_NODES;
                                    hash_pty_structural(stys, a, &mut h, &mut hb, &mut stack);
                                }
                            }
                        }
                        tc::PClause::Projection { trait_, name, args, term } => {
                            h.tag(2);
                            h.u128(trait_.0);
                            h.u64(name.0);
                            if let Ok(al) = stys.list(args) {
                                h.u32(u32::try_from(al.len()).unwrap_or(u32::MAX));
                                for &a in al {
                                    let mut hb = MOST_DROP_NODES;
                                    hash_pty_structural(stys, a, &mut h, &mut hb, &mut stack);
                                }
                            }
                            let mut hb = MOST_DROP_NODES;
                            hash_pty_structural(stys, term, &mut h, &mut hb, &mut stack);
                        }
                        _ => h.tag(3),
                    }
                }
            } else {
                h.tag(0);
            }
        } else {
            h.tag(0);
        }
        self.read_impls.push((imp.0, h.finish()));
    }

    fn select_impl<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        trait_: tc::ItemKey,
        args: &[tc::PTy],
        norm_depth: u32,
        budget: &mut u32,
    ) -> ImplMatch<'a> {
        if norm_depth > MOST_INST_NORM_DEPTH || *budget == 0 {
            return ImplMatch::Unknown;
        }
        *budget -= 1;
        let Some(&self_ty) = args.first() else { return ImplMatch::Unknown };
        if self.active_goals.scan_for(&(trait_, self_ty)).is_some() {
            return ImplMatch::Unknown;
        }
        let Some(shape) = shape_of(self.tys.view(), self_ty) else {
            return ImplMatch::Unknown;
        };
        let (impls, has_gap) = if let Some(v) = answer_impls(r, trait_, shape) {
            (v, false)
        } else if let Some(got) = r.buckets.as_ref().and_then(|b| b.impls_of(trait_, shape))
            && !got.is_empty()
        {
            let mut v = blaze_own::Vec::with_capacity_in(got.len(), r.store.work_alloc());
            v.extend_from_slice(got);
            (v, true)
        } else {
            return ImplMatch::Unknown;
        };
        // Record Rule 21 digest for this `(trait_, shape) -> impls` query.
        {
            let mut sh = tc::StableHasher::new("blaze-ladder front2 inst drop shape v1");
            hash_shape(&mut sh, shape);
            let shape_digest = sh.finish();
            let mut ih = tc::StableHasher::new("blaze-ladder front2 inst drop impls v2");
            ih.tag(u8::from(has_gap));
            ih.u32(u32::try_from(impls.len()).unwrap_or(u32::MAX));
            for &imp in impls.iter() {
                ih.u128(imp.0);
            }
            let impls_digest = ih.finish();
            if !self.read_queries.iter().any(|&(t, sd, _)| t == trait_.0 && sd == shape_digest) {
                self.read_queries.push((trait_.0, shape_digest, impls_digest));
            }
        }
        if impls.len() > MOST_INST_DROP_CANDIDATES {
            return ImplMatch::Unknown;
        }
        self.active_goals.push((trait_, self_ty));
        let mut matched: Option<(tc::ItemKey, AVec<'a, tc::PTy>, usize)> = None;
        let mut saw_unknown = false;
        let after_self = &args[1..];
        for &imp in impls.iter() {
            if !r.store.impls.contains_key(&imp) {
                if let Some(lib) = r.keys.library_in(r.names, imp) {
                    if !super::ensure_one(r, Want::Impl(lib)) {
                        saw_unknown = true;
                        continue;
                    }
                } else {
                    saw_unknown = true;
                    continue;
                }
            }
            self.record_impl_read(&*r.store, imp);
            let (generics, polarity, header_ok, trait_arg_count, subst_opt) = {
                let Some(head) = r.store.impls.get(&imp) else {
                    saw_unknown = true;
                    continue;
                };
                if head.polarity != tc::ImplPolarity::Positive {
                    continue;
                }
                if head.generics > 64 {
                    saw_unknown = true;
                    continue;
                }
                let stys = r.store.tys.view();
                let Ok(head_ta) = stys.list(head.trait_args.list()) else {
                    saw_unknown = true;
                    continue;
                };
                if after_self.len() < head_ta.len() {
                    continue;
                }
                let g_usize = head.generics as usize;
                let mut subst_buf = [None; 64];
                let mut ok = match_pattern(stys, head.self_ty, self.tys.view(), self_ty, &mut subst_buf[..g_usize], 0);
                if ok {
                    for (&pat_a, &val_a) in head_ta.iter().zip(after_self.iter()) {
                        if !match_pattern(stys, pat_a, self.tys.view(), val_a, &mut subst_buf[..g_usize], 0) {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    continue;
                }
                let all_bound = subst_buf[..g_usize].iter().all(Option::is_some);
                let subst_opt = if all_bound {
                    let mut v: AVec<'a, tc::PTy> = AVec::with_capacity_in(g_usize, self.alloc);
                    for s in &subst_buf[..g_usize] {
                        if let Some(t) = *s {
                            v.push(t);
                        }
                    }
                    Some(v)
                } else {
                    None
                };
                (head.generics, head.polarity, true, head_ta.len(), subst_opt)
            };
            let _ = (generics, polarity, header_ok);
            let Some(subst) = subst_opt else {
                saw_unknown = true;
                continue;
            };
            match self.check_impl_clauses(r, langs, imp, &subst, norm_depth + 1, budget) {
                Truth::True => {
                    if matched.is_some() {
                        // Two distinct impls both proved: ambiguous, refuse conservatively.
                        self.active_goals.pop();
                        return ImplMatch::Unknown;
                    }
                    matched = Some((imp, subst, trait_arg_count));
                }
                Truth::False => {}
                Truth::Unknown => saw_unknown = true,
            }
        }
        self.active_goals.pop();
        match (matched, saw_unknown || has_gap) {
            (Some((imp, subst, trait_arg_count)), false) => ImplMatch::Selected { imp, subst, trait_arg_count },
            // By Rust coherence (E0119) + RFC 1210, a positive impl with no `default` items cannot
            // be overlapped or specialized by another impl, so an unrelated candidate gap or
            // unknown clause in a large bucket (e.g. `IntoIterator for &T`) cannot override it.
            // Conversely, `ImplMatch::None` still requires `!saw_unknown && !has_gap`.
            (Some((imp, subst, trait_arg_count)), true) if self.impl_has_no_default(r, imp) => {
                ImplMatch::Selected { imp, subst, trait_arg_count }
            }
            (None, false) => ImplMatch::None,
            _ => ImplMatch::Unknown,
        }
    }

    fn impl_has_no_default<'n, S: NamesSource<'n>>(&self, r: &Records<'_, 'n, S>, imp: tc::ItemKey) -> bool {
        let Some(lib) = r.keys.library_in(r.names, imp) else {
            return false;
        };
        let Some(item) = r.names.text_of(lib) else {
            return false;
        };
        !item.text.contains("default")
    }

    fn check_impl_clauses<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        imp: tc::ItemKey,
        subst: &[tc::PTy],
        norm_depth: u32,
        budget: &mut u32,
    ) -> Truth {
        // Snapshot instantiated clauses from `r.store.preds` before recursively querying `r`.
        enum InstClause<'a> {
            Trait(tc::ItemKey, AVec<'a, tc::PTy>),
            Proj(tc::ItemKey, tc::Name, AVec<'a, tc::PTy>, tc::PTy),
            Unknown,
        }
        let mut clauses: AVec<'a, InstClause<'a>> = AVec::new_in(self.alloc);
        {
            let Some(preds) = r.store.preds.get(&imp) else {
                return Truth::Unknown;
            };
            let stys = r.store.tys.view();
            for c in preds.clauses.iter() {
                match *c {
                    tc::PClause::TypeOutlives { .. } | tc::PClause::RegionOutlives { .. } => {}
                    tc::PClause::Trait { trait_, args } => {
                        let Ok(al) = stys.list(args) else {
                            clauses.push(InstClause::Unknown);
                            continue;
                        };
                        let mut inst_args: AVec<'a, tc::PTy> = AVec::with_capacity_in(al.len(), self.alloc);
                        let mut ok = true;
                        for &a in al {
                            match self.copy_from_table(stys, a, Some(subst), 0, budget) {
                                Some(t) => inst_args.push(t),
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            clauses.push(InstClause::Trait(trait_, inst_args));
                        } else {
                            clauses.push(InstClause::Unknown);
                        }
                    }
                    tc::PClause::Projection { trait_, name, args, term } => {
                        let Ok(al) = stys.list(args) else {
                            clauses.push(InstClause::Unknown);
                            continue;
                        };
                        let mut inst_args: AVec<'a, tc::PTy> = AVec::with_capacity_in(al.len(), self.alloc);
                        let mut ok = true;
                        for &a in al {
                            match self.copy_from_table(stys, a, Some(subst), 0, budget) {
                                Some(t) => inst_args.push(t),
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        let inst_term = self.copy_from_table(stys, term, Some(subst), 0, budget);
                        match (ok, inst_term) {
                            (true, Some(t)) => clauses.push(InstClause::Proj(trait_, name, inst_args, t)),
                            _ => clauses.push(InstClause::Unknown),
                        }
                    }
                    tc::PClause::Const { .. } | tc::PClause::ForFields { .. } => {
                        clauses.push(InstClause::Unknown);
                    }
                }
            }
        }
        let mut any_unknown = false;
        for cl in clauses.iter() {
            match cl {
                InstClause::Unknown => any_unknown = true,
                InstClause::Trait(ct, cargs) => {
                    match self.prove_trait(r, langs, *ct, cargs, norm_depth, budget) {
                        Truth::True => {}
                        Truth::False => return Truth::False,
                        Truth::Unknown => any_unknown = true,
                    }
                }
                InstClause::Proj(pt, pn, pargs, pterm) => {
                    let Ok(list) = self.tys.intern_list(pargs) else {
                        any_unknown = true;
                        continue;
                    };
                    let Ok(alias_ty) = self.tys.intern(tc::PNode::Alias { trait_: *pt, name: *pn, args: list }) else {
                        any_unknown = true;
                        continue;
                    };
                    let lhs = self.normalize_alias(r, langs, alias_ty, norm_depth, budget);
                    let rhs = self.normalize_deep(r, langs, *pterm, norm_depth, budget);
                    match (lhs, rhs) {
                        (Some(l), Some(rt)) if l == rt => {}
                        (Some(l), Some(rt))
                            if self.is_concrete_normalized(l) && self.is_concrete_normalized(rt) =>
                        {
                            return Truth::False;
                        }
                        _ => any_unknown = true,
                    }
                }
            }
        }
        if any_unknown { Truth::Unknown } else { Truth::True }
    }

    fn is_concrete_normalized(&self, ty: tc::PTy) -> bool {
        !matches!(
            self.tys.view().node(ty),
            Ok(tc::PNode::Param(_) | tc::PNode::Alias { .. } | tc::PNode::Opaque(..) | tc::PNode::Infer | tc::PNode::Error) | Err(_)
        )
    }

    fn prove_trait<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        langs: super::BorrowLangs,
        trait_: tc::ItemKey,
        raw_args: &[tc::PTy],
        norm_depth: u32,
        budget: &mut u32,
    ) -> Truth {
        if norm_depth > MOST_INST_NORM_DEPTH || *budget == 0 {
            return Truth::Unknown;
        }
        let [sized, meta_sized, pointee_sized, destruct] = langs.relates_nothing;
        let sized = sized.or_else(|| r.store.sized.map(checker_of));
        if Some(trait_) == pointee_sized || Some(trait_) == destruct {
            return Truth::True;
        }
        let mut args: AVec<'a, tc::PTy> = AVec::with_capacity_in(raw_args.len(), self.alloc);
        for &a in raw_args {
            match self.normalize_deep(r, langs, a, norm_depth + 1, budget) {
                Some(na) => args.push(na),
                None => return Truth::Unknown,
            }
        }
        let Some(&self_ty) = args.first() else { return Truth::Unknown };
        if Some(trait_) == sized || Some(trait_) == meta_sized {
            return match self.is_known_sized(r, self_ty, 0, budget) {
                Some(true) => Truth::True,
                Some(false) if Some(trait_) == sized => Truth::False,
                _ => Truth::Unknown,
            };
        }
        if Some(trait_) == langs.tuple_trait {
            return match self.tys.view().node(self_ty) {
                Ok(tc::PNode::Tuple(_)) => Truth::True,
                Ok(tc::PNode::Param(_) | tc::PNode::Alias { .. } | tc::PNode::Opaque(..) | tc::PNode::Infer | tc::PNode::Error) | Err(_) => {
                    Truth::Unknown
                }
                Ok(_) => Truth::False,
            };
        }
        if langs.fn_traits.contains(&Some(trait_)) {
            match self.tys.view().node(self_ty) {
                Ok(tc::PNode::FnPtr(l, _)) => {
                    if let (Ok(fp_elems), Some(&arg_tuple)) = (self.tys.view().list(l), args.get(1))
                        && let Ok(tc::PNode::Tuple(tl)) = self.tys.view().node(arg_tuple)
                        && let Ok(t_elems) = self.tys.view().list(tl)
                    {
                        let inputs = fp_elems.get(..fp_elems.len().saturating_sub(1)).unwrap_or(&[]);
                        return if inputs == t_elems { Truth::True } else { Truth::False };
                    }
                    return Truth::Unknown;
                }
                Ok(tc::PNode::FnDef(fn_key, fn_args)) => {
                    let Some(fn_args_vec) = self.copy_own_list(fn_args) else { return Truth::Unknown };
                    let Some(&arg_tuple) = args.get(1) else { return Truth::Unknown };
                    let Ok(tc::PNode::Tuple(tl)) = self.tys.view().node(arg_tuple) else { return Truth::Unknown };
                    let Some(t_elems) = self.copy_own_list(tl) else { return Truth::Unknown };
                    let mut fn_inputs: AVec<'a, tc::PTy> = AVec::new_in(self.alloc);
                    {
                        let Some(f) = r.store.fns.get(&fn_key) else { return Truth::Unknown };
                        let stys = r.store.tys.view();
                        for &inp in f.inputs.iter() {
                            match self.copy_from_table(stys, inp, Some(&fn_args_vec), 0, budget) {
                                Some(t) => fn_inputs.push(t),
                                None => return Truth::Unknown,
                            }
                        }
                    }
                    return if fn_inputs.as_slice() == t_elems.as_slice() { Truth::True } else { Truth::False };
                }
                _ => {}
            }
        }
        match self.select_impl(r, langs, trait_, &args, norm_depth + 1, budget) {
            ImplMatch::Selected { .. } => Truth::True,
            ImplMatch::None => Truth::False,
            ImplMatch::Unknown => Truth::Unknown,
        }
    }

    fn is_known_sized<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        ty: tc::PTy,
        depth: u32,
        budget: &mut u32,
    ) -> Option<bool> {
        if depth > MOST_INST_NORM_DEPTH || *budget == 0 {
            return None;
        }
        *budget -= 1;
        match self.tys.view().node(ty).ok()? {
            tc::PNode::Bool
            | tc::PNode::Char
            | tc::PNode::Int(_)
            | tc::PNode::Uint(_)
            | tc::PNode::Float(_)
            | tc::PNode::Never
            | tc::PNode::Ref(..)
            | tc::PNode::Ptr(..)
            | tc::PNode::Array(..)
            | tc::PNode::FnPtr(..)
            | tc::PNode::FnDef(..)
            | tc::PNode::ConstValue { .. }
            | tc::PNode::ConstParam(_) => Some(true),
            tc::PNode::Str | tc::PNode::Slice(_) | tc::PNode::Dynamic(_) | tc::PNode::Foreign(_) => Some(false),
            tc::PNode::Pat(inner, _) => self.is_known_sized(r, inner, depth + 1, budget),
            tc::PNode::Tuple(l) | tc::PNode::Record(_, l) | tc::PNode::Union(l) => {
                let elems = self.copy_own_list(l)?;
                for &e in elems.iter() {
                    if !self.is_known_sized(r, e, depth + 1, budget)? {
                        return Some(false);
                    }
                }
                Some(true)
            }
            tc::PNode::Adt(adt, args_list) => {
                if !r.store.adts.contains_key(&adt)
                    && let Some(lib) = r.keys.library_in(r.names, adt)
                {
                    super::ensure_one(r, Want::Adt(lib));
                }
                let args = self.copy_own_list(args_list)?;
                let last_field = {
                    let row = BodyHeadFacts::new(&*r.store, r.names).adt(adt)?;
                    if row.kind != tc::AdtKind::Struct {
                        return Some(true);
                    }
                    let Some(last) = row.fields.last() else {
                        return Some(true);
                    };
                    let stys = r.store.tys.view();
                    self.copy_from_table(stys, last.ty, Some(&args), 0, budget)?
                };
                self.is_known_sized(r, last_field, depth + 1, budget)
            }
            _ => None,
        }
    }
}

/// Quick, allocation-free check for whether any declared field type of an ADT contains a
/// `PNode::Alias` node (or exceeds the small stack bound, in which case `true` triggers the full
/// [`FieldAliasGuard`] walk).
pub(super) fn adt_has_alias_field(tys: tc::PTys<'_>, fields: &[tc::Field]) -> bool {
    let mut stack = [tc::PTy(0); 64];
    for f in fields {
        let mut sp = 1usize;
        stack[0] = f.ty;
        let mut steps = 0u32;
        while sp > 0 {
            steps += 1;
            if steps > 512 {
                return true;
            }
            sp -= 1;
            let ty = stack[sp];
            let Ok(node) = tys.node(ty) else { return true };
            match node {
                tc::PNode::Alias { .. } => return true,
                tc::PNode::Ref(_, i)
                | tc::PNode::Ptr(_, i)
                | tc::PNode::Slice(i)
                | tc::PNode::Array(i, _)
                | tc::PNode::Pat(i, _) => {
                    if sp >= stack.len() {
                        return true;
                    }
                    stack[sp] = i;
                    sp += 1;
                }
                tc::PNode::Tuple(l)
                | tc::PNode::Record(_, l)
                | tc::PNode::Union(l)
                | tc::PNode::FnPtr(l, _)
                | tc::PNode::Adt(_, l)
                | tc::PNode::Dynamic(l)
                | tc::PNode::FnDef(_, l)
                | tc::PNode::Opaque(_, l)
                | tc::PNode::ExTrait(_, l) => {
                    let Ok(es) = tys.list(l) else { return true };
                    if sp + es.len() > stack.len() {
                        return true;
                    }
                    for &e in es {
                        stack[sp] = e;
                        sp += 1;
                    }
                }
                tc::PNode::ExProj { args, term, .. } => {
                    let Ok(es) = tys.list(args) else { return true };
                    if sp + es.len() + 1 > stack.len() {
                        return true;
                    }
                    stack[sp] = term;
                    sp += 1;
                    for &e in es {
                        stack[sp] = e;
                        sp += 1;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

/// Per-body soundness guard for ADT fields whose declared type contains `PNode::Alias`.
///
/// An unnormalized `PNode::Alias` in `RegionSlots` has slots `[own_lifetimes..., arg_slots...]`
/// in pre-order, matching [`field_row`] exactly (clearing `Hit::root: &'b K::Root` and
/// `Lazy::started: Option<Started<'a, F::Start, F::Ids, F::Maker>>` across generic methods).
/// However, if an alias normalizes in a specific body (via a concrete instantiation such as
/// `Foo<Pair<'a, 'b>>` whose impl swaps lifetimes, via a blanket impl, or via an owner/ADT/supertrait
/// `PClause::Projection` such as `where F: Front<Maker = RunMaker<'h>>`), the unnormalized
/// `FieldRegion` row can either misalign (`FieldRegionsUnaligned`) or unsoundly misattribute
/// lifetimes. This guard suppresses `Facts::field_regions` to `None` ONLY for fields whose
/// `PNode::Alias` is not provably unnormalized in the body, while leaving sibling fields and
/// provably unnormalized alias fields intact.
pub(super) struct FieldAliasGuard<'a> {
    owner_projs: Option<(AVec<'a, (tc::ItemKey, tc::Name)>, bool)>,
    blanket_cache: AVec<'a, (tc::ItemKey, bool)>,
    read_blankets: AVec<'a, (u128, u64)>,
    read_supers: AVec<'a, (u128, u64)>,
    alloc: Alloc<'a>,
}

impl<'a> FieldAliasGuard<'a> {
    pub(super) fn new_in(alloc: Alloc<'a>) -> FieldAliasGuard<'a> {
        FieldAliasGuard {
            owner_projs: None,
            blanket_cache: AVec::new_in(alloc),
            read_blankets: AVec::new_in(alloc),
            read_supers: AVec::new_in(alloc),
            alloc,
        }
    }

    pub(super) fn push_reads(&self, reads: &mut AVec<'a, Read>) {
        for &(tr, digest) in self.read_blankets.iter() {
            reads.push(field_alias_blanket_read(tr, digest));
        }
        for &(tr, digest) in self.read_supers.iter() {
            reads.push(field_alias_super_read(tr, digest));
        }
    }

    fn collect_projs_from<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        item: tc::ItemKey,
        out: &mut AVec<'a, (tc::ItemKey, tc::Name)>,
    ) -> bool {
        let mut queue: AVec<'a, tc::ItemKey> = AVec::new_in(self.alloc);
        let mut visited: AVec<'a, tc::ItemKey> = AVec::new_in(self.alloc);
        if let Some(preds) = r.store.preds.get(&item) {
            for c in preds.clauses.iter() {
                match *c {
                    tc::PClause::Projection { trait_, name, .. } => {
                        if !out.iter().any(|&(t, n)| t == trait_ && n == name) {
                            out.push((trait_, name));
                        }
                    }
                    tc::PClause::Trait { trait_, .. } => {
                        if queue.scan_for(&trait_).is_none() {
                            queue.push(trait_);
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut cursor = 0usize;
        while let Some(&tr) = queue.get(cursor) {
            cursor += 1;
            if visited.scan_for(&tr).is_some() {
                continue;
            }
            if visited.len() >= 64 {
                return false;
            }
            visited.push(tr);
            if !r.store.trait_defs.contains_key(&tr)
                && let Some(lib) = r.keys.library_in(r.names, tr)
            {
                super::ensure_one(r, Want::Members(lib));
            }
            let Some(def) = r.store.trait_defs.get(&tr) else {
                if !self.read_supers.iter().any(|&(k, _)| k == tr.0) {
                    let mut h = tc::StableHasher::new("blaze-ladder front2 field alias super v1");
                    h.tag(0);
                    self.read_supers.push((tr.0, h.finish()));
                }
                return false;
            };
            let mut h = tc::StableHasher::new("blaze-ladder front2 field alias super v1");
            h.tag(1);
            h.u32(u32::try_from(def.supertraits.len()).unwrap_or(u32::MAX));
            for sc in def.supertraits.iter() {
                match *sc {
                    tc::PClause::Projection { trait_: pt, name: pn, .. } => {
                        h.tag(1);
                        h.u128(pt.0);
                        h.u64(pn.0);
                        if !out.iter().any(|&(t, n)| t == pt && n == pn) {
                            out.push((pt, pn));
                        }
                    }
                    tc::PClause::Trait { trait_: st, .. } => {
                        h.tag(2);
                        h.u128(st.0);
                        if queue.scan_for(&st).is_none() {
                            queue.push(st);
                        }
                    }
                    _ => h.tag(0),
                }
            }
            if !self.read_supers.iter().any(|&(k, _)| k == tr.0) {
                self.read_supers.push((tr.0, h.finish()));
            }
        }
        true
    }

    fn trait_has_no_blanket<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        trait_: tc::ItemKey,
    ) -> bool {
        if let Some(&(_, no_blanket)) = self.blanket_cache.iter().find(|&&(k, _)| k == trait_) {
            return no_blanket;
        }
        let ans = answer_impls(r, trait_, tc::Shape::Param);
        let no_blanket = ans.as_ref().is_some_and(|v| v.is_empty());
        if !self.read_blankets.iter().any(|&(k, _)| k == trait_.0) {
            let mut h = tc::StableHasher::new("blaze-ladder front2 field alias blanket v1");
            match &ans {
                None => h.tag(0),
                Some(v) => {
                    h.tag(1);
                    h.u32(u32::try_from(v.len()).unwrap_or(u32::MAX));
                    for &imp in v.iter() {
                        h.u128(imp.0);
                    }
                }
            }
            self.read_blankets.push((trait_.0, h.finish()));
        }
        self.blanket_cache.push((trait_, no_blanket));
        no_blanket
    }

    /// Appends the field indices of `adt` whose `PNode::Alias` is not provably unnormalized in
    /// `body_tys` onto `out`, returning `Some((start, len))` when any field was suppressed.
    pub(super) fn check_adt<'n, S: NamesSource<'n>>(
        &mut self,
        r: &mut Records<'_, 'n, S>,
        owner: tc::ItemKey,
        body_tys: tc::PTys<'_>,
        adt: tc::ItemKey,
        out: &mut AVec<'a, u32>,
    ) -> Option<(u32, u32)> {
        if self.owner_projs.is_none() {
            let mut projs: AVec<'a, (tc::ItemKey, tc::Name)> = AVec::new_in(self.alloc);
            let complete = self.collect_projs_from(r, owner, &mut projs);
            self.owner_projs = Some((projs, complete));
        }
        let mut adt_projs: AVec<'a, (tc::ItemKey, tc::Name)> = AVec::new_in(self.alloc);
        let adt_complete = self.collect_projs_from(r, adt, &mut adt_projs);

        // Snapshot every instantiation `PNode::Adt(adt, inst_list)` appearing in `body_tys`.
        let mut insts: AVec<'a, tc::List> = AVec::new_in(self.alloc);
        for node in body_tys.nodes() {
            if let tc::PNode::Adt(k, args) = *node
                && k == adt
            {
                insts.push(args);
            }
        }

        // Snapshot each field's `PNode::Alias` occurrences from `r.store.tys` before querying
        // `answer_impls` (which borrows `r` mutably).
        struct FieldAliases {
            field_idx: u32,
            start: usize,
            len: usize,
            incomplete: bool,
        }
        let mut field_entries: AVec<'a, FieldAliases> = AVec::new_in(self.alloc);
        let mut aliases: AVec<'a, (tc::ItemKey, tc::Name, Option<u32>)> = AVec::new_in(self.alloc);
        {
            let Some(row) = BodyHeadFacts::new(&*r.store, r.names).adt(adt) else {
                return None;
            };
            let stys = r.store.tys.view();
            let mut stack: AVec<'a, tc::PTy> = AVec::new_in(self.alloc);
            for (fi, f) in row.fields.iter().enumerate() {
                let Ok(fi_u32) = u32::try_from(fi) else { continue };
                let a_start = aliases.len();
                stack.clear();
                stack.push(f.ty);
                let mut steps = 0u32;
                let mut incomplete = false;
                while let Some(ty) = stack.pop() {
                    steps += 1;
                    if steps > MOST_NODES {
                        incomplete = true;
                        break;
                    }
                    let Ok(node) = stys.node(ty) else {
                        incomplete = true;
                        break;
                    };
                    match node {
                        tc::PNode::Alias { trait_, name, args } => {
                            let Ok(es) = stys.list(args) else {
                                incomplete = true;
                                break;
                            };
                            let self_param = es.first().and_then(|&first| match stys.node(first) {
                                Ok(tc::PNode::Param(p)) => Some(p),
                                _ => None,
                            });
                            aliases.push((trait_, name, self_param));
                            stack.extend_from_slice(es);
                        }
                        tc::PNode::Ref(_, i)
                        | tc::PNode::Ptr(_, i)
                        | tc::PNode::Slice(i)
                        | tc::PNode::Array(i, _)
                        | tc::PNode::Pat(i, _) => stack.push(i),
                        tc::PNode::Tuple(l)
                        | tc::PNode::Record(_, l)
                        | tc::PNode::Union(l)
                        | tc::PNode::FnPtr(l, _)
                        | tc::PNode::Adt(_, l)
                        | tc::PNode::Dynamic(l)
                        | tc::PNode::FnDef(_, l)
                        | tc::PNode::Opaque(_, l)
                        | tc::PNode::ExTrait(_, l) => match stys.list(l) {
                            Ok(es) => stack.extend_from_slice(es),
                            Err(_) => {
                                incomplete = true;
                                break;
                            }
                        },
                        tc::PNode::ExProj { args, term, .. } => {
                            stack.push(term);
                            match stys.list(args) {
                                Ok(es) => stack.extend_from_slice(es),
                                Err(_) => {
                                    incomplete = true;
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let a_len = aliases.len() - a_start;
                if a_len > 0 || incomplete {
                    field_entries.push(FieldAliases { field_idx: fi_u32, start: a_start, len: a_len, incomplete });
                }
            }
        }
        if field_entries.is_empty() {
            return None;
        }

        let start = out.len();
        let owner_complete = self.owner_projs.as_ref().is_some_and(|&(_, c)| c);
        for fe in field_entries.iter() {
            let mut safe = !fe.incomplete && owner_complete && adt_complete && !insts.is_empty();
            if safe {
                for &(trait_, name, self_param) in &aliases[fe.start..fe.start + fe.len] {
                    let Some(p) = self_param else {
                        safe = false;
                        break;
                    };
                    let Ok(pi) = usize::try_from(p) else {
                        safe = false;
                        break;
                    };
                    let all_param = insts.iter().all(|&inst_list| {
                        body_tys
                            .list(inst_list)
                            .ok()
                            .and_then(|es| es.get(pi).copied())
                            .is_some_and(|arg| matches!(body_tys.node(arg), Ok(tc::PNode::Param(_))))
                    });
                    if !all_param {
                        safe = false;
                        break;
                    }
                    let has_owner_proj = self
                        .owner_projs
                        .as_ref()
                        .is_some_and(|(ps, _)| ps.iter().any(|&(t, n)| t == trait_ && n == name));
                    let has_adt_proj = adt_projs.iter().any(|&(t, n)| t == trait_ && n == name);
                    if has_owner_proj || has_adt_proj || !self.trait_has_no_blanket(r, trait_) {
                        safe = false;
                        break;
                    }
                }
            }
            if !safe {
                out.push(fe.field_idx);
            }
        }
        let added = out.len() - start;
        if added == 0 {
            None
        } else {
            Some((u32::try_from(start).ok()?, u32::try_from(added).ok()?))
        }
    }
}

/// Convenience wrapper evaluating one instantiated body `PTy` and recording its Rule 21 reads.
#[allow(dead_code)]
pub(super) fn inst_needs_drop<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    langs: super::BorrowLangs,
    body_tys: tc::PTys<'_>,
    ty: tc::PTy,
    reads: &mut AVec<'a, Read>,
    alloc: Alloc<'a>,
) -> Option<bool> {
    let mut eval = InstDropEval::new_in(alloc);
    let res = eval.eval_body_ty(r, langs, body_tys, ty);
    eval.push_reads(reads);
    res
}

fn match_pattern(
    pat_tys: tc::PTys<'_>,
    pat: tc::PTy,
    val_tys: tc::PTys<'_>,
    val: tc::PTy,
    subst: &mut [Option<tc::PTy>],
    depth: u32,
) -> bool {
    if depth > MOST_INST_DROP_DEPTH {
        return false;
    }
    let (Ok(pn), Ok(vn)) = (pat_tys.node(pat), val_tys.node(val)) else {
        return false;
    };
    match (pn, vn) {
        (tc::PNode::Param(p) | tc::PNode::ConstParam(p), _) => {
            let Ok(pi) = usize::try_from(p) else { return false };
            let Some(slot) = subst.get_mut(pi) else { return false };
            match *slot {
                None => {
                    *slot = Some(val);
                    true
                }
                Some(prev) => prev == val,
            }
        }
        (tc::PNode::Bool, tc::PNode::Bool)
        | (tc::PNode::Char, tc::PNode::Char)
        | (tc::PNode::Str, tc::PNode::Str)
        | (tc::PNode::Never, tc::PNode::Never) => true,
        (tc::PNode::Int(a), tc::PNode::Int(b)) => a == b,
        (tc::PNode::Uint(a), tc::PNode::Uint(b)) => a == b,
        (tc::PNode::Float(a), tc::PNode::Float(b)) => a == b,
        (tc::PNode::Foreign(a), tc::PNode::Foreign(b)) => a == b,
        (tc::PNode::ExAuto(a), tc::PNode::ExAuto(b)) => a == b,
        (tc::PNode::ConstValue { ty: pt, bits: pb }, tc::PNode::ConstValue { ty: vt, bits: vb }) => {
            pb == vb && match_pattern(pat_tys, pt, val_tys, vt, subst, depth + 1)
        }
        (tc::PNode::Ref(pm, pi), tc::PNode::Ref(vm, vi)) if pm == vm => {
            match_pattern(pat_tys, pi, val_tys, vi, subst, depth + 1)
        }
        (tc::PNode::Ptr(pm, pi), tc::PNode::Ptr(vm, vi)) if pm == vm => {
            match_pattern(pat_tys, pi, val_tys, vi, subst, depth + 1)
        }
        (tc::PNode::Slice(pi), tc::PNode::Slice(vi)) => {
            match_pattern(pat_tys, pi, val_tys, vi, subst, depth + 1)
        }
        (tc::PNode::Array(pe, pl), tc::PNode::Array(ve, vl)) => {
            match_pattern(pat_tys, pe, val_tys, ve, subst, depth + 1)
                && match_pattern(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (tc::PNode::Pat(pb, pr), tc::PNode::Pat(vb, vr)) => {
            match_pattern(pat_tys, pb, val_tys, vb, subst, depth + 1)
                && match_pattern_lists(pat_tys, pr, val_tys, vr, subst, depth + 1)
        }
        (tc::PNode::Tuple(pl), tc::PNode::Tuple(vl))
        | (tc::PNode::Union(pl), tc::PNode::Union(vl))
        | (tc::PNode::Dynamic(pl), tc::PNode::Dynamic(vl)) => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (tc::PNode::Record(ps, pl), tc::PNode::Record(vs, vl)) if ps == vs => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (tc::PNode::Adt(pk, pl), tc::PNode::Adt(vk, vl)) if pk == vk => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (tc::PNode::FnDef(pk, pl), tc::PNode::FnDef(vk, vl)) if pk == vk => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (tc::PNode::FnPtr(pl, ph), tc::PNode::FnPtr(vl, vh)) if ph == vh => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (tc::PNode::Opaque(pk, pl), tc::PNode::Opaque(vk, vl)) if pk == vk => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (tc::PNode::ExTrait(pk, pl), tc::PNode::ExTrait(vk, vl)) if pk == vk => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
        }
        (
            tc::PNode::Alias { trait_: pt, name: pn, args: pl },
            tc::PNode::Alias { trait_: vt, name: vn, args: vl },
        ) if pt == vt && pn == vn => match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1),
        (
            tc::PNode::ExProj { trait_: pt, name: pn, args: pl, term: pterm },
            tc::PNode::ExProj { trait_: vt, name: vn, args: vl, term: vterm },
        ) if pt == vt && pn == vn => {
            match_pattern_lists(pat_tys, pl, val_tys, vl, subst, depth + 1)
                && match_pattern(pat_tys, pterm, val_tys, vterm, subst, depth + 1)
        }
        _ => false,
    }
}

fn match_pattern_lists(
    pat_tys: tc::PTys<'_>,
    pl: tc::List,
    val_tys: tc::PTys<'_>,
    vl: tc::List,
    subst: &mut [Option<tc::PTy>],
    depth: u32,
) -> bool {
    let (Ok(ps), Ok(vs)) = (pat_tys.list(pl), val_tys.list(vl)) else {
        return false;
    };
    ps.len() == vs.len()
        && ps.iter().zip(vs.iter()).all(|(&p, &v)| match_pattern(pat_tys, p, val_tys, v, subst, depth))
}

fn hash_shape(h: &mut tc::StableHasher, shape: tc::Shape) {
    match shape {
        tc::Shape::Bool => h.tag(0),
        tc::Shape::Char => h.tag(1),
        tc::Shape::Int(i) => {
            h.tag(2);
            h.tag(i as u8);
        }
        tc::Shape::Uint(u) => {
            h.tag(3);
            h.tag(u as u8);
        }
        tc::Shape::Float(f) => {
            h.tag(4);
            h.tag(f as u8);
        }
        tc::Shape::Str => h.tag(5),
        tc::Shape::Never => h.tag(6),
        tc::Shape::Tuple(n) => {
            h.tag(7);
            h.u32(n);
        }
        tc::Shape::Record(s, n) => {
            h.tag(8);
            h.u64(s);
            h.u32(n);
        }
        tc::Shape::Union(n) => {
            h.tag(9);
            h.u32(n);
        }
        tc::Shape::Ref(m) => {
            h.tag(10);
            h.tag(u8::from(m == tc::Mutbl::Mut));
        }
        tc::Shape::Ptr(m) => {
            h.tag(11);
            h.tag(u8::from(m == tc::Mutbl::Mut));
        }
        tc::Shape::Array => h.tag(12),
        tc::Shape::Slice => h.tag(13),
        tc::Shape::Pat => h.tag(14),
        tc::Shape::Adt(k) => {
            h.tag(15);
            h.u128(k.0);
        }
        tc::Shape::FnDef(k) => {
            h.tag(16);
            h.u128(k.0);
        }
        tc::Shape::FnPtr(n) => {
            h.tag(17);
            h.u32(n);
        }
        tc::Shape::Closure => h.tag(18),
        tc::Shape::Param => h.tag(19),
        tc::Shape::Opaque(k) => {
            h.tag(20);
            h.u128(k.0);
        }
        tc::Shape::Foreign(k) => {
            h.tag(21);
            h.u128(k.0);
        }
        tc::Shape::Dynamic(k) => {
            h.tag(22);
            match k {
                None => h.tag(0),
                Some(t) => {
                    h.tag(1);
                    h.u128(t.0);
                }
            }
        }
        tc::Shape::AnyInt => h.tag(23),
        tc::Shape::AnyFloat => h.tag(24),
    }
}

fn hash_pty_structural(tys: tc::PTys<'_>, root: tc::PTy, h: &mut tc::StableHasher, budget: &mut u32, stack: &mut AVec<'_, tc::PTy>) {
    stack.clear();
    stack.push(root);
    while let Some(ty) = stack.pop() {
        if *budget == 0 {
            h.tag(0xff);
            stack.clear();
            return;
        }
        *budget -= 1;
        match tys.node(ty) {
            Ok(tc::PNode::Bool) => h.tag(1),
            Ok(tc::PNode::Char) => h.tag(2),
            Ok(tc::PNode::Int(i)) => {
                h.tag(3);
                h.tag(i as u8);
            }
            Ok(tc::PNode::Uint(u)) => {
                h.tag(4);
                h.tag(u as u8);
            }
            Ok(tc::PNode::Float(f)) => {
                h.tag(5);
                h.tag(f as u8);
            }
            Ok(tc::PNode::Never) => h.tag(6),
            Ok(tc::PNode::Str) => h.tag(7),
            Ok(tc::PNode::Ptr(m, inner)) => {
                h.tag(8);
                h.tag(u8::from(m == tc::Mutbl::Mut));
                stack.push(inner);
            }
            Ok(tc::PNode::Ref(m, inner)) => {
                h.tag(9);
                h.tag(u8::from(m == tc::Mutbl::Mut));
                stack.push(inner);
            }
            Ok(tc::PNode::Slice(inner)) => {
                h.tag(10);
                stack.push(inner);
            }
            Ok(tc::PNode::Array(elem, len)) => {
                h.tag(11);
                stack.push(len);
                stack.push(elem);
            }
            Ok(tc::PNode::Pat(elem, _)) => {
                h.tag(12);
                stack.push(elem);
            }
            Ok(tc::PNode::ConstValue { ty: ct, bits }) => {
                h.tag(13);
                h.u128(bits);
                stack.push(ct);
            }
            Ok(tc::PNode::ConstParam(cp)) => {
                h.tag(14);
                h.u32(cp);
            }
            Ok(tc::PNode::Param(p)) => {
                h.tag(15);
                h.u32(p);
            }
            Ok(tc::PNode::Tuple(l)) => {
                h.tag(16);
                if let Ok(es) = tys.list(l) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::Record(s, l)) => {
                h.tag(17);
                h.u64(s.0);
                if let Ok(es) = tys.list(l) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::Union(l)) => {
                h.tag(18);
                if let Ok(es) = tys.list(l) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::Adt(k, args)) => {
                h.tag(19);
                h.u128(k.0);
                if let Ok(es) = tys.list(args) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::Alias { trait_, name, args }) => {
                h.tag(20);
                h.u128(trait_.0);
                h.u64(name.0);
                if let Ok(es) = tys.list(args) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::FnDef(k, args)) => {
                h.tag(21);
                h.u128(k.0);
                if let Ok(es) = tys.list(args) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::FnPtr(args, _)) => {
                h.tag(22);
                if let Ok(es) = tys.list(args) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::Opaque(k, args)) => {
                h.tag(23);
                h.u128(k.0);
                if let Ok(es) = tys.list(args) {
                    h.u32(u32::try_from(es.len()).unwrap_or(u32::MAX));
                    for &e in es.iter().rev() {
                        stack.push(e);
                    }
                }
            }
            Ok(tc::PNode::Foreign(k)) => {
                h.tag(24);
                h.u128(k.0);
            }
            _ => h.tag(25),
        }
    }
}

#[cfg(test)]
mod field_row_tests {
    //! A field's row is its head column's slots, read at the field type's own positions (D148):
    //! `struct W<'a>(&'a u8)`'s field is `Lifetime(0)`, so the flow lowering ties a borrow
    //! stored in it to the value's `'a` (lib.rs `field_slot`: the referent must outlive it, or
    //! E0597). The control moves the column's one slot to another parameter and the row moves
    //! with it: the row is read from the column, not from the text or a guess (rule 23).

    use super::*;
    use blaze_typeck::{Abi, FnPtrHeader, List, Mutbl, PNode, PortableTys, Safety, UintTy};

    const TR: tc::ItemKey = tc::ItemKey(0x7a);
    const RUST: FnPtrHeader = FnPtrHeader { abi: Abi::Rust, safety: Safety::Safe, c_variadic: false };

    /// `trait Tr<'x>`: one lifetime; no ADT, opaque or alias is known.
    struct Toy;

    impl Arity for Toy {
        fn adt_lifetimes(&self, _: tc::ItemKey) -> tc::regions::Count {
            tc::regions::Count::NotKnown
        }
        fn opaque_captures(&self, _: tc::ItemKey) -> tc::regions::Count {
            tc::regions::Count::NotKnown
        }
        fn alias_lifetimes(&self, trait_: tc::ItemKey, _: tc::Name) -> tc::regions::Count {
            if trait_ == TR { tc::regions::Count::Known(1) } else { tc::regions::Count::NotKnown }
        }
        fn trait_lifetimes(&self, trait_: tc::ItemKey) -> tc::regions::Count {
            if trait_ == TR { tc::regions::Count::Known(1) } else { tc::regions::Count::NotKnown }
        }
    }

    fn a() -> Alloc<'static> {
        Alloc::global()
    }

    fn row(t: &PortableTys<'_>, ty: tc::PTy, col: &[Region], lifetimes: u32) -> Result<std::vec::Vec<FieldRegion>, Unlaid> {
        let mut out: AVec<'static, FieldRegion> = AVec::new_in(a());
        field_row(t.view(), ty, &Toy, col, lifetimes, &mut out, a())?;
        Ok(out.iter().copied().collect())
    }

    fn ref_u8(t: &mut PortableTys<'_>) -> tc::PTy {
        let u = t.intern(PNode::Uint(UintTy::U8)).unwrap();
        t.intern(PNode::Ref(Mutbl::Not, u)).unwrap()
    }

    /// `struct W<'a>(&'a u8)`: the reference's slot is `'a`; the control, `struct W<'a, 'b>(&'b
    /// u8)` (the slot moved to the other parameter), reads `'b`.
    #[test]
    fn a_reference_field_is_its_adts_lifetime() {
        let mut t = PortableTys::new_in(a());
        let r = ref_u8(&mut t);
        assert_eq!(row(&t, r, &[Region::Param(0)], 1), Ok(std::vec![FieldRegion::Lifetime(0)]));
        assert_eq!(row(&t, r, &[Region::Param(1)], 2), Ok(std::vec![FieldRegion::Lifetime(1)]));
        assert_eq!(row(&t, r, &[Region::Static], 1), Ok(std::vec![FieldRegion::Static]));
    }

    /// A slot naming a lifetime the ADT does not declare, a column one short and one long: each
    /// refused by name, never read at a wrong index.
    #[test]
    fn a_column_that_does_not_line_up_is_refused() {
        let mut t = PortableTys::new_in(a());
        let r = ref_u8(&mut t);
        assert!(matches!(row(&t, r, &[Region::Param(1)], 1), Err(Unlaid::Shape(_))));
        assert!(matches!(row(&t, r, &[], 1), Err(Unlaid::Shape(_))));
        assert!(matches!(row(&t, r, &[Region::Param(0), Region::Static], 1), Err(Unlaid::Shape(_))));
    }

    /// `&'a (dyn Tr<'b> + 'c)` in `struct D<'a, 'b, 'c>`: the column is `['a, 'c, 'b]` (the
    /// reference's slot, the object's own slot, then its trait's); the row walks `Dynamic` and
    /// `ExTrait` in pre-order matching `RegionCol` / `RegionSlots`, yielding `['a, 'c, 'b]`.
    #[test]
    fn a_trait_objects_position_is_its_own_slot() {
        let mut t = PortableTys::new_in(a());
        let principal = t.intern(PNode::ExTrait(TR, List::EMPTY)).unwrap();
        let d = t.dynamic(&[principal]).unwrap();
        let r = t.intern(PNode::Ref(Mutbl::Not, d)).unwrap();
        let col = [Region::Param(0), Region::Param(2), Region::Param(1)];
        assert_eq!(row(&t, r, &col, 3), Ok(std::vec![FieldRegion::Lifetime(0), FieldRegion::Lifetime(2), FieldRegion::Lifetime(1)]));
    }

    /// `fn(&u8, fn(&u8))`: two binders' variables are two bound regions, not one; a type
    /// parameter is its argument's positions.
    #[test]
    fn bound_regions_are_numbered_per_binder_variable() {
        let mut t = PortableTys::new_in(a());
        let r = ref_u8(&mut t);
        let unit = t.intern_list(&[]).unwrap();
        let unit = t.intern(PNode::Tuple(unit)).unwrap();
        let inner_io = t.intern_list(&[r, unit]).unwrap();
        let inner = t.intern(PNode::FnPtr(inner_io, RUST)).unwrap();
        let io = t.intern_list(&[r, inner, unit]).unwrap();
        let f = t.intern(PNode::FnPtr(io, RUST)).unwrap();
        let col = [Region::Late { binder: 0, var: 0 }, Region::Late { binder: 1, var: 0 }];
        assert_eq!(row(&t, f, &col, 0), Ok(std::vec![FieldRegion::Bound(0), FieldRegion::Bound(1)]));
        let p = t.intern(PNode::Param(0)).unwrap();
        assert_eq!(row(&t, p, &[], 0), Ok(std::vec![FieldRegion::Arg(0)]));
    }

    /// A field whose declared type holds a projection (`PNode::Alias`) lays its own lifetime
    /// slots followed by its arguments in pre-order (matching `RegionSlots` for unnormalized
    /// aliases); per-body normalization is guarded by [`FieldAliasGuard`].
    #[test]
    fn alias_field_lays_own_and_arg_slots() {
        let mut t = PortableTys::new_in(a());
        let p = t.intern(PNode::Param(0)).unwrap();
        let args = t.intern_list(&[p]).unwrap();
        let alias = t.intern(PNode::Alias { trait_: TR, name: tc::Name::of("Maker"), args }).unwrap();
        assert_eq!(row(&t, alias, &[Region::Param(0)], 1), Ok(std::vec![FieldRegion::Lifetime(0), FieldRegion::Arg(0)]));
    }
}
