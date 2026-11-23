//! Auto traits of ADTs by a summary per (ADT, trait), once per solver (one body): what
//! `ADT<P..>: Auto` comes down to once every structural step through the ADT's fields is taken.
//!
//! The rule it shortcuts is `consider_auto_trait_candidate` (structural.rs `step_auto`): an ADT
//! with no impl of the auto trait that may apply holds it when each field type does, and a field
//! type holds it structurally in turn (a tuple when its elements do, a reference or array when
//! its element does, an inner ADT when its fields do) down to the ADT's own parameters. Before,
//! each of these steps was a goal of its own, with its own frame, imports and read list, once
//! per distinct instantiation: `N5<K0>: Send` .. `N5<K99>: Send` each walked every field of
//! every level (the busy thread's profile of tempo's check on 2026-11-17: `structural::step`
//! 21%, `builtin::adt_fields` 19%). The summary walks the ADT's portable field types once and
//! keeps, per (ADT, trait):
//!
//! - `params`: the parameters whose goals remain (`P_i: Auto`), sorted;
//! - `opaque`: the fields it could not see through, whose goals are added as before;
//! - `reads`: every fact the walk read, replayed into the goal's read list at each use (the
//!   answer is valid by them: rule 21).
//!
//! A step is taken in the summary only where the solver would take exactly it: the structural
//! candidate is the type's only candidate, which holds when no impl of the trait may apply to
//! the type's shape ([`crate::structural::possible_impl`], the same test as assembly's), the
//! goal is in `TypingMode::Analysis` (coherence adds the unknowable candidate) and no
//! where-clause of the environment names the trait (a param-env candidate would compete). The
//! caller checks the last two. Anything else (an impl that may apply, a parameterless leaf
//! with a negative impl, an alias, a closure, a pattern type, PhantomData's argument, an ADT
//! already being summarized) leaves its field opaque: its goal is solved as before, so a cycle
//! through an ADT's own fields is the search graph's coinductive cycle, as the Reference has
//! it, and polymorphic recursion overflows where it did.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, Vec};

use crate::assembly::Cx;
use crate::error::{Limit, SolveError, Stop};
use crate::facts::{AdtFlags, Facts, Portable};
use crate::term::{Def, List, Ty, TyKind};
use crate::tys::Tys;

/// How deep summaries nest (an ADT's field naming an ADT whose field ..): past it a field is
/// opaque (rule 18).
pub const MAX_NEST: usize = 32;

/// One summary's ranges into the pools.
#[derive(Clone, Copy, Debug)]
struct Row {
    params: (u32, u32),
    opaque: (u32, u32),
    reads: (u32, u32),
}

/// The summaries of one solver.
pub struct Summaries<'a> {
    by: Map<(Def, Def), u32, FixedState, Alloc<'a>>,
    rows: Vec<Row, Alloc<'a>>,
    params: Vec<u32, Alloc<'a>>,
    opaque: Vec<u32, Alloc<'a>>,
    reads: Vec<u32, Alloc<'a>>,
    /// The ADTs being summarized, outermost first.
    stack: Vec<Def, Alloc<'a>>,
}

/// A summary, borrowed: see the module doc.
pub struct Summary<'s> {
    pub params: &'s [u32],
    pub opaque: &'s [u32],
    pub reads: &'s [u32],
}

impl<'a> Summaries<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Summaries<'a> {
        Summaries {
            by: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            rows: Vec::new_in(alloc),
            params: Vec::new_in(alloc),
            opaque: Vec::new_in(alloc),
            reads: Vec::new_in(alloc),
            stack: Vec::new_in(alloc),
        }
    }

    pub fn get(&self, row: u32) -> Summary<'_> {
        let r = self.rows[row as usize];
        let range = |(a, l): (u32, u32)| a as usize..(a + l) as usize;
        Summary { params: &self.params[range(r.params)], opaque: &self.opaque[range(r.opaque)], reads: &self.reads[range(r.reads)] }
    }
}

/// The summary of `d: trait_`, computed on first use; `None` while `d` is being summarized
/// (a field naming its own ADT) or past [`MAX_NEST`]. The reads of the walk are in the open
/// segment of the read log either way, as if read by the caller.
pub fn summary<T: Tys, F: Facts>(cx: &mut Cx<'_, '_, T, F>, d: Def, trait_: Def) -> Result<Option<u32>, Stop> {
    if let Some(&row) = cx.scratch.auto.by.get(&(d, trait_)) {
        let lo = cx.reads.lo;
        let Row { reads: (a, l), .. } = cx.scratch.auto.rows[row as usize];
        cx.reads.seg.extend(lo, &cx.scratch.auto.reads[a as usize..(a + l) as usize])?;
        return Ok(Some(row));
    }
    if cx.scratch.auto.stack.contains(&d) || cx.scratch.auto.stack.len() >= MAX_NEST {
        return Ok(None);
    }
    // The walk's reads in a segment of their own, folded into the caller's after.
    let outer = cx.reads.lo;
    let sub = crate::error::row(cx.reads.seg.len())?;
    cx.reads.lo = sub;
    cx.scratch.auto.stack.push(d);
    let mut params: Vec<u32, Alloc<'_>> = Vec::new_in(cx.alloc);
    let mut opaque: Vec<u32, Alloc<'_>> = Vec::new_in(cx.alloc);
    let r = walk_adt(cx, d, trait_, &mut params, &mut opaque);
    cx.scratch.auto.stack.pop();
    let row = match r {
        Ok(()) => {
            params.sort_unstable();
            params.dedup();
            let a = &mut cx.scratch.auto;
            let p = (crate::error::row(a.params.len())?, params.len() as u32);
            a.params.extend_from_slice(&params);
            let o = (crate::error::row(a.opaque.len())?, opaque.len() as u32);
            a.opaque.extend_from_slice(&opaque);
            let ids = cx.reads.seg.from(sub as usize);
            let rd = (crate::error::row(a.reads.len())?, ids.len() as u32);
            a.reads.extend_from_slice(ids);
            let row = crate::error::row(a.rows.len())?;
            a.rows.push(Row { params: p, opaque: o, reads: rd });
            a.by.insert((d, trait_), row);
            cx.scratch.stats.auto_summaries += 1;
            Ok(Some(row))
        }
        Err(e) => Err(e),
    };
    cx.reads.seg.close(outer, sub);
    cx.reads.lo = outer;
    row
}

/// Walks `d`'s fields: each field's remaining parameters into `params`, or its index into
/// `opaque`.
fn walk_adt<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    d: Def,
    trait_: Def,
    params: &mut Vec<u32, Alloc<'_>>,
    opaque: &mut Vec<u32, Alloc<'_>>,
) -> Result<(), Stop> {
    let adt = cx.adt(d)?;
    let mut work: Vec<Ty, Alloc<'_>> = Vec::new_in(cx.alloc);
    let mut found: Vec<u32, Alloc<'_>> = Vec::new_in(cx.alloc);
    for (j, &f) in adt.fields.iter().enumerate() {
        cx.scratch.stats.summary_fields += 1;
        found.clear();
        if walk_field(cx, adt.portable, f, trait_, &mut work, &mut found)? {
            params.extend_from_slice(&found);
        } else {
            opaque.push(crate::error::row(j)?);
        }
    }
    Ok(())
}

/// Whether the portable type `root` of `p` holds `trait_` when the parameters it pushes onto
/// `found` do (`false`: the field is opaque).
fn walk_field<T: Tys, F: Facts>(
    cx: &mut Cx<'_, '_, T, F>,
    p: Portable<'_>,
    root: Ty,
    trait_: Def,
    work: &mut Vec<Ty, Alloc<'_>>,
    found: &mut Vec<u32, Alloc<'_>>,
) -> Result<bool, Stop> {
    work.clear();
    work.push(root);
    let mut visited = 0usize;
    let node = |t: Ty| p.nodes.get(t.0 as usize).copied();
    let elem = |l: List, i: u32| p.lists.get((l.start + i) as usize).copied();
    while let Some(t) = work.pop() {
        visited += 1;
        if visited > crate::fold::MAX_WALK {
            return Err(Stop::Error(SolveError::Limit(Limit::Walk)));
        }
        let Some(k) = node(t) else { return Err(crate::builtin::malformed()) };
        if let TyKind::Param(i) = k {
            found.push(i);
            continue;
        }
        // The shape's bucket: a step is taken only where the structural candidate is the
        // only one (assembly's test).
        let first = |l: List| if l.len > 0 { elem(l, 0).and_then(node) } else { None };
        let Some(shape) = crate::object_shape::shape_with_principal(k, first) else { return Ok(false) };
        match k {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Never
            | TyKind::Str
            | TyKind::FnDef(..)
            | TyKind::FnPtr(..) => {
                if crate::structural::possible_impl(cx, trait_, shape, None)? {
                    return Ok(false);
                }
                // `str` is `[u8]`: those two shapes too.
                if k == TyKind::Str
                    && (crate::structural::possible_impl(cx, trait_, crate::term::Shape::Slice, None)?
                        || crate::structural::possible_impl(cx, trait_, crate::term::Shape::Uint(crate::term::UintTy::U8), None)?)
                {
                    return Ok(false);
                }
            }
            TyKind::Ref(_, e, _) | TyKind::RawPtr(e, _) | TyKind::Array(e, _) | TyKind::Slice(e) => {
                if crate::structural::possible_impl(cx, trait_, shape, None)? {
                    return Ok(false);
                }
                work.push(e);
            }
            TyKind::Tuple(l) | TyKind::Record(_, l) | TyKind::Union(l) => {
                if crate::structural::possible_impl(cx, trait_, shape, None)? {
                    return Ok(false);
                }
                for i in 0..l.len {
                    let Some(x) = elem(l, i) else { return Err(crate::builtin::malformed()) };
                    work.push(x);
                }
            }
            TyKind::Adt(d2, args) => {
                if crate::structural::possible_impl(cx, trait_, shape, None)? {
                    return Ok(false);
                }
                if cx.adt(d2)?.flags.has(AdtFlags::PHANTOM_DATA) {
                    return Ok(false);
                }
                let Some(row) = summary(cx, d2, trait_)? else { return Ok(false) };
                let s = cx.scratch.auto.get(row);
                if !s.opaque.is_empty() {
                    return Ok(false);
                }
                for &i in s.params {
                    if i >= args.len {
                        return Err(crate::builtin::malformed());
                    }
                    let Some(x) = elem(args, i) else { return Err(crate::builtin::malformed()) };
                    work.push(x);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(true)
}
