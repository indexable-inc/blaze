//! A coroutine's state layout as a pure function of TYPES' sizes (ASYNC.md P8, option (b) as main
//! decided it, 2026-11-16): the upvars first, then one field per saved local of the block's
//! flow (blaze-flow-lower state_recipe.rs), then one drop flag byte per saved local whose type
//! needs drop, then the tag. Made BEFORE any LIR.
//!
//! Each saved local has its OWN offset ([`Laid::field`], [`Laid::flag`]), keyed by its identity
//! in the flow (blaze-flow-lower identity.rs) through [`StateName`]: the builder looks a slot it
//! keeps across a suspension up by that key and the LIR split places the slot at that offset
//! (blaze-lir coroutine_region.rs). The flow layer is the one owner of which locals cross a
//! suspension (main's ruling, async-P8-4); nothing here or in the split decides it again.
//!
//! Order: the upvars at offset 0 (the builder's own prefix, as today); the saved fields by
//! falling alignment, then by their index in the recipe (deterministic: rule 25); the flags in
//! recipe order; the tag last, in the narrowest of `i8`, `i16`, `i32` holding `Unresumed` 0,
//! `Returned` 1, `Panicked` 2 and point `k` as `3 + k` (blaze-lir coroutine.rs's numbering). The
//! Reference leaves the layout of coroutines unspecified; only what is held is its ("Async
//! blocks": "one variant per await point, where each variant stores the data needed to
//! resume").
//!
//! FIRST CUT (stated, owed): no two saved fields share bytes, even when no suspension holds
//! both (the flow-level overlap follow-up, state_recipe.rs's module doc).

use core::alloc::Allocator;

use blaze_flow_lower::identity::Identity;
use blaze_lir::Ty;
use blaze_own::{Alloc, Vec};

use crate::{BuildError, StateShape};

/// The bytes of a state past the upvars and before the tag: the saved fields and the flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateRegion {
    pub start: u32,
    pub end: u32,
    /// The log2 of the alignment `start` has.
    pub align: u8,
}

/// A state laid out from types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LaidState {
    pub shape: StateShape,
    pub region: StateRegion,
    /// The tag's valid range (`Unresumed` 0 to the last point's `2 + points`): what
    /// `LayoutFacts::coroutine_niche` answers, from the types too.
    pub niche: crate::StateNiche,
}

/// [`lay_state`]'s answer: the state, per saved local (the recipe's order) its field's offset,
/// its drop flag's offset (`None`: its type needs no drop), and its 8-byte partial-move
/// field-flags word offset (`rest`), plus per by-value upvar needing a drop flag or field-flags
/// word its offset(s).
pub struct Laid<A: Allocator> {
    pub state: LaidState,
    pub field: Vec<u32, A>,
    pub flag: Vec<Option<u32>, A>,
    pub rest: Vec<Option<u32>, A>,
    pub upvar_flag: Vec<u32, A>,
    pub upvar_rest: Vec<Option<u32>, A>,
}

/// One saved local as the builder looks it up: its identity in the flow, its field's offset
/// and size, its drop flag's offset, and its optional 8-byte field-flags word (`rest`, for a
/// partially moved binding). A table of these is sorted by `identity`, each once
/// (state_recipe.rs refuses a saved identity twice).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StateName {
    pub identity: Identity,
    pub offset: u32,
    pub size: u32,
    pub flag: Option<u32>,
    pub rest: Option<u32>,
}

/// What `build_coroutine` builds a coroutine into: the state laid from types and the table of
/// its saved locals by identity ([`StateNames::new`] checked it sorted and unique once).
#[derive(Clone, Copy)]
pub struct StatePlan<'a> {
    pub laid: LaidState,
    pub names: StateNames<'a>,
    /// A planted bug for this crate's tests; it acts only in this crate's test builds.
    pub plant: StatePlant,
}

/// The `what` of `BuildError::Unsupported` for a slot the walk keeps across a suspension that
/// the flow's table does not name, at the expression whose task made the slot (walk/state_names.rs).
pub const UNNAMED_ACROSS: &str = "a slot this expression made is read after a suspension, and the flow saves no local it names (only flow locals cross a suspension: ASYNC.md P8, async-P8-4); the flow body must name the temporary, or the walk end the slot before the `.await`";

/// A planted bug for the tests' controls (blaze-build walk/state_names.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatePlant {
    None,
    /// Each `.await` makes a slot of the walk's own before its loop and reads it after the
    /// resume: a builder-made slot across a suspension, which the split must refuse.
    BuilderSlot,
}

/// A sorted, unique table of [`StateName`]s: the proof that a lookup has one answer.
#[derive(Clone, Copy)]
pub struct StateNames<'a> {
    rows: &'a [StateName],
}

impl<'a> StateNames<'a> {
    /// `rows`, checked sorted by identity with no identity twice and every field and flag
    /// inside `region`. Refused by name otherwise (a caller's bug: the rows come from one
    /// recipe and one [`lay_state`]).
    pub fn new(rows: &'a [StateName], region: StateRegion) -> Result<StateNames<'a>, BuildError> {
        if rows.windows(2).any(|w| w[0].identity >= w[1].identity) {
            return Err(BuildError::Malformed { what: "a coroutine state's names not sorted by identity, or one identity twice" });
        }
        let inside = |at: u32, len: u32| at >= region.start && u64::from(at) + u64::from(len) <= u64::from(region.end);
        if rows.iter().any(|r| {
            !inside(r.offset, r.size)
                || r.flag.is_some_and(|f| !inside(f, 1))
                || r.rest.is_some_and(|w| !inside(w, 8) || w % 8 != 0)
        }) {
            return Err(BuildError::Malformed { what: "a coroutine state's saved field or flag outside its region" });
        }
        Ok(StateNames { rows })
    }

    /// The saved local of identity `id`, if the flow saves one.
    pub fn get(self, id: Identity) -> Option<StateName> {
        self.rows.binary_search_by_key(&id, |r| r.identity).ok().map(|i| self.rows[i])
    }
}

/// The most alignment (log2) a state field may ask (blaze-lir's `CoroutineError::TooLarge` bound).
const MOST_ALIGN: u8 = 31;

fn align_up(at: u64, align: u8) -> u64 {
    let a = 1u64 << align;
    (at + a - 1) & !(a - 1)
}

/// The state of a coroutine whose upvars' tuple is `env` (size, log2 align), whose saved
/// locals' ground types have the layouts `saved` (size, log2 align; the recipe's order), of
/// which those `needs_flag` marks need drop (one `u8` drop flag each, after the saved fields:
/// main's ruling (ii), blaze-flow-lower state_recipe.rs), and whose body has `points`
/// suspensions; the offsets in `alloc`. Refused by name: `needs_flag` not one row per saved
/// local, an alignment past 2^31, a state past `u32::MAX` bytes, more points than an `i32` tag
/// numbers.
pub fn lay_state<A: Allocator + Copy>(
    env: (u64, u8),
    saved: &[(u64, u8)],
    needs_flag: &[bool],
    points: u32,
    scratch: Alloc<'_>,
    alloc: A,
) -> Result<Laid<A>, BuildError> {
    lay_state_with_upvar_flags(env, saved, needs_flag, 0, points, scratch, alloc)
}

/// [`lay_state`] with `upvar_flags` extra 1-byte drop-flag slots in the state's saved region
/// for by-value upvars whose ground type needs drop (the unresumed path drops them if never
/// polled, while the body owns them once polled and may move them conditionally across a
/// suspension).
pub fn lay_state_with_upvar_flags<A: Allocator + Copy>(
    env: (u64, u8),
    saved: &[(u64, u8)],
    needs_flag: &[bool],
    upvar_flags: usize,
    points: u32,
    scratch: Alloc<'_>,
    alloc: A,
) -> Result<Laid<A>, BuildError> {
    let mut needs_rest: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(saved.len(), scratch);
    needs_rest.resize(saved.len(), false);
    let mut upvar_has_flag: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(upvar_flags, scratch);
    upvar_has_flag.resize(upvar_flags, true);
    let mut upvar_has_rest: Vec<bool, Alloc<'_>> = Vec::with_capacity_in(upvar_flags, scratch);
    upvar_has_rest.resize(upvar_flags, false);
    lay_state_with_flags(env, saved, needs_flag, &needs_rest, &upvar_has_flag, &upvar_has_rest, points, scratch, alloc)
}

/// [`lay_state`] with drop flags (`needs_flag`, 1 byte each) and 8-byte (align 3) partial-move
/// field-flags words (`needs_rest`) for saved locals, plus extra drop flags (`upvar_has_flag`)
/// and field-flags words (`upvar_has_rest`) for by-value upvars not in `saved`.
#[expect(clippy::too_many_arguments, reason = "explicit layout slices keep lay_state_with_flags allocation-free and clear")]
pub fn lay_state_with_flags<A: Allocator + Copy>(
    env: (u64, u8),
    saved: &[(u64, u8)],
    needs_flag: &[bool],
    needs_rest: &[bool],
    upvar_has_flag: &[bool],
    upvar_has_rest: &[bool],
    points: u32,
    scratch: Alloc<'_>,
    alloc: A,
) -> Result<Laid<A>, BuildError> {
    // Built at each use: `BuildError` is not `Clone`.
    let too_big = || BuildError::UnsupportedSig { what: "a coroutine state past 4 GiB" };
    let too_aligned = BuildError::UnsupportedSig { what: "a coroutine state field aligned past 2^31" };
    if needs_flag.len() != saved.len() || needs_rest.len() != saved.len() || upvar_has_flag.len() != upvar_has_rest.len() {
        return Err(BuildError::Malformed { what: "a coroutine state's drop-flag rows not one per saved local" });
    }
    if env.1 > MOST_ALIGN || saved.iter().any(|f| f.1 > MOST_ALIGN) {
        return Err(too_aligned);
    }
    let tags = u64::from(points) + 3;
    // The same widths as blaze-lir coroutine.rs `plan`.
    let (tag_ty, tag_align) = if tags <= 1 << 8 {
        (Ty::I8, 0u8)
    } else if tags <= 1 << 16 {
        (Ty::I16, 1)
    } else if tags <= 1 << 31 {
        (Ty::I32, 2)
    } else {
        return Err(BuildError::UnsupportedSig { what: "a coroutine with more suspensions than an i32 tag numbers" });
    };
    // The saved fields by falling alignment, then recipe index.
    let mut order: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(saved.len(), scratch);
    order.extend(0..saved.len() as u32);
    // Keys are unique (the index is in them): an unstable sort is deterministic.
    order.sort_unstable_by_key(|&i| (core::cmp::Reverse(saved[i as usize].1), i));
    let any_rest = needs_rest.iter().any(|&r| r) || upvar_has_rest.iter().any(|&r| r);
    let region_align = saved.iter().map(|f| f.1).max().unwrap_or(0).max(if any_rest { 3 } else { 0 });
    let start = align_up(env.0, region_align);
    let mut at = start;
    let mut field: Vec<u32, A> = Vec::with_capacity_in(saved.len(), alloc);
    field.resize(saved.len(), 0);
    for &i in order.iter() {
        let (size, align) = saved[i as usize];
        let here = align_up(at, align);
        field[i as usize] = u32::try_from(here).map_err(|_| too_big())?;
        at = here.checked_add(size).ok_or_else(too_big)?;
    }
    // The 8-byte partial-move field-flags words (`rest`, align 3), in recipe then upvar order.
    let mut rest: Vec<Option<u32>, A> = Vec::with_capacity_in(saved.len(), alloc);
    for &needs in needs_rest {
        if needs {
            let here = align_up(at, 3);
            rest.push(Some(u32::try_from(here).map_err(|_| too_big())?));
            at = here.checked_add(8).ok_or_else(too_big)?;
        } else {
            rest.push(None);
        }
    }
    let mut upvar_rest: Vec<Option<u32>, A> = Vec::with_capacity_in(upvar_has_rest.len(), alloc);
    for &needs in upvar_has_rest {
        if needs {
            let here = align_up(at, 3);
            upvar_rest.push(Some(u32::try_from(here).map_err(|_| too_big())?));
            at = here.checked_add(8).ok_or_else(too_big)?;
        } else {
            upvar_rest.push(None);
        }
    }
    // The drop flags, a byte each, in recipe order, followed by upvar drop flags.
    let mut flag: Vec<Option<u32>, A> = Vec::with_capacity_in(saved.len(), alloc);
    for &needs in needs_flag {
        if needs {
            flag.push(Some(u32::try_from(at).map_err(|_| too_big())?));
            at += 1;
        } else {
            flag.push(None);
        }
    }
    let mut upvar_flag: Vec<u32, A> = Vec::with_capacity_in(upvar_has_flag.len(), alloc);
    for &needs in upvar_has_flag {
        if needs {
            upvar_flag.push(u32::try_from(at).map_err(|_| too_big())?);
            at += 1;
        } else {
            upvar_flag.push(u32::MAX);
        }
    }
    let tag_offset = align_up(at, tag_align);
    let align = env.1.max(region_align).max(tag_align);
    let size = align_up(tag_offset + (1u64 << tag_align), align);
    if size > u64::from(u32::MAX) {
        return Err(too_big());
    }
    // Every value below fits: `size` does; `points + 2` does, `tags` fit an i32 tag.
    let shape = StateShape { size: size as u32, align, tag_offset: tag_offset as u32, tag_ty };
    let niche = crate::StateNiche { tag_offset: tag_offset as u32, tag_ty, valid_end: (tags - 1) as u32 };
    let region = StateRegion { start: start as u32, end: tag_offset as u32, align: region_align };
    Ok(Laid { state: LaidState { shape, region, niche }, field, flag, rest, upvar_flag, upvar_rest })
}

/// Conservative syntactic scan over `body` and `checked`: returns a bit per `PatId` that is `true`
/// whenever the walk (`Walk::scan_flags`) may allocate an 8-byte partial-move field-flags word
/// (`rest` / `KeptFlags`) for that binding (field/tuple-field/deref place move, struct update base,
/// match/let destructuring scrutinee, or non-whole closure/async capture).
pub fn may_partial_move_bindings<'a>(body: &blaze_typeck::BodyView<'_>, checked: &blaze_typeck::Checked<'_>, alloc: Alloc<'a>) -> Vec<bool, Alloc<'a>> {
    use blaze_typeck::{Expr, ExprId, Stmt, UnOp};

    let mut out = Vec::with_capacity_in(body.pats.len(), alloc);
    out.resize(body.pats.len(), false);
    let root_local = |mut cur: ExprId| -> Option<blaze_typeck::PatId> {
        for _ in 0..=body.exprs.len() {
            match *body.exprs.get(cur.ix())? {
                Expr::Local(p) => return Some(p),
                Expr::Paren(inner)
                | Expr::Field { base: inner, .. }
                | Expr::TupleField { base: inner, .. }
                | Expr::Unary { op: UnOp::Deref, operand: inner } => cur = inner,
                _ => return None,
            }
        }
        None
    };
    let mark = |out: &mut Vec<bool, Alloc<'a>>, e: ExprId| {
        if let Some(p) = root_local(e)
            && let Some(slot) = out.get_mut(p.ix())
        {
            *slot = true;
        }
    };
    for (i, x) in body.exprs.iter().enumerate() {
        match *x {
            Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Unary { op: UnOp::Deref, operand: base } => {
                mark(&mut out, base);
            }
            Expr::Match { scrutinee, .. } => mark(&mut out, scrutinee),
            Expr::Let { init, .. } => mark(&mut out, init),
            Expr::Struct { base: blaze_typeck::StructBase::Expr(base), .. } | Expr::Record { base: blaze_typeck::StructBase::Expr(base), .. } => mark(&mut out, base),
            Expr::Closure { .. } | Expr::AsyncBlock { .. } => {
                if let Ok(ix) = u32::try_from(i) {
                    for f in checked.env_fields_by::<crate::layout::BuildEnv>(ExprId(ix)) {
                        if !f.is_whole()
                            && let Some(slot) = out.get_mut(f.base.ix())
                        {
                            *slot = true;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for s in body.stmts.iter() {
        if let Stmt::Let { init: Some(init), .. } = *s {
            mark(&mut out, init);
        }
    }
    for (&e, kind) in checked.adjusts.column::<0>().iter().zip(checked.adjusts.column::<1>().iter()) {
        if matches!(kind, blaze_typeck::AdjustKind::Deref { .. }) {
            mark(&mut out, e);
        }
    }
    out
}

/// Conservative syntactic scan over `body`: returns a bit per `ExprId` that is `true` whenever
/// the walk (`own_scrutinee`, `move_temp_field`, `move_temp_box`, `take_temp_field`) may allocate
/// an 8-byte partial-move field-flags word (`rest` / `Held::Moved`) for a temporary value of that
/// expression.
pub fn may_partial_move_temps<'a>(body: &blaze_typeck::BodyView<'_>, alloc: Alloc<'a>) -> Vec<bool, Alloc<'a>> {
    use blaze_typeck::{Expr, ExprId, Pat, Stmt, UnOp};

    let mut out = Vec::with_capacity_in(body.exprs.len(), alloc);
    out.resize(body.exprs.len(), false);
    let peel_parens = |mut cur: ExprId| -> ExprId {
        for _ in 0..=body.exprs.len() {
            match body.exprs.get(cur.ix()) {
                Some(&Expr::Paren(inner)) => cur = inner,
                _ => break,
            }
        }
        cur
    };
    let root_temp = |mut cur: ExprId| -> Option<ExprId> {
        for _ in 0..=body.exprs.len() {
            match *body.exprs.get(cur.ix())? {
                Expr::Local(_) => return None,
                Expr::Paren(inner)
                | Expr::Field { base: inner, .. }
                | Expr::TupleField { base: inner, .. }
                | Expr::Unary { op: UnOp::Deref, operand: inner } => cur = inner,
                _ => return Some(cur),
            }
        }
        None
    };
    let mark_expr = |out: &mut Vec<bool, Alloc<'a>>, e: ExprId| {
        if let Some(slot) = out.get_mut(e.ix()) {
            *slot = true;
        }
    };
    for x in body.exprs.iter() {
        match *x {
            Expr::Match { scrutinee, .. } => mark_expr(&mut out, peel_parens(scrutinee)),
            Expr::Let { init, .. } => mark_expr(&mut out, peel_parens(init)),
            Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Unary { op: UnOp::Deref, operand: base } => {
                if let Some(root) = root_temp(base) {
                    mark_expr(&mut out, root);
                }
            }
            Expr::Struct { base: blaze_typeck::StructBase::Expr(base), .. }
            | Expr::Record { base: blaze_typeck::StructBase::Expr(base), .. } => {
                mark_expr(&mut out, peel_parens(base));
                if let Some(root) = root_temp(base) {
                    mark_expr(&mut out, root);
                }
            }
            _ => {}
        }
    }
    for s in body.stmts.iter() {
        if let Stmt::Let { pat, init: Some(init), else_, .. } = *s {
            let plain = matches!(body.pats.get(pat.ix()), Some(Pat::Bind { sub: None, .. } | Pat::Wild));
            if else_.is_some() || !plain {
                mark_expr(&mut out, peel_parens(init));
            }
        }
    }
    out
}
