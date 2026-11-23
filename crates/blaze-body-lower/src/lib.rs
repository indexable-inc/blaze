//! The one lowering (docs/v2/plan/design/backend-next.md, "One lowering", D119): a checked body
//! (`BodyView` and its `Checked` record) to a [`blaze_body::Body`].
//!
//! - Generic: every type is the checker's [`PTy`], never substituted.
//! - Complete: where blaze-flow-lower abstracts a value away, the body holds the row
//!   blaze-build's walk writes (a constant, an operand, a callee, an aggregate, a cast).
//! - Unwind from the start: every call, drop and assert names its unwind edge, and the unwind
//!   path is cleanup blocks of plain `Drop` rows ending in `resume` ([`scope`]).
//! - Drops are un-elaborated: a `Drop` row means "drop if initialized"; drop elaboration is a
//!   later pass (D119 P4).
//!
//! The Reference decides evaluation order, temporaries and drop order; each module quotes the
//! rule it follows. A construct not lowered yet is a typed refusal ([`LowerError`]) naming the
//! expression or pattern and the construct: never a panic, never a dropped row.
//!
//! - [`scope`]: the drop schedule, scope exits and the unwind chain.
//! - [`expr`]: values, places, operands, calls and operators.
//! - [`control`]: blocks, statements, `if`, `loop`, `break`, `continue`, `return`.
//! - [`pat`]: patterns: tests (`match`, `if let`) and bindings (`let`, parameters, arms).
//! - [`closure`]: closure values and closure bodies with their captures as places.

#![no_std]
#![forbid(unsafe_code)]
#![feature(allocator_api)]

#[cfg(test)]
extern crate std;

mod closure;
mod control;
mod expr;
mod pat;
mod scope;

#[cfg(test)]
mod tests;

use core::alloc::Allocator;
use core::cell::Cell;

use blaze_body::{Block, BlockKind, Body, BodyBuilder, BuildError, Local, LocalDecl, LocalKind, Loan, NONE, Place, Span, StmtKind};
use blaze_flow_lower::{
    AdtCopyArgs, ClosureCopy, ClosureDrop, CopyWalk, CopyWalkError, DropWalk, DropWalkError, Facts, index_rows,
    pty_is_copy, pty_needs_drop,
};
use blaze_own::Vec;
use blaze_typeck::{
    AdjustKind, BodyView, Checked, Def, Expr, ExprId, IntTy, ItemKey, List, Mutbl, PNode, PTy, PTys, Pat, PatId, Range,
    UintTy,
};

pub use closure::lower_closure;

use scope::{Entry, Pending, Target};

/// Most positions a type walk (`is_copy`, `needs_drop`) visits before it refuses by name.
const MOST_POSITIONS: u32 = 4096;

/// Most nested expressions the lowering descends through before it refuses by name.
const MOST_DEPTH: u32 = 512;

/// What a refusal points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    Expr(ExprId),
    Pat(PatId),
    /// The body as a whole (a builder error at the end).
    Body,
}

/// A construct the lowering does not take yet, or a fact it lacks: named, never guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Construct {
    /// The record does not hold what the body names (an id past its table, a missing row).
    Malformed,
    /// A type walk passed [`MOST_POSITIONS`].
    Bound,
    /// Expressions nested past [`MOST_DEPTH`].
    Depth,
    /// An async fn or coroutine body.
    Coroutine,
    AsyncBlock,
    Await,
    InlineAsm,
    FormatArgs,
    OffsetOf,
    /// A literal whose value [`BodyFacts`] did not give.
    NoLiteralValue,
    /// A C string literal.
    CStr,
    /// `isize`/`usize` arithmetic whose bound needs the target's pointer width.
    PointerWidth,
    /// Whether a callee is a trait's method ([`BodyFacts::trait_item`]) is not known.
    CalleeKind,
    /// A call of a value that is no fn item and no fn pointer (a closure, a `dyn Fn`, an `F: Fn`).
    CallThroughFnTrait,
    /// An overloaded deref (`Deref::deref`), written or by an adjustment.
    OverloadedDeref,
    /// An overloaded index (`Index::index`, `IndexMut::index_mut`).
    OverloadedIndex,
    Union,
    LetElse,
    SuperLet,
    /// `let` inside a condition chain, or outside `if`.
    LetChain,
    OrPattern,
    SlicePattern,
    RangePattern,
    /// A float or string literal pattern, or a constant used as a pattern.
    ValuePattern,
    /// A refutable pattern where only an irrefutable one is taken (`let`, a parameter).
    Refutable,
    /// A guard over an arm that binds a non-`Copy` value by move.
    GuardBindsByMove,
    /// An assignment whose left side is a pattern (`(a, b) = ..`).
    DestructuringAssign,
    /// A cast the kinds below do not name (an enum to an integer, ..).
    Cast,
    /// The field a field expression or pattern names is not known.
    UnknownField,
    /// The discriminant of a variant ([`BodyFacts::discr`]) is not known.
    Discriminant,
    /// `break` or `continue` whose target is no enclosing loop or block.
    Target,
    /// `_` outside an assignment's left side.
    Underscore,
    /// A path the record resolves to no value (a trait, a type).
    PathValue,
    /// A closure environment field holding a captured place below its binding (`x.a`,
    /// `(*r).b`: RFC 2229, blaze-typeck env_fields.rs): not lowered here yet.
    CapturedPlace,
    /// The builder refused a row (blaze-body's [`BuildError`]).
    Build(BuildError),
}

/// Why a body was not lowered: the construct and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowerError {
    pub construct: Construct,
    pub at: At,
    /// The item-relative range of `at`.
    pub range: Range,
}

/// The integer type of an enum's discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscrInt {
    Int(IntTy),
    Uint(UintTy),
}

/// A variant's discriminant: its integer type and its value's bits, little-endian, masked to
/// the type's width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Discr {
    pub int: DiscrInt,
    pub bits: u128,
}

/// The facts the checked record does not hold and the full body needs, beside blaze-flow-lower's
/// [`Facts`]: literal values (as blaze-build's `Adts` gives them), discriminants and the target's
/// pointer width. `None` is "not known": the construct is refused by name, never guessed.
pub trait BodyFacts: Facts {
    /// The value of integer literal `e`, its bits masked to its type's width.
    fn int_lit(&self, _e: ExprId) -> Option<u128> {
        None
    }
    /// The bits of float literal `e` in its type's format.
    fn float_lit(&self, _e: ExprId) -> Option<u128> {
        None
    }
    /// The scalar value of char literal `e`.
    fn char_lit(&self, _e: ExprId) -> Option<u32> {
        None
    }
    /// The bytes of string or byte-string literal `e`.
    fn str_lit(&self, _e: ExprId) -> Option<&[u8]> {
        None
    }
    /// The bits of integer or char literal pattern `p` (a `NegLit` already negated, masked to
    /// the type's width).
    fn int_pat(&self, _p: PatId) -> Option<u128> {
        None
    }
    /// The discriminant of `variant` of enum `adt`.
    fn discr(&self, _adt: ItemKey, _variant: u32) -> Option<Discr> {
        None
    }
    /// The target's pointer width in bits, for `isize` bounds.
    fn pointer_bits(&self) -> Option<u32> {
        None
    }
    /// Whether `item` is a trait's method (its call is `Callee::Method`, resolved per
    /// instance) or not (`Callee::FnItem`).
    fn trait_item(&self, item: ItemKey) -> Option<bool>;
}

/// Lowers the body of a fn (`body`, checked as `ck`) to a [`Body`] in `alloc`.
///
/// Locals are the return place `_0`, then one argument per parameter, then bindings and
/// temporaries. A parameter whose pattern is a plain by-value binding is that binding; any
/// other parameter pattern binds from its argument at entry.
pub fn lower<A: Allocator + Copy, F: BodyFacts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Body<A>, LowerError> {
    if body.coroutine.is_some() {
        return Err(LowerError { construct: Construct::Coroutine, at: At::Body, range: range_of(body, body.root) });
    }
    let mut l = Lower::new(body, ck, facts, alloc);
    l.fn_frame()?;
    let root = body.root;
    l.finish(root)
}

/// Why a checked body was not kept (run.md R4): its lowering refused a construct, or the kept
/// record refused the body (blaze-body kept.rs). The caller names the body's item (rule 19).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeepError {
    Lower(LowerError),
    Kept(blaze_body::KeptError),
}

/// The kept record of a fn body checked clean as `ck` (run.md R4, D119): [`lower`]'s generic
/// body written with the table its handles index, `ck`'s own (`ck.types.view()`, the table the
/// lowering built against), as one blaze-body kept record in `alloc`. What the Bodies stage
/// leaves beside the facts record; a run builds instances from it (`blaze_body_build::build`
/// over `blaze_body::kept::open`'s `verified()` and `tys()`) and lowers nothing again.
///
/// Two bodies whose verdicts agree but whose code differs (`return 1`, `return 2`) keep two
/// records: the record is the code, not the verdict.
pub fn keep<A: Allocator + Copy, F: BodyFacts>(
    body: &BodyView<'_>,
    ck: &Checked<'_>,
    facts: &F,
    alloc: A,
) -> Result<Vec<u8, A>, KeepError> {
    let lowered = lower(body, ck, facts, alloc).map_err(KeepError::Lower)?;
    blaze_body::kept::encode(&lowered, ck.types.view(), alloc).map_err(KeepError::Kept)
}

fn range_of(body: &BodyView<'_>, e: ExprId) -> Range {
    body.expr_ranges.get(e.ix()).copied().unwrap_or(Range { start: 0, end: 0 })
}

fn pat_range_of(body: &BodyView<'_>, p: PatId) -> Range {
    body.pat_ranges.get(p.ix()).copied().unwrap_or(Range { start: 0, end: 0 })
}

/// The lowering's state over one body.
pub(crate) struct Lower<'b, 'v, 'c, F, A: Allocator + Copy> {
    pub(crate) body: &'b BodyView<'v>,
    pub(crate) ck: &'b Checked<'c>,
    pub(crate) tys: PTys<'b>,
    pub(crate) facts: &'b F,
    pub(crate) alloc: A,
    pub(crate) b: BodyBuilder<A>,
    /// The return place `_0` and its type.
    pub(crate) ret: Place,
    pub(crate) ret_ty: PTy,
    /// Each binding pattern's place (indexed by `PatId`, `Place(NONE)` until bound):
    /// its local's, or a closure's upvar place.
    pub(crate) pat_place: Vec<Place, A>,
    /// Dense O(1) first-match lookup tables into `ck` columns (`u32::MAX` = absent).
    expr_def_row: Vec<u32, A>,
    pat_def_row: Vec<u32, A>,
    callee_row: Vec<u32, A>,
    field_row: Vec<u32, A>,
    /// Per-`PTy` memo tables for `is_copy` and `needs_drop` (0 = unknown, 1 = false, 2 = true).
    copy_memo: Vec<Cell<u8>, A>,
    drop_memo: Vec<Cell<u8>, A>,
    /// The drop schedule: what is live, in declaration order ([`scope`]).
    pub(crate) sched: Vec<Entry, A>,
    /// Where each open scope begins in `sched`.
    pub(crate) marks: Vec<u32, A>,
    /// The enclosing loops and labelled blocks, innermost last.
    pub(crate) targets: Vec<Target, A>,
    /// Every expression a `break` or `continue` names, sorted.
    pub(crate) break_targets: Vec<ExprId, A>,
    /// Cleanup blocks made, filled after the body ([`scope`]).
    pub(crate) pending: Vec<Pending, A>,
    /// The one `resume` block, made when first needed.
    pub(crate) resume: Option<Block>,
    /// The operands of extending borrows of the `let` being lowered (the Reference, "Temporary
    /// lifetime extension"), and the index in `marks` of its statement's scope.
    pub(crate) extended: Vec<ExprId, A>,
    pub(crate) ext_mark: Option<usize>,
    /// Two-phase loans made for the call being lowered, activated at the call.
    pub(crate) activate: Vec<Loan, A>,
    pub(crate) depth: u32,
}

impl<'b, 'v, 'c, F: BodyFacts, A: Allocator + Copy> Lower<'b, 'v, 'c, F, A> {
    pub(crate) fn new(body: &'b BodyView<'v>, ck: &'b Checked<'c>, facts: &'b F, alloc: A) -> Self {
        let mut break_targets = Vec::new_in(alloc);
        for x in body.exprs.iter() {
            match *x {
                Expr::Break { target, .. } | Expr::Continue { target } => break_targets.push(target),
                _ => {}
            }
        }
        break_targets.sort_unstable();
        break_targets.dedup();
        let tys = ck.types.view();
        let n_exprs = body.exprs.len();
        let n_pats = body.pats.len();
        let n_tys = tys.len();
        let expr_def_row = index_rows(ck.expr_defs.column::<0>(), n_exprs, ExprId::ix, alloc);
        let pat_def_row = index_rows(ck.pat_defs.column::<0>(), n_pats, PatId::ix, alloc);
        let callee_row = index_rows(ck.callees.column::<0>(), n_exprs, ExprId::ix, alloc);
        let field_row = index_rows(ck.fields.column::<0>(), n_exprs, ExprId::ix, alloc);
        Lower {
            body,
            ck,
            tys,
            facts,
            b: BodyBuilder::new_with_types_in(tys, alloc),
            ret: Place(0),
            ret_ty: PTy(0),
            pat_place: Vec::from_elem_in(Place(NONE), n_pats, alloc),
            expr_def_row,
            pat_def_row,
            callee_row,
            field_row,
            copy_memo: Vec::from_elem_in(Cell::new(0u8), n_tys, alloc),
            drop_memo: Vec::from_elem_in(Cell::new(0u8), n_tys, alloc),
            sched: Vec::new_in(alloc),
            marks: Vec::new_in(alloc),
            targets: Vec::new_in(alloc),
            break_targets,
            pending: Vec::new_in(alloc),
            resume: None,
            extended: Vec::new_in(alloc),
            ext_mark: None,
            activate: Vec::new_in(alloc),
            depth: 0,
            alloc,
        }
    }

    /// The return place and the parameters of a fn body, then the entry block open.
    fn fn_frame(&mut self) -> Result<(), LowerError> {
        let root = self.body.root;
        let ret_ty = self.adjusted_ty(root)?;
        self.return_place(ret_ty);
        let params = self.body.params;
        let mut args: Vec<(PatId, Local, Place, PTy), A> = Vec::new_in(self.alloc);
        for (i, &p) in params.iter().enumerate() {
            let ty = self.pat_ty(p)?;
            let mutbl = match self.pat(p)? {
                Pat::Bind { ann, sub: None, .. } if ann.by_ref.is_none() && ann.mutable => Mutbl::Mut,
                _ => Mutbl::Not,
            };
            let local = self.b.new_local(LocalDecl { ty, kind: LocalKind::Arg(i as u32), mutbl });
            let place = self.b.local_place(local);
            args.push((p, local, place, ty));
        }
        // The Reference, "Destructors", function parameters: each parameter's scope holds the
        // ones after it, and its pattern's bindings are dropped before it.
        for &(p, local, place, ty) in args.iter() {
            let drops = self.needs_drop(ty, At::Pat(p))?;
            self.sched.push(Entry { local, place, drops, storage: false, cleanup: None });
            if self.simple_bind(p)? {
                self.bind(p, place);
                continue;
            }
            let mut out = Vec::new_in(self.alloc);
            self.bind_irrefutable(p, place, ty, &mut out)?;
            self.schedule(&out);
        }
        Ok(())
    }

    /// The return place `_0` of `ty`, and the entry block open.
    pub(crate) fn return_place(&mut self, ty: PTy) {
        let ret = self.b.new_local(LocalDecl { ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        self.ret = self.b.local_place(ret);
        self.ret_ty = ty;
        let entry = self.b.new_block(BlockKind::Normal);
        self.b.start(entry);
    }

    /// `_0 = root`, every live value dropped, `return`; the cleanup blocks filled; the body.
    pub(crate) fn finish(mut self, root: ExprId) -> Result<Body<A>, LowerError> {
        let ret = self.ret;
        self.lower_into(ret, root)?;
        let span = self.span(root);
        self.emit_exit(0, span)?;
        self.b.terminate(StmtKind::Return, span);
        self.fill_cleanups(span);
        let range = range_of(self.body, root);
        self.b.finish().map_err(|e| LowerError { construct: Construct::Build(e), at: At::Body, range })
    }

    pub(crate) fn refuse<T>(&self, construct: Construct, e: ExprId) -> Result<T, LowerError> {
        Err(LowerError { construct, at: At::Expr(e), range: range_of(self.body, e) })
    }

    pub(crate) fn refuse_pat<T>(&self, construct: Construct, p: PatId) -> Result<T, LowerError> {
        Err(LowerError { construct, at: At::Pat(p), range: pat_range_of(self.body, p) })
    }

    pub(crate) fn error_at(&self, construct: Construct, at: At) -> LowerError {
        let range = match at {
            At::Expr(e) => range_of(self.body, e),
            At::Pat(p) => pat_range_of(self.body, p),
            At::Body => range_of(self.body, self.body.root),
        };
        LowerError { construct, at, range }
    }

    pub(crate) fn refuse_at<T>(&self, construct: Construct, at: At) -> Result<T, LowerError> {
        Err(self.error_at(construct, at))
    }

    pub(crate) fn span(&self, e: ExprId) -> Span {
        let r = range_of(self.body, e);
        Span { lo: r.start, hi: r.end }
    }

    pub(crate) fn pat_span(&self, p: PatId) -> Span {
        let r = pat_range_of(self.body, p);
        Span { lo: r.start, hi: r.end }
    }

    pub(crate) fn expr(&self, e: ExprId) -> Result<Expr, LowerError> {
        match self.body.exprs.get(e.ix()) {
            Some(x) => Ok(*x),
            None => self.refuse(Construct::Malformed, self.body.root),
        }
    }

    pub(crate) fn pat(&self, p: PatId) -> Result<Pat, LowerError> {
        match self.body.pats.get(p.ix()) {
            Some(x) => Ok(*x),
            None => self.refuse(Construct::Malformed, self.body.root),
        }
    }

    pub(crate) fn expr_ty(&self, e: ExprId) -> Result<PTy, LowerError> {
        match self.ck.expr_ty.get(e.ix()) {
            Some(t) => Ok(*t),
            None => self.refuse(Construct::Malformed, e),
        }
    }

    /// The type of the place pattern `p` matches.
    pub(crate) fn pat_ty(&self, p: PatId) -> Result<PTy, LowerError> {
        match self.ck.pat_ty.get(p.ix()) {
            Some(t) => Ok(*t),
            None => self.refuse_pat(Construct::Malformed, p),
        }
    }

    /// The type of the variable binding pattern `p` makes.
    pub(crate) fn local_ty(&self, p: PatId) -> Result<PTy, LowerError> {
        match self.ck.local_ty.get(p.ix()) {
            Some(t) => Ok(*t),
            None => self.refuse_pat(Construct::Malformed, p),
        }
    }

    /// The type of `e` after its adjustments.
    pub(crate) fn adjusted_ty(&self, e: ExprId) -> Result<PTy, LowerError> {
        let (lo, hi) = self.adjust_rows(e);
        if hi > lo {
            let Some(&ty) = self.ck.adjusts.column::<2>().get(hi - 1) else {
                return self.refuse(Construct::Malformed, e);
            };
            return Ok(ty);
        }
        self.expr_ty(e)
    }

    /// The rows of `Checked::adjusts` for `e`, in the order they apply.
    pub(crate) fn adjust_rows(&self, e: ExprId) -> (usize, usize) {
        let col = self.ck.adjusts.column::<0>();
        let lo = col.partition_point(|x| *x < e);
        let hi = col.partition_point(|x| *x <= e);
        (lo, hi)
    }

    pub(crate) fn adjust(&self, i: usize) -> Option<(AdjustKind, PTy)> {
        let &kind = self.ck.adjusts.column::<1>().get(i)?;
        let &ty = self.ck.adjusts.column::<2>().get(i)?;
        Some((kind, ty))
    }

    pub(crate) fn expr_def(&self, e: ExprId) -> Option<Def> {
        let row = *self.expr_def_row.get(e.ix())?;
        if row == u32::MAX {
            return None;
        }
        self.ck.expr_defs.column::<1>().get(row as usize).copied()
    }

    pub(crate) fn pat_def(&self, p: PatId) -> Option<Def> {
        let row = *self.pat_def_row.get(p.ix())?;
        if row == u32::MAX {
            return None;
        }
        self.ck.pat_defs.column::<1>().get(row as usize).copied()
    }

    /// The method or operator trait method the check chose for `e`, and its arguments.
    pub(crate) fn callee_of(&self, e: ExprId) -> Option<(ItemKey, List)> {
        let row = *self.callee_row.get(e.ix())?;
        if row == u32::MAX {
            return None;
        }
        let i = row as usize;
        let &k = self.ck.callees.column::<1>().get(i)?;
        let &l = self.ck.callees.column::<2>().get(i)?;
        Some((k, l))
    }

    /// The index of the field field expression `e` names.
    pub(crate) fn field_of(&self, e: ExprId) -> Option<u32> {
        let row = *self.field_row.get(e.ix())?;
        if row == u32::MAX {
            return None;
        }
        self.ck.fields.column::<2>().get(row as usize).copied()
    }

    /// Type `t`'s node: the body's own (an overlay type this lowering made) or the record's.
    pub(crate) fn node(&self, t: PTy, at: At) -> Result<PNode, LowerError> {
        match self.b.body().view().own_ty(t) {
            Ok(Some(n)) => return Ok(n),
            Ok(None) => {}
            Err(_) => return self.refuse_at(Construct::Malformed, at),
        }
        match self.tys.node(t) {
            Ok(n) => Ok(n),
            Err(_) => self.refuse_at(Construct::Malformed, at),
        }
    }

    /// List `l`'s entries, the record's or the body's own: the builder holds both, so the
    /// slice borrows `self`.
    pub(crate) fn list(&self, l: List, at: At) -> Result<&[PTy], LowerError> {
        match self.b.list_entries(l) {
            Some(s) => Ok(s),
            None => self.refuse_at(Construct::Malformed, at),
        }
    }

    /// The type whose node is `n`: the record's when its table holds it, else the body's
    /// own (blaze-body's overlay), made once.
    pub(crate) fn find_ty(&mut self, n: PNode, at: At) -> Result<PTy, LowerError> {
        match self.b.intern_ty(n) {
            Ok(t) => Ok(t),
            Err(_) => self.refuse_at(Construct::Malformed, at),
        }
    }

    /// The tuple type `(a, b)`: the record's or the body's own.
    pub(crate) fn find_pair(&mut self, a: PTy, b: PTy, at: At) -> Result<PTy, LowerError> {
        match self.b.intern_list(&[a, b]) {
            Ok(l) => self.find_ty(PNode::Tuple(l), at),
            Err(_) => self.refuse_at(Construct::Malformed, at),
        }
    }

    pub(crate) fn adt_facts(&self, k: ItemKey, at: At) -> Result<blaze_flow_lower::AdtFacts, LowerError> {
        match self.facts.adt(k) {
            Some(f) => Ok(f),
            None => self.refuse_at(Construct::Malformed, at),
        }
    }

    /// Whether a value of `t` is `Copy` (a use copies it rather than moving it).
    pub(crate) fn is_copy(&self, root: PTy, at: At) -> Result<bool, LowerError> {
        pty_is_copy(
            CopyWalk {
                facts: self.facts,
                copy_memo: &self.copy_memo,
                adt_args: AdtCopyArgs::All,
                closures: ClosureCopy::Never,
                alloc: self.alloc,
            },
            root,
            |t| self.node(t, at).ok(),
            |l| self.b.list_entries(l),
            |err| match err {
                CopyWalkError::Malformed
                | CopyWalkError::UnknownAdt
                | CopyWalkError::CopyArgsUnaligned { .. } => self.error_at(Construct::Malformed, at),
                CopyWalkError::Bound => self.error_at(Construct::Bound, at),
            },
        )
    }

    /// Whether dropping a value of `t` may run code: its `Drop` row is written, and drop
    /// elaboration (D119 P4) removes the ones that do nothing at an instance.
    pub(crate) fn needs_drop(&self, root: PTy, at: At) -> Result<bool, LowerError> {
        pty_needs_drop(
            DropWalk {
                facts: self.facts,
                copy_memo: &self.copy_memo,
                drop_memo: &self.drop_memo,
                closures: ClosureDrop::ByValueCaptures { owner: self.body.owner, ck: self.ck },
                alloc: self.alloc,
            },
            root,
            || self.is_copy(root, at),
            |t| self.node(t, at).ok(),
            |l| self.b.list_entries(l),
            |err| match err {
                DropWalkError::Malformed | DropWalkError::UnknownAdt => self.error_at(Construct::Malformed, at),
                DropWalkError::Bound => self.error_at(Construct::Bound, at),
            },
        )
    }

    /// Whether parameter or binding pattern `p` is a plain by-value binding of the place's type.
    pub(crate) fn simple_bind(&self, p: PatId) -> Result<bool, LowerError> {
        Ok(match self.pat(p)? {
            Pat::Bind { ann, sub: None, .. } => {
                ann.by_ref.is_none() && self.ck.union_pat_of(p).is_none() && self.local_ty(p)? == self.pat_ty(p)?
            }
            _ => false,
        })
    }

    /// Records `place` as the binding place for pattern `p` (latest binding wins).
    #[inline]
    pub(crate) fn bind(&mut self, p: PatId, place: Place) {
        let i = p.ix();
        if i >= self.pat_place.len() {
            self.pat_place.resize(i + 1, Place(NONE));
        }
        if let Some(slot) = self.pat_place.get_mut(i) {
            *slot = place;
        }
    }

    /// The place binding pattern `p` was given.
    pub(crate) fn bound(&self, p: PatId, e: ExprId) -> Result<Place, LowerError> {
        match self.pat_place.get(p.ix()) {
            Some(&place) if place.0 != NONE => Ok(place),
            _ => self.refuse(Construct::Malformed, e),
        }
    }

    pub(crate) fn is_break_target(&self, e: ExprId) -> bool {
        self.break_targets.binary_search(&e).is_ok()
    }
}
