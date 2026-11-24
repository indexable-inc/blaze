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

use crate::front2::borrow_reads::drop_adt_read;
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
    let reader = store.regions.reader(store.tys.alloc());
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
    for c in store.preds.get(&imp)?.clauses.iter() {
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

/// Structural drop summary for `adt`: `(needs_drop_unconditionally, drop_args_span)`.
/// Computes a bounded fixed point over ADTs reachable from `adt`'s fields in dropping positions
/// and files a `BorrowAdt` (`DROP_ADT_DETAIL`) read row for each nested ADT inspected (rule 21).
#[expect(clippy::too_many_arguments, reason = "the ADT's already-queried Copy/Drop facts and output buffers")]
pub fn drop_facts_of<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    langs: super::BorrowLangs,
    adt: tc::ItemKey,
    adt_drop_impl: Option<bool>,
    adt_uncond_copy: bool,
    need: &mut AVec<'a, bool>,
    reads: &mut AVec<'a, Read>,
    alloc: Alloc<'a>,
) -> (bool, Option<(u32, u32)>) {
    let is_inert = |k: tc::ItemKey| langs.manually_drop == Some(k) || langs.phantom_data == Some(k);
    if is_inert(adt) || adt_uncond_copy {
        let generics = BodyHeadFacts::new(&*r.store, r.names).adt(adt).map_or(0, |row| row.generics);
        return push_drop_mask(generics, 0, need);
    }
    if adt_drop_impl != Some(false) {
        return (true, None);
    }
    let Some(root_row) = BodyHeadFacts::new(&*r.store, r.names).adt(adt) else {
        return (true, None);
    };
    let root_generics = root_row.generics;
    if root_row.kind == tc::AdtKind::Union {
        return push_drop_mask(root_generics, 0, need);
    }
    if root_generics > 64 {
        return (true, None);
    }

    // Discover reachable ADTs in dropping positions (BFS, bounded by MOST_DROP_ADTS).
    struct DropNode {
        key: tc::ItemKey,
        fixed: Option<(bool, u64)>,
        generics: u32,
        uncond: bool,
        params: u64,
    }
    let mut nodes: AVec<'a, DropNode> = AVec::new_in(alloc);
    nodes.push(DropNode { key: adt, fixed: None, generics: root_generics, uncond: false, params: 0 });
    let mut cursor = 0usize;
    let mut capped = false;
    let mut field_tys: AVec<'a, tc::PTy> = AVec::new_in(alloc);
    let mut field_spans: AVec<'a, (usize, usize)> = AVec::new_in(alloc);
    let mut ty_stack: AVec<'a, tc::PTy> = AVec::new_in(alloc);
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
        } else if inert || uncond_copy {
            Some(false)
        } else {
            super::has_impl(r, langs.drop, key).ok()
        };
        let row_opt = BodyHeadFacts::new(&*r.store, r.names).adt(key);
        let (made, union, generics) = match row_opt {
            Some(row) => (true, row.kind == tc::AdtKind::Union, row.generics),
            None => (false, false, 0),
        };
        let fields_start = field_tys.len();
        let mut fhash = tc::StableHasher::new("blaze-ladder front2 borrow drop fields v1");
        if let Some(row) = row_opt {
            let tys = r.store.tys.view();
            fhash.u32(u32::try_from(row.fields.len()).unwrap_or(u32::MAX));
            for f in row.fields.iter() {
                field_tys.push(f.ty);
                let mut hb = MOST_DROP_NODES;
                hash_pty_for_drop(tys, f.ty, &mut fhash, &mut hb, &mut ty_stack);
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
        if let Some(node) = nodes.get_mut(cursor) {
            node.generics = generics;
            node.fixed = fixed;
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
                ty_stack.extend_from_slice(slice);
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
                            nodes.push(DropNode { key: nested, fixed: None, generics: 0, uncond: false, params: 0 });
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
        return (true, None);
    }

    // Monotone fixed point over `nodes`: each pass only sets `uncond = true` or sets bits in `params`.
    let tys = r.store.tys.view();
    let max_iters = nodes.len().saturating_add(2);
    for _ in 0..max_iters {
        let mut changed = false;
        for idx in 0..nodes.len() {
            let Some((fixed, generics, old_uncond, old_params)) = nodes.get(idx).map(|n| (n.fixed, n.generics, n.uncond, n.params)) else {
                continue;
            };
            if fixed.is_some() || old_uncond {
                continue;
            }
            let Some(&(fs, fe)) = field_spans.get(idx) else { continue };
            let mut uncond = old_uncond;
            let mut params = old_params;
            ty_stack.clear();
            if let Some(slice) = field_tys.get(fs..fe) {
                ty_stack.extend_from_slice(slice);
            }
            let mut steps = 0u32;
            while let Some(t) = ty_stack.pop() {
                steps += 1;
                if steps > MOST_DROP_NODES {
                    uncond = true;
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
                            break;
                        }
                    }
                    Ok(tc::PNode::Tuple(l)) => match tys.list(l) {
                        Ok(es) => ty_stack.extend_from_slice(es),
                        Err(_) => {
                            uncond = true;
                            break;
                        }
                    },
                    Ok(tc::PNode::Array(elem, _) | tc::PNode::Pat(elem, _)) => ty_stack.push(elem),
                    Ok(tc::PNode::Adt(nested, args)) => {
                        let Some(target) = nodes.iter().find(|n| n.key == nested) else {
                            uncond = true;
                            break;
                        };
                        if target.uncond {
                            uncond = true;
                            break;
                        }
                        let mask = target.params;
                        let Ok(es) = tys.list(args) else {
                            uncond = true;
                            break;
                        };
                        for (j, &arg) in es.iter().enumerate() {
                            if j >= 64 || (mask & (1u64 << j)) != 0 {
                                ty_stack.push(arg);
                            }
                        }
                    }
                    _ => {
                        uncond = true;
                        break;
                    }
                }
            }
            if (uncond != old_uncond || params != old_params)
                && let Some(node) = nodes.get_mut(idx)
            {
                node.uncond = uncond;
                node.params = params;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    match nodes.first() {
        Some(root) if !root.uncond => push_drop_mask(root_generics, root.params, need),
        _ => (true, None),
    }
}

fn push_drop_mask(generics: u32, mask: u64, need: &mut AVec<'_, bool>) -> (bool, Option<(u32, u32)>) {
    let Ok(start) = u32::try_from(need.len()) else { return (true, None) };
    for j in 0..generics {
        need.push(j < 64 && (mask & (1u64 << j)) != 0);
    }
    (false, Some((start, generics)))
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
        fn alias_lifetimes(&self, _: tc::ItemKey, _: tc::Name) -> tc::regions::Count {
            tc::regions::Count::NotKnown
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
    /// object's own slot before its trait's); the row is the reference's and the object's
    /// positions, `['a, 'c]`, the trait's slot stepped over.
    #[test]
    fn a_trait_objects_position_is_its_own_slot() {
        let mut t = PortableTys::new_in(a());
        let principal = t.intern(PNode::ExTrait(TR, List::EMPTY)).unwrap();
        let d = t.dynamic(&[principal]).unwrap();
        let r = t.intern(PNode::Ref(Mutbl::Not, d)).unwrap();
        let col = [Region::Param(0), Region::Param(2), Region::Param(1)];
        assert_eq!(row(&t, r, &col, 3), Ok(std::vec![FieldRegion::Lifetime(0), FieldRegion::Lifetime(2)]));
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
}
