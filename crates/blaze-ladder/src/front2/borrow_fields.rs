//! Two facts of an ADT the borrow check's lowering asks (blaze-flow-lower `Facts`), lane B11:
//!
//! - [`field_regions_of`]: each field's regions, read from its WRITTEN type (`Facts::field_regions`).
//!   THE RULE (the Reference, "Generic parameters", "Lifetime elision"): a field's type is its
//!   declared type at the value's arguments, so each lifetime it writes IS one of the ADT's
//!   lifetime parameters, or `'static`; each type parameter it names stands for every position
//!   of that argument. A struct field cannot elide a lifetime (rustc E0106), and a trait
//!   object's default lifetime there is the one the Reference's "Default trait object
//!   lifetimes" gives a fn parameter of the same type, so the field's written type is read by
//!   the ONE signature reader (blaze-flow-lower sig.rs `read_signature_with`), as the input of
//!   `fn f(x: TY) -> TY {}`, through the front end's world (borrow_sig_world.rs: paths resolved
//!   from the ADT's module, aliases expanded). Its regions come in the lowering's position order
//!   (`npos`: pre-order, a node's own position before its children's): a walk of the field's
//!   ERASED type in that order takes one region per position and gives `Arg(p)` at each type
//!   parameter `p` (zero positions of its own).
//! - [`copy_args_of`]: which type arguments the ADT's one `Copy` impl needs to be `Copy`
//!   (`Facts::copy_args`), read from the impl's own clauses: `T: Copy` asks it; `T: Sized`
//!   asks nothing the ADT's own well-formedness does not (when the ADT bounds that parameter by
//!   `Sized` too); `T: Clone` beside `T: Copy` asks nothing more (`Copy: Clone`). Any other
//!   clause, a self type that is not the ADT over distinct bare parameters, or not exactly one
//!   impl: no row (`None`: every argument must be `Copy`, the derive's rule).
//!
//! An unknown fact is `None`, never a guess (rule 3); what each answer was read from is in the
//! ADT's read row (borrow_reads.rs `adt_read`: the regions, the needs, every ADT lifetime
//! count and path answer the reading took; rule 21).
//!
//! FLOOR (rule 67): one reading of each field's written type per ADT per command. This reads
//! it once per ADT per BODY that names the ADT (gather_facts is per body): the ratio is the
//! number of bodies naming the ADT. A table of the store's (one row per ADT, kept with its
//! head) is the end state; NOT DONE in this piece.

use blaze_flow_lower::sig::SigWorld;
use blaze_flow_lower::{FieldRegion, Region, SigBuf};
use blaze_lower::{form as lf, head};
use blaze_own::{AVec, Alloc};
use blaze_typeck as tc;

use super::sig_world::{AskedPaths, FrontWorld, text_and_module};
use crate::front2::borrow_reads::AskedAdts;
use crate::front2::check::{Records, answer_impls};
use crate::front2::heads::{Store, Want};
use crate::front2::keys::{Keys, checker_of};
use crate::front2::source::NamesSource;

/// A field with no row in `at` (its regions not known).
pub const NO_ROW: (u32, u32) = (u32::MAX, 0);

/// The most nodes one field's type may have walked (rule 18): past it, no row.
const MOST_NODES: u32 = 4096;

/// Each field's regions of the ADT `adt` (`field_tys`: its fields' erased types, variant after
/// variant, as the store's row lists them): one entry of `at` per field, in order, `(start,
/// len)` into `list`, or [`NO_ROW`]. Every field gets an entry; a failure of the whole ADT
/// (no text, a text whose fields do not line up with the row's) gives every one `NO_ROW`.
#[expect(clippy::too_many_arguments, reason = "the names, the key map, the store, the ADT and its fields, the two outputs and the two recorders")]
pub fn field_regions_of<'n, 'a, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    store: &Store<'n>,
    adt: tc::ItemKey,
    is_enum: bool,
    field_tys: &[tc::PTy],
    at: &mut AVec<'a, (u32, u32)>,
    list: &mut AVec<'a, FieldRegion>,
    adts: &AskedAdts<'a>,
    paths: &AskedPaths<'a>,
    alloc: Alloc<'a>,
) {
    let from = at.len();
    let made = regions_or_none(names, keys, store, adt, is_enum, field_tys, at, list, adts, paths, alloc);
    if made.is_none() {
        at.truncate(from);
        for _ in field_tys {
            at.push(NO_ROW);
        }
    }
}

#[expect(clippy::too_many_arguments, reason = "field_regions_of's")]
fn regions_or_none<'n, 'a, S: NamesSource<'n>>(
    names: S,
    keys: &Keys,
    store: &Store<'n>,
    adt: tc::ItemKey,
    is_enum: bool,
    field_tys: &[tc::PTy],
    at: &mut AVec<'a, (u32, u32)>,
    list: &mut AVec<'a, FieldRegion>,
    adts: &AskedAdts<'a>,
    paths: &AskedPaths<'a>,
    alloc: Alloc<'a>,
) -> Option<()> {
    let lib = keys.library_in(names, adt)?;
    let (text, module) = text_and_module(names, &store.locals, lib)?;
    // The ADT's generics by name: its lifetimes in declared order (a `Named` region of the
    // reading maps to one by name), its type parameters (they shadow items in the world).
    let mut lifetimes: AVec<'a, tc::Name> = AVec::new_in(alloc);
    let mut params: AVec<'a, tc::Name> = AVec::new_in(alloc);
    let mut unread = false;
    super::generic_entries(super::super::head_text::after_outer_trivia(text), &mut |word| {
        if word.starts_with('\'') {
            lifetimes.push(tc::Name::of(word));
        } else if word.is_empty() || word == "#" {
            unread = true;
        } else if word != "const" {
            params.push(tc::Name::of(word));
        }
    });
    if unread {
        return None;
    }
    // The ADT's own count must be the one the lowering counts positions by.
    if lifetimes.len() as u32 != super::adt_head_lifetimes(keys, names, adt) {
        return None;
    }
    let low = super::super::heads::lower(names, lib, text).ok()?;
    let v = low.view();
    let mut written: AVec<'a, lf::TypeId> = AVec::new_in(alloc);
    let rows = v.rows(v.list(head::FIELDS)).ok()?;
    if is_enum {
        for &row in rows {
            for &f in v.rows(lf::Variant::decode(row).ok()?.fields).ok()? {
                written.push(lf::Field::decode(f).ok()?.ty);
            }
        }
    } else {
        for &f in rows {
            written.push(lf::Field::decode(f).ok()?.ty);
        }
    }
    if written.len() != field_tys.len() {
        return None;
    }
    let world = FrontWorld::new(names, keys, &store.locals, module, &params, adts, paths, alloc);
    let tys = store.tys.view();
    let mut sig_text: AVec<'a, u8> = AVec::new_in(alloc);
    let mut mapped: AVec<'a, FieldRegion> = AVec::new_in(alloc);
    for (j, &ty) in field_tys.iter().enumerate() {
        let start = list.len() as u32;
        // Its positions first: a field with none needs no reading (its row is its `Arg`s).
        let Some(positions) = walk(tys, ty, &world, None, list, alloc) else {
            at.push(NO_ROW);
            continue;
        };
        mapped.clear();
        if positions > 0 {
            let Some(written_ty) = v.type_range(written[j]).ok().and_then(|r| v.text_at(r).ok()) else {
                at.push(NO_ROW);
                continue;
            };
            sig_text.clear();
            for part in ["fn f(x: ", written_ty, ") -> ", written_ty, " {}"] {
                sig_text.extend_from_slice(part.as_bytes());
            }
            let Ok(fn_text) = core::str::from_utf8(&sig_text) else {
                at.push(NO_ROW);
                continue;
            };
            let mut buf = SigBuf::new_in(alloc);
            if blaze_flow_lower::sig::read_signature_with(fn_text, tys, &[ty], ty, &world, &mut buf, alloc).is_err() {
                at.push(NO_ROW);
                continue;
            }
            let end = buf.ends.first().copied().unwrap_or(0) as usize;
            let Some(regions) = buf.regions.get(..end) else {
                at.push(NO_ROW);
                continue;
            };
            let mut known = regions.len() as u32 == positions;
            for &r in regions {
                match field_region(r, &buf, &lifetimes) {
                    Some(f) => mapped.push(f),
                    None => known = false,
                }
            }
            if !known {
                at.push(NO_ROW);
                continue;
            }
        }
        match walk(tys, ty, &world, Some(&mapped[..]), list, alloc) {
            Some(_) => at.push((start, list.len() as u32 - start)),
            None => {
                list.truncate(start as usize);
                at.push(NO_ROW);
            }
        }
    }
    Some(())
}

/// A region of the reading as the field's: `'static`, or a lifetime the ADT declares (found by
/// name: the reader numbers names by first appearance, `SigBuf::names`). An anonymous, `Self`
/// or hidden region is no lifetime of the ADT: `None`.
fn field_region<A: core::alloc::Allocator + Clone>(r: Region, buf: &SigBuf<A>, lifetimes: &[tc::Name]) -> Option<FieldRegion> {
    match r {
        Region::Static => Some(FieldRegion::Static),
        Region::Named(_) => {
            let &(name, _) = buf.names.iter().find(|&&(_, x)| x == r)?;
            lifetimes.iter().position(|&l| l == name).map(|i| FieldRegion::Lifetime(i as u32))
        }
        // A lifetime a fn pointer's `for<..>` binds (lane B15): no parameter of the ADT's.
        Region::Bound(v) => Some(FieldRegion::Bound(v)),
        Region::Anon(_) | Region::SelfTy(_) | Region::Hidden(_) => None,
    }
}

/// Walks the field's erased type `root` in the lowering's position order (pre-order, a node's
/// own position before its children's, children left to right; an ADT's lifetimes before its
/// arguments). Counting (`regions: None`): its positions, nothing pushed. Emitting: each
/// position takes the next of `regions`, each type parameter `p` pushes `Arg(p)`, onto `out`.
/// `None` for a type whose positions no `FieldRegion` names (a projection, an opaque type, a
/// closure, a future, an inference or error node) or past [`MOST_NODES`].
fn walk<'a>(tys: tc::PTys<'_>, root: tc::PTy, world: &dyn SigWorld, regions: Option<&[FieldRegion]>, out: &mut AVec<'a, FieldRegion>, alloc: Alloc<'a>) -> Option<u32> {
    let mut stack: AVec<'a, tc::PTy> = AVec::new_in(alloc);
    stack.push(root);
    let (mut taken, mut steps) = (0u32, 0u32);
    let position = |taken: &mut u32, out: &mut AVec<'a, FieldRegion>| -> Option<()> {
        if let Some(rs) = regions {
            out.push(*rs.get(*taken as usize)?);
        }
        *taken += 1;
        Some(())
    };
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > MOST_NODES {
            return None;
        }
        match tys.node(t).ok()? {
            tc::PNode::Ref(_, inner) => {
                position(&mut taken, out)?;
                stack.push(inner);
            }
            tc::PNode::Ptr(_, inner) | tc::PNode::Slice(inner) | tc::PNode::Pat(inner, _) | tc::PNode::Array(inner, _) => stack.push(inner),
            // A fn pointer's positions are its inputs' then its output's (lane B15), as a tuple's.
            tc::PNode::Tuple(l) | tc::PNode::FnPtr(l, _) => {
                for &a in tys.list(l).ok()?.iter().rev() {
                    stack.push(a);
                }
            }
            tc::PNode::Adt(k, l) => {
                for _ in 0..world.adt_lifetimes(k)? {
                    position(&mut taken, out)?;
                }
                for &a in tys.list(l).ok()?.iter().rev() {
                    stack.push(a);
                }
            }
            tc::PNode::Dynamic(_) => position(&mut taken, out)?,
            tc::PNode::Param(p) => {
                if regions.is_some() {
                    out.push(FieldRegion::Arg(p));
                }
            }
            tc::PNode::Bool
            | tc::PNode::Char
            | tc::PNode::Int(_)
            | tc::PNode::Uint(_)
            | tc::PNode::Float(_)
            | tc::PNode::Str
            | tc::PNode::Never
            | tc::PNode::FnDef(..)
            | tc::PNode::ConstValue { .. }
            | tc::PNode::ConstParam(_) => {}
            tc::PNode::Alias { .. }
            | tc::PNode::Opaque(..)
            | tc::PNode::Closure { .. }
            | tc::PNode::Future(..)
            | tc::PNode::Coroutine { .. }
            | tc::PNode::Infer
            | tc::PNode::Error
            | tc::PNode::ExTrait(..)
            | tc::PNode::ExProj { .. }
            | tc::PNode::ExAuto(_) => return None,
        }
    }
    // Emitting: every region taken, none left over.
    match regions {
        Some(rs) if rs.len() as u32 != taken => None,
        _ => Some(taken),
    }
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
    // Per argument: the impl's parameter it is (distinct), or a const (asks nothing of `Copy`).
    let mut param_of: AVec<'a, Option<u32>> = AVec::new_in(alloc);
    for &a in args {
        match tys.node(a).ok()? {
            tc::PNode::Param(p) => {
                if param_of.contains(&Some(p)) {
                    return None;
                }
                param_of.push(Some(p));
            }
            tc::PNode::ConstParam(_) | tc::PNode::ConstValue { .. } => param_of.push(None),
            _ => return None,
        }
    }
    let start = need.len();
    for _ in args {
        need.push(false);
    }
    let mut cloned: AVec<'a, usize> = AVec::new_in(alloc);
    let sized = store.sized.map(checker_of);
    let adt_preds = store.preds.get(&adt)?;
    for c in store.preds.get(&imp)?.clauses.iter() {
        let tc::PClause::Trait { trait_, args: cargs } = *c else { return None };
        let &[self_ty] = tys.list(cargs).ok()? else { return None };
        let tc::PNode::Param(p) = tys.node(self_ty).ok()? else { return None };
        let j = param_of.iter().position(|&q| q == Some(p))?;
        if trait_ == copy {
            need[start + j] = true;
        } else if Some(trait_) == sized {
            // Implied by the ADT's own well-formedness only when the ADT bounds its own
            // parameter `j` (`Param(j)` in its record) by `Sized` too.
            let bounded = adt_preds.clauses.iter().any(|ac| match *ac {
                tc::PClause::Trait { trait_: at, args: aargs } if at == trait_ => match tys.list(aargs) {
                    Ok(&[t]) => matches!(tys.node(t), Ok(tc::PNode::Param(q)) if q as usize == j),
                    _ => false,
                },
                _ => false,
            });
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
