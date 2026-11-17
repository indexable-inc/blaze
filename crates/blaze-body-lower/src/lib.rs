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

use blaze_body::{Block, BlockKind, Body, BodyBuilder, BuildError, Local, LocalDecl, LocalKind, Loan, Place, Span, StmtKind};
use blaze_flow_lower::Facts;
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
pub fn lower<A: Allocator + Clone, F: BodyFacts>(
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

fn range_of(body: &BodyView<'_>, e: ExprId) -> Range {
    body.expr_ranges.get(e.ix()).copied().unwrap_or(Range { start: 0, end: 0 })
}

fn pat_range_of(body: &BodyView<'_>, p: PatId) -> Range {
    body.pat_ranges.get(p.ix()).copied().unwrap_or(Range { start: 0, end: 0 })
}

/// The lowering's state over one body.
pub(crate) struct Lower<'b, 'v, 'c, F, A: Allocator + Clone> {
    pub(crate) body: &'b BodyView<'v>,
    pub(crate) ck: &'b Checked<'c>,
    pub(crate) tys: PTys<'b>,
    pub(crate) facts: &'b F,
    pub(crate) alloc: A,
    pub(crate) b: BodyBuilder<A>,
    /// The return place `_0` and its type.
    pub(crate) ret: Place,
    pub(crate) ret_ty: PTy,
    /// Each binding pattern's place: its local's, or a closure's upvar place.
    pub(crate) binds: Vec<(PatId, Place), A>,
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

impl<'b, 'v, 'c, F: BodyFacts, A: Allocator + Clone> Lower<'b, 'v, 'c, F, A> {
    pub(crate) fn new(body: &'b BodyView<'v>, ck: &'b Checked<'c>, facts: &'b F, alloc: A) -> Self {
        let mut break_targets = Vec::new_in(alloc.clone());
        for x in body.exprs.iter() {
            match *x {
                Expr::Break { target, .. } | Expr::Continue { target } => break_targets.push(target),
                _ => {}
            }
        }
        break_targets.sort_unstable();
        break_targets.dedup();
        Lower {
            body,
            ck,
            tys: ck.types.view(),
            facts,
            b: BodyBuilder::new_with_types_in(ck.types.view(), alloc.clone()),
            ret: Place(0),
            ret_ty: PTy(0),
            binds: Vec::new_in(alloc.clone()),
            sched: Vec::new_in(alloc.clone()),
            marks: Vec::new_in(alloc.clone()),
            targets: Vec::new_in(alloc.clone()),
            break_targets,
            pending: Vec::new_in(alloc.clone()),
            resume: None,
            extended: Vec::new_in(alloc.clone()),
            ext_mark: None,
            activate: Vec::new_in(alloc.clone()),
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
        let mut args: Vec<(PatId, Local, Place, PTy), A> = Vec::new_in(self.alloc.clone());
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
                self.binds.push((p, place));
                continue;
            }
            let mut out = Vec::new_in(self.alloc.clone());
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

    pub(crate) fn refuse_at<T>(&self, construct: Construct, at: At) -> Result<T, LowerError> {
        match at {
            At::Expr(e) => self.refuse(construct, e),
            At::Pat(p) => self.refuse_pat(construct, p),
            At::Body => Err(LowerError { construct, at, range: range_of(self.body, self.body.root) }),
        }
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
            return Ok(self.ck.adjusts.column::<2>()[hi - 1]);
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

    pub(crate) fn adjust(&self, i: usize) -> (AdjustKind, PTy) {
        (self.ck.adjusts.column::<1>()[i], self.ck.adjusts.column::<2>()[i])
    }

    pub(crate) fn expr_def(&self, e: ExprId) -> Option<Def> {
        let col = self.ck.expr_defs.column::<0>();
        let i = col.iter().position(|x| *x == e)?;
        Some(self.ck.expr_defs.column::<1>()[i])
    }

    pub(crate) fn pat_def(&self, p: PatId) -> Option<Def> {
        let col = self.ck.pat_defs.column::<0>();
        let i = col.iter().position(|x| *x == p)?;
        Some(self.ck.pat_defs.column::<1>()[i])
    }

    /// The method or operator trait method the check chose for `e`, and its arguments.
    pub(crate) fn callee_of(&self, e: ExprId) -> Option<(ItemKey, List)> {
        let col = self.ck.callees.column::<0>();
        let i = col.iter().position(|x| *x == e)?;
        Some((self.ck.callees.column::<1>()[i], self.ck.callees.column::<2>()[i]))
    }

    /// The index of the field field expression `e` names.
    pub(crate) fn field_of(&self, e: ExprId) -> Option<u32> {
        let col = self.ck.fields.column::<0>();
        let i = col.iter().position(|x| *x == e)?;
        Some(self.ck.fields.column::<2>()[i])
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
    pub(crate) fn is_copy(&self, t: PTy, at: At) -> Result<bool, LowerError> {
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_at(Construct::Bound, at);
            }
            match self.node(t, at)? {
                PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Never
                | PNode::Ptr(..)
                | PNode::FnDef(..)
                | PNode::FnPtr(..)
                | PNode::ConstValue { .. }
                | PNode::ConstParam(_)
                | PNode::Ref(Mutbl::Not, _) => {}
                PNode::Ref(Mutbl::Mut, _) => return Ok(false),
                PNode::Tuple(l) => stack.extend_from_slice(self.list(l, at)?),
                PNode::Array(i, _) | PNode::Pat(i, _) => stack.push(i),
                PNode::Adt(k, l) => {
                    if !self.adt_facts(k, at)?.copy {
                        return Ok(false);
                    }
                    stack.extend_from_slice(self.list(l, at)?);
                }
                PNode::Param(i) => {
                    if !self.facts.param_copy(i) {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Whether dropping a value of `t` may run code: its `Drop` row is written, and drop
    /// elaboration (D119 P4) removes the ones that do nothing at an instance.
    pub(crate) fn needs_drop(&self, t: PTy, at: At) -> Result<bool, LowerError> {
        if self.is_copy(t, at)? {
            return Ok(false);
        }
        let mut stack: Vec<PTy, A> = Vec::new_in(self.alloc.clone());
        stack.push(t);
        let mut steps = 0u32;
        while let Some(t) = stack.pop() {
            steps += 1;
            if steps > MOST_POSITIONS {
                return self.refuse_at(Construct::Bound, at);
            }
            match self.node(t, at)? {
                PNode::Tuple(l) => stack.extend_from_slice(self.list(l, at)?),
                PNode::Array(i, _) | PNode::Pat(i, _) => stack.push(i),
                PNode::Adt(k, l) => {
                    if self.adt_facts(k, at)?.needs_drop {
                        return Ok(true);
                    }
                    stack.extend_from_slice(self.list(l, at)?);
                }
                PNode::Param(_) | PNode::Alias { .. } | PNode::Opaque(..) | PNode::Dynamic(_) => return Ok(true),
                PNode::Future(..) | PNode::Coroutine { .. } => return Ok(true),
                // A closure of this body holds its by-value captures; one of another body is
                // not known here, and its `Drop` row stays for elaboration to judge.
                PNode::Closure { owner, ordinal, .. } => {
                    if owner != self.body.owner {
                        return Ok(true);
                    }
                    let Some(&c) = self.ck.closures.column::<0>().get(ordinal as usize) else {
                        return self.refuse_at(Construct::Malformed, at);
                    };
                    let cols = self.ck.captures.column::<0>();
                    for (i, x) in cols.iter().enumerate() {
                        if *x == c && self.ck.captures.column::<2>()[i] == blaze_typeck::CaptureKind::ByValue {
                            stack.push(self.local_ty(self.ck.captures.column::<1>()[i])?);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(false)
    }

    /// Whether parameter or binding pattern `p` is a plain by-value binding of the place's type.
    pub(crate) fn simple_bind(&self, p: PatId) -> Result<bool, LowerError> {
        Ok(match self.pat(p)? {
            Pat::Bind { ann, sub: None, .. } => ann.by_ref.is_none() && self.local_ty(p)? == self.pat_ty(p)?,
            _ => false,
        })
    }

    /// The place binding pattern `p` was given.
    pub(crate) fn bound(&self, p: PatId, e: ExprId) -> Result<Place, LowerError> {
        match self.binds.iter().rev().find(|(q, _)| *q == p) {
            Some(&(_, place)) => Ok(place),
            None => self.refuse(Construct::Malformed, e),
        }
    }

    pub(crate) fn is_break_target(&self, e: ExprId) -> bool {
        self.break_targets.binary_search(&e).is_ok()
    }
}
