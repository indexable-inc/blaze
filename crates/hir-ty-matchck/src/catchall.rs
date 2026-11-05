//! What each catch-all of a `match` stands for: a `_` or a binding with no subpattern, at a
//! place whose type is an enum, and the variants of that enum it is the arm for.
//!
//! The arms above the catch-all's arm are analyzed alone, guards as rustc takes them (a guarded
//! arm covers nothing). The witnesses of that analysis are every value those arms leave, so the
//! catch-all stands for the variants that the witnesses its arm's pattern can match have at its
//! place. A witness that is `_` there (rustc reports `_` where no arm names a constructor of the
//! place) stands for every inhabited variant. Where it can't tell two constructors apart (two
//! integer ranges), the pattern is taken to meet the witness, so the list may name a variant the
//! arms above already cover, never leave out one they don't.
//!
//! Used by `blaze::wildcard_arm`, which lists the variants in place of the catch-all; nothing
//! in compilation reads it.

use base_db::Db;
use hir_def::{
    AdtId, DefWithBodyId, EnumId, EnumVariantId, HasModule,
    expr_store::Body,
    hir::{Expr, ExprId, MatchArm, Pat as HirPat, PatId},
    unstable_features::UnstableFeatures,
};
use rustc_pattern_analysis::{
    PatCx as _,
    constructor::{Constructor, ConstructorSet, VariantVisibility},
};
use rustc_type_ir::inherent::IntoKind as _;
use smallvec::SmallVec;

use hir_ty::{
    InferenceResult,
    db::HirDatabase,
    next_solver::{DbInterner, Ty, TyKind, TypingMode, infer::DbInternerInferExt},
};

use crate::{
    cx::{Arm, Cx, Pat, VariantIdx, Witness},
    lower::Lowerer,
    visit::known_valid_scrutinee,
};

/// How deep a pattern and a witness are walked together.
const DEPTH: u32 = 64;

/// One catch-all of a `match` arm whose place is an enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CatchAll {
    pub match_expr: ExprId,
    /// The pattern of the arm the catch-all is in.
    pub arm_pat: PatId,
    /// The `_` or the binding.
    pub pat: PatId,
    /// The arm has a guard.
    pub guarded: bool,
    /// The enum at the catch-all's place, through references.
    pub enum_: EnumId,
    /// The variants of `enum_` the catch-all stands for, in declaration order; empty when the
    /// arms above cover every value it can match.
    pub variants: Box<[EnumVariantId]>,
}

/// The catch-alls of a body's `match`es, desugared ones (`for`, `?`) included, in the order
/// the body's expressions and each arm's patterns are walked.
#[derive(Debug, Default, PartialEq, Eq, Hash)]
pub struct CatchAlls {
    pub arms: Box<[CatchAll]>,
    /// A `match` of the body was left out: a pattern that does not lower, or an analysis that
    /// stopped (an error rustc reports, or the complexity limit).
    pub incomplete: bool,
}

/// A catch-all found before the analysis: its match, its arm's index, the pattern, the enum.
struct Candidate {
    match_expr: ExprId,
    scrutinee: ExprId,
    arm: usize,
    pat: PatId,
    enum_: EnumId,
}

/// The enum `ty` is, through references.
fn enum_of(ty: Ty<'_>) -> Option<EnumId> {
    match ty.strip_references().kind() {
        TyKind::Adt(def, _) => match def.def_id() {
            AdtId::EnumId(it) => Some(it),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn catch_alls<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    infer: &'db InferenceResult<'db>,
) -> CatchAlls {
    let body = Body::of(db, def);
    // alloc: the catch-alls of one body, a lint's query only
    let mut candidates: Vec<Candidate> = Vec::new();
    for (id, expr) in body.exprs() {
        let Expr::Match { expr: scrutinee, arms } = expr else { continue };
        for (i, arm) in arms.iter().enumerate() {
            body.walk_pats(arm.pat, &mut |pat| {
                if matches!(body.pat(pat), HirPat::Wild | HirPat::Bind { subpat: None, .. })
                    && let Some(enum_) = enum_of(infer.pat_ty(pat))
                {
                    candidates.push(Candidate { match_expr: id, scrutinee, arm: i, pat, enum_ });
                }
            });
        }
    }
    if candidates.is_empty() {
        return CatchAlls::default();
    }

    let krate = def.krate(db);
    let Ok(target) = db.target_data_layout(krate) else {
        return CatchAlls { incomplete: true, ..CatchAlls::default() };
    };
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::typeck_for_body(interner, def.into()));
    // With what the body's `for const` loops assume of their binders, as inference had it.
    let env =
        hir_ty::comptime_env::body_env(&infcx, infer, db.trait_environment(def.generic_def(db)));
    let features = UnstableFeatures::query(db, krate);
    let cx = Cx {
        db,
        infcx: &infcx,
        env,
        module: def.module(db),
        krate,
        pointer_bits: target.pointer_size().bits(),
        exhaustive_patterns: features.exhaustive_patterns,
        deref_patterns: features.deref_patterns,
        never_patterns: features.never_patterns,
        // Each witness left by the arms above a catch-all is a value it takes: with rustc's
        // relevant witnesses only, `Some(_)` after `Some(A)` would take none of `Some(B)`.
        exhaustive_witnesses: true,
        has_deref_pat: std::cell::Cell::new(false),
        range_lints: std::cell::RefCell::new(Vec::new()), // alloc: none kept, cleared per analysis
    };
    let lower = Lowerer::new(&cx, &body, infer, def);

    // alloc: the answer
    let mut found: Vec<CatchAll> = Vec::new();
    let mut incomplete = false;
    let mut at = 0;
    while at < candidates.len() {
        let match_expr = candidates[at].match_expr;
        let end = candidates[at..]
            .iter()
            .position(|it| it.match_expr != match_expr)
            .map_or(candidates.len(), |it| at + it);
        let group = &candidates[at..end];
        at = end;
        let Expr::Match { arms, .. } = body.expr(match_expr) else { continue };
        match check_match(&cx, &lower, &body, infer, group, arms, &mut found) {
            Some(()) => {}
            None => incomplete = true,
        }
    }
    CatchAlls { arms: found.into_boxed_slice(), incomplete }
}

/// The catch-alls of one `match`, `group` all of it; `None` when the match can't be analyzed.
fn check_match<'a, 'db>(
    cx: &Cx<'a, 'db>,
    lower: &Lowerer<'_, 'a, 'db>,
    body: &Body,
    infer: &'db InferenceResult<'db>,
    group: &[Candidate],
    arms: &[MatchArm],
    found: &mut Vec<CatchAll>,
) -> Option<()> {
    let first = group.first()?;
    lower.scope.set(first.match_expr);
    cx.has_deref_pat.set(false);
    // alloc: the lowered arms of one match
    let mut pats: Vec<Pat<'a, 'db>> = Vec::with_capacity(arms.len());
    for arm in arms {
        pats.push(lower.lower(arm.pat).ok()?);
    }
    let scrut_ty = infer.type_of_expr_with_adjust(first.scrutinee)?;
    let known_valid = known_valid_scrutinee(body, infer, Some(lower), first.scrutinee);
    // The arms above each catch-all's arm, analyzed once per arm.
    let mut last: Option<(usize, SmallVec<[Witness<'a, 'db>; 8]>)> = None;
    for candidate in group {
        let i = candidate.arm;
        if last.as_ref().is_none_or(|(arm, _)| *arm != i) {
            let above: Vec<Arm<'_, 'a, 'db>> = pats[..i]
                .iter()
                .zip(arms)
                .enumerate()
                .map(|(j, (pat, arm))| rustc_pattern_analysis::MatchArm {
                    pat,
                    has_guard: arm.guard.is_some(),
                    arm_data: j as u32,
                })
                .collect(); // alloc: the arms above one catch-all's arm
            let report = cx.analyze(&above, scrut_ty, known_valid);
            cx.range_lints.borrow_mut().clear();
            let report = report.ok()?;
            last = Some((i, report.non_exhaustiveness_witnesses.into_iter().collect()));
        }
        let Some((_, witnesses)) = &last else { continue };
        // alloc: the variant indices of one catch-all
        let mut covered: Vec<u32> = Vec::new();
        for witness in witnesses {
            let mark = covered.len();
            if !meet(cx, &pats[i], witness, candidate.pat, &mut covered, DEPTH) {
                covered.truncate(mark);
            }
        }
        covered.sort_unstable();
        covered.dedup();
        let variants =
            covered.into_iter().map(|idx| VariantIdx(idx).id(cx.db, candidate.enum_)).collect(); // alloc: the answer's variants
        found.push(CatchAll {
            match_expr: candidate.match_expr,
            arm_pat: arms[i].pat,
            pat: candidate.pat,
            guarded: arms[i].guard.is_some(),
            enum_: candidate.enum_,
            variants,
        });
    }
    Some(())
}

/// Whether `pat` can match a value `witness` stands for, adding to `out` the variants the
/// catch-all `target` in `pat` takes from it.
fn meet<'a, 'db>(
    cx: &Cx<'a, 'db>,
    pat: &Pat<'a, 'db>,
    witness: &Witness<'a, 'db>,
    target: PatId,
    out: &mut Vec<u32>,
    depth: u32,
) -> bool {
    if depth == 0 {
        // Too deep to tell: the pattern may meet it, and `target` may take any variant.
        take_all_under(cx, pat, target, out);
        return true;
    }
    match pat.ctor() {
        Constructor::Wildcard => {
            if *pat.data() == target {
                take(cx, witness, out, depth);
            }
            return true;
        }
        Constructor::Or => {
            let mut any = false;
            for alt in pat.iter_fields() {
                let mark = out.len();
                if meet(cx, &alt.pat, witness, target, out, depth - 1) {
                    any = true;
                } else {
                    out.truncate(mark);
                }
            }
            return any;
        }
        _ => {}
    }
    match witness.ctor() {
        // Every value of the place: the pattern meets it, and a catch-all under it takes all.
        Constructor::Wildcard | Constructor::NonExhaustive | Constructor::Hidden => {
            take_all_under(cx, pat, target, out);
            return true;
        }
        _ => {}
    }
    // A slice's fields are indexed by its kind (`[a, .., z]` against `[a, b, c]`), which this
    // walk doesn't line up: a catch-all in it may take any variant.
    if matches!(pat.ctor(), Constructor::Slice(_))
        || matches!(witness.ctor(), Constructor::Slice(_))
    {
        take_all_under(cx, pat, target, out);
        return true;
    }
    if !ctors_meet(pat.ctor(), witness.ctor()) {
        return false;
    }
    let fields: SmallVec<[&Witness<'a, 'db>; 4]> = witness.iter_fields().collect();
    for field in pat.iter_fields() {
        let Some(&sub) = fields.get(field.idx) else { continue };
        if !meet(cx, &field.pat, sub, target, out, depth - 1) {
            return false;
        }
    }
    true
}

/// Whether two constructors can match one value; `true` where that isn't told apart here.
fn ctors_meet<'a, 'db>(pat: &Constructor<Cx<'a, 'db>>, witness: &Constructor<Cx<'a, 'db>>) -> bool {
    match (pat, witness) {
        (Constructor::Variant(a), Constructor::Variant(b)) => a == b,
        (Constructor::Bool(a), Constructor::Bool(b)) => a == b,
        _ => true,
    }
}

/// Every variant for the catch-all `target`, where it is inside `pat`.
fn take_all_under<'a, 'db>(
    cx: &Cx<'a, 'db>,
    pat: &Pat<'a, 'db>,
    target: PatId,
    out: &mut Vec<u32>,
) {
    pat.walk(&mut |inner| {
        if matches!(inner.ctor(), Constructor::Wildcard) && *inner.data() == target {
            every_variant(cx, *inner.ty(), out);
        }
        true
    });
}

/// The variants of the enum the witness at a catch-all's place names, through references.
fn take<'a, 'db>(cx: &Cx<'a, 'db>, witness: &Witness<'a, 'db>, out: &mut Vec<u32>, depth: u32) {
    let mut witness = witness;
    for _ in 0..depth {
        match witness.ctor() {
            Constructor::Ref => match witness.iter_fields().next() {
                Some(inner) => witness = inner,
                None => return,
            },
            Constructor::Variant(idx) => {
                out.push(idx.0);
                return;
            }
            Constructor::Wildcard | Constructor::NonExhaustive | Constructor::Hidden => {
                every_variant(cx, *witness.ty(), out);
                return;
            }
            _ => return,
        }
    }
}

/// Every inhabited variant of the enum `ty` is, through references.
fn every_variant<'db>(cx: &Cx<'_, 'db>, ty: Ty<'db>, out: &mut Vec<u32>) {
    let ty = ty.strip_references();
    let Ok(ConstructorSet::Variants { variants, .. }) = cx.ctors_for_ty(&ty) else { return };
    for (idx, visibility) in variants.iter().enumerate() {
        if !matches!(visibility, VariantVisibility::Empty) {
            out.push(idx as u32);
        }
    }
}
