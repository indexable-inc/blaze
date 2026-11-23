//! What only `unsafe` code may do (E0133): one pass over the body after the end sequence, when
//! every type, callee and path is settled, in the same check and so in the same record and
//! read list as the types it reads (rules work-once and keys-cover-every-input).
//!
//! The rule is the Reference's, "Unsafety": "the following language level features cannot be
//! used in the safe subset of Rust: dereferencing a raw pointer; reading or writing a mutable or
//! unsafe external static variable; accessing a field of a union, other than to assign to it;
//! calling an unsafe function (including an intrinsic or foreign function); calling a safe
//! function marked with a `target_feature` from a function that does not have a `target_feature`
//! attribute enabling the same features; ... inline assembly". Each is legal only inside an
//! `unsafe` block or the body of an `unsafe fn` ("Unsafe functions", "Unsafe blocks").
//!
//! The context is lexical: a closure, an `async` block and an inline `const` take the context
//! they are written in, so an operation in a closure written inside `unsafe { .. }` is inside
//! it, and an `unsafe` block a macro's expansion writes covers what is written inside that
//! block, whoever wrote it, and nothing outside it.
//!
//! The exceptions the Reference states, each by its words:
//! - "Raw borrow operators": `&raw const *p` "is safe ... the dereference itself is not
//!   performed", and `&raw const STATIC_MUT` (`&raw const` of a `static mut` or an extern
//!   static) is safe ("Mutable statics": "taking a raw pointer to one is safe"); a chain of
//!   union fields under `&raw` (`&raw const u.f`, `&raw const (*p).f` for a union `*p`) is a
//!   place, never a read, so it is safe too. The operand of the dereference is still checked.
//! - "Unions", "Writing to unions": a write of a union field is safe. The place is the left
//!   side of `=` through fields and parentheses (`u.f = x`, `u.s.g = x`); `u.f += 1` reads the
//!   field, and a pattern that binds or tests a union field (`let U { f } = u`) reads it, where
//!   `U { f: _ }` does not.
//!
//! In an `unsafe fn`'s body outside any `unsafe` block, an operation is allowed; from edition
//! 2024 it is the `unsafe-op-in-unsafe-fn` lint's finding (rustc's `unsafe_op_in_unsafe_fn`,
//! warn by default there), a [`TypedFinding`] with no level (rule 81); before 2024 that lint is
//! allowed by default, so nothing is filed.
//!
//! A safe `#[target_feature]` fn called from a body with its own `#[target_feature]` is legal
//! when the body enables every feature of the callee; the world does not answer feature SETS
//! yet (rules/target_feature.rs), so that call is [`Undecided`], said by name in the record and
//! never taken as an error or a pass (stdarch calls such fns so throughout:
//! `vmovl_high_s8` calls `vget_high_s8`, core_arch aarch64/neon/generated.rs).
//!
//! Like rustc, the pass does not run over a body with type errors: what an expression is (a
//! raw pointer, a union) is unknown at an error type, and the errors are the program's to fix
//! first.

use blaze_own::{Alloc, Vec};

use crate::body::{Expr, Pat, Stmt, UnOp};
use crate::diag::{About, At, DiagCode, UnsafeOp};
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{Edition, ExprId, ItemKey, Mutbl, PatId, Range, Safety};
use crate::machine::Machine;
use crate::rules::args_inline::TypedFinding;
use crate::rules::target_feature::Gate;
use crate::ty::{Ty, TyView, TypeCtx};
use crate::world::{AdtKind, Def, World};

/// What the unsafety pass could not decide in a body: no error and no pass, filed as "not
/// checked" by the command that prints the body's verdict (rule 3: never quietly a pass).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Undecided {
    /// A call of the safe `#[target_feature]` fn `callee` from a body with a
    /// `#[target_feature]` of its own: legal when the body enables every feature of `callee`,
    /// which the world does not answer yet (the module doc).
    TargetFeatureCall { callee: ItemKey, at: Range },
}

impl Undecided {
    /// Its item-relative range (rule 71).
    pub const fn at(&self) -> Range {
        match *self {
            Undecided::TargetFeatureCall { at, .. } => at,
        }
    }

    /// Why it is not checked, in words a person reads.
    pub const fn says(&self) -> &'static str {
        match self {
            Undecided::TargetFeatureCall { .. } => {
                "a call of a safe `#[target_feature]` fn from a body with its own `#[target_feature]`: whether the body enables every feature of the callee is not read (the world gives no feature sets)"
            }
        }
    }

    /// The order the record keeps them in: by place (rule 25).
    pub const fn order(&self) -> (u32, u32) {
        let at = self.at();
        (at.start, at.end)
    }
}

/// The most parentheses looked through under one `&raw` (rule 18).
const MOST_PEELS: u32 = 256;

/// What covers an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctx {
    /// Nothing: an operation here is E0133.
    Safe,
    /// The body of an `unsafe fn`, and no `unsafe` block: an operation here is allowed, and from
    /// edition 2024 it is the `unsafe_op_in_unsafe_fn` lint's finding (the Edition Guide,
    /// "unsafe_op_in_unsafe_fn warning": "the body of an unsafe function is no longer treated
    /// as an unsafe block"); before 2024 that lint is allowed by default and nothing is filed.
    UnsafeFn,
    /// An `unsafe` block.
    Block,
}

/// One expression to visit: what covers it, and whether it is the place the left side of `=`
/// writes (a union field there is written, not read).
#[derive(Clone, Copy)]
struct Visit {
    e: ExprId,
    ctx: Ctx,
    assigned: bool,
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The unsafety pass (the module doc), after the end sequence.
    pub(crate) fn unsafety(&mut self) -> Result<(), Stop> {
        if self.errors > 0 {
            return Ok(());
        }
        let body = self.body;
        let alloc = self.alloc;
        // The owner's context: an `unsafe fn`'s body ("Unsafe functions"). A const, a static,
        // or a fn with no signature in the world is a safe context.
        let owner = match self.reads.fn_sig(body.owner)? {
            Some(sig) if sig.header.safety == Safety::Unsafe => Ctx::UnsafeFn,
            _ => Ctx::Safe,
        };
        // The settled facts, each sorted by expression for a binary search.
        let mut union_fields: Vec<ExprId, Alloc<'a>> = Vec::with_capacity_in(self.union_fields.len(), alloc);
        union_fields.extend_from_slice(&self.union_fields);
        union_fields.sort_unstable();
        union_fields.dedup();
        let mut callees: Vec<(ExprId, ItemKey), Alloc<'a>> = Vec::with_capacity_in(self.callees.len(), alloc);
        callees.extend(self.callees.iter().map(|&(e, k, _)| (e, k)));
        callees.sort_unstable_by_key(|c| c.0);
        let mut statics: Vec<(ExprId, ItemKey, Mutbl), Alloc<'a>> = Vec::new_in(alloc);
        for &(at, def) in self.defs.iter() {
            if let (At::Expr(e), Def::Static(k, m)) = (at, def) {
                statics.push((e, k, m));
            }
        }
        statics.sort_unstable_by_key(|s| s.0);
        // The places under `&raw` whose operation is no read (the module doc): a deref of a
        // raw pointer, a static, a union field of the chain.
        let mut raw_places: Vec<ExprId, Alloc<'a>> = Vec::new_in(alloc);
        for &x in body.exprs {
            if let Expr::RawRef { operand, .. } = x {
                self.raw_place(operand, &union_fields, &mut raw_places)?;
            }
        }
        raw_places.sort_unstable();
        // The walk, from the root and the fn's parameters, each node once.
        let mut stack: Vec<Visit, Alloc<'a>> = Vec::with_capacity_in(64, alloc);
        let mut pats: Vec<PatId, Alloc<'a>> = Vec::new_in(alloc);
        let mut kids: Vec<ExprId, Alloc<'a>> = Vec::new_in(alloc);
        for &p in body.params {
            self.union_pats(p, owner, &mut pats)?;
        }
        stack.push(Visit { e: body.root, ctx: owner, assigned: false });
        let mut steps: usize = 0;
        while let Some(Visit { e, ctx, assigned }) = stack.pop() {
            steps += 1;
            // Each node is under one parent, so the walk meets it once; the bound is the
            // frame column's (rule 18), past which the body is malformed.
            if steps > 4 * body.exprs.len() + 64 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let x = body.expr(e)?;
            let ctx = if body.is_unsafe_block(e) { Ctx::Block } else { ctx };
            let open = ctx != Ctx::Block;
            let raw = raw_places.binary_search(&e).is_ok();
            match x {
                Expr::Call { callee, .. } => match callees.binary_search_by_key(&e, |c| c.0) {
                    Ok(i) => self.unsafe_call(e, callees[i].1, ctx)?,
                    Err(_) => {
                        if open && self.unsafe_fn_ptr(self.expr_ty[callee.ix()])? {
                            self.refuse(ctx, At::Expr(e), UnsafeOp::Call(None));
                        }
                    }
                },
                Expr::MethodCall { .. } => {
                    if let Ok(i) = callees.binary_search_by_key(&e, |c| c.0) {
                        self.unsafe_call(e, callees[i].1, ctx)?;
                    }
                }
                Expr::Unary { op: UnOp::Deref, operand } if open && !raw => {
                    if let TyView::Ptr(..) = self.view(self.expr_ty[operand.ix()]) {
                        self.refuse(ctx, At::Expr(e), UnsafeOp::RawDeref);
                    }
                }
                Expr::Path(_) if open && !raw => {
                    if let Ok(i) = statics.binary_search_by_key(&e, |s| s.0) {
                        let (_, k, m) = statics[i];
                        let op = if m == Mutbl::Mut {
                            Some(UnsafeOp::MutStatic(k))
                        } else if self.reads.fn_sig(k)?.is_some_and(|s| s.header.safety == Safety::Unsafe) {
                            // A static of an `extern` block not written `safe`: its head's
                            // header is `unsafe` (front2 foreign_head.rs).
                            Some(UnsafeOp::ExternStatic(k))
                        } else {
                            None
                        };
                        if let Some(op) = op {
                            self.refuse(ctx, At::Expr(e), op);
                        }
                    }
                }
                Expr::Field { .. } | Expr::TupleField { .. }
                    if open && !raw && !assigned && union_fields.binary_search(&e).is_ok() =>
                {
                    self.refuse(ctx, At::Expr(e), UnsafeOp::UnionField);
                }
                Expr::Asm { .. } if open => self.refuse(ctx, At::Expr(e), UnsafeOp::InlineAsm),
                _ => {}
            }
            // The patterns this node binds, in its context.
            match x {
                Expr::Block { stmts, .. } | Expr::ConstBlock { stmts, .. } => {
                    for &s in body.stmt_list(stmts)? {
                        if let Stmt::Let { pat, .. } = body.stmt(s)? {
                            self.union_pats(pat, ctx, &mut pats)?;
                        }
                    }
                }
                Expr::Let { pat, .. } => self.union_pats(pat, ctx, &mut pats)?,
                Expr::Match { arms, .. } => {
                    for arm in body.arm_list(arms)? {
                        self.union_pats(arm.pat, ctx, &mut pats)?;
                    }
                }
                Expr::Closure { params, .. } => {
                    for &p in body.pat_list(params)? {
                        self.union_pats(p, ctx, &mut pats)?;
                    }
                }
                _ => {}
            }
            kids.clear();
            body.children(x, &mut kids)?;
            // Pushed in reverse, so the walk meets them in source order.
            for &k in kids.iter().rev() {
                let assigned = match x {
                    // The written place: its first node, and on down through fields and
                    // parentheses (the module doc).
                    Expr::Assign { lhs, .. } => k == lhs,
                    Expr::Field { .. } | Expr::TupleField { .. } | Expr::Paren(_) => assigned,
                    _ => false,
                };
                stack.push(Visit { e: k, ctx, assigned });
            }
        }
        Ok(())
    }

    /// The operation `op` at `at`, met in `ctx` (never [`Ctx::Block`]): E0133 in a safe
    /// context; in an `unsafe fn`'s body from edition 2024 the `unsafe-op-in-unsafe-fn` lint's
    /// finding (no level: rule 81), before 2024 nothing ([`Ctx::UnsafeFn`]).
    fn refuse(&mut self, ctx: Ctx, at: At, op: UnsafeOp) {
        match ctx {
            Ctx::Safe => self.diag(DiagCode::RequiresUnsafe, at, About::Unsafe(op)),
            Ctx::UnsafeFn if self.body.edition >= Edition::E2024 => {
                let at = self.range_of(at);
                self.typed_findings.push(TypedFinding::UnsafeOpInUnsafeFn { op, at });
            }
            Ctx::UnsafeFn | Ctx::Block => {}
        }
    }

    /// A call `e` of the fn item `k`: E0133 when `k` is `unsafe` or a safe `#[target_feature]`
    /// fn in a body that enables none of its features, outside `unsafe`; [`Undecided`] when the
    /// body has features of its own and the outcome would be a finding.
    fn unsafe_call(&mut self, e: ExprId, k: ItemKey, ctx: Ctx) -> Result<(), Stop> {
        // Before 2024 nothing in an `unsafe fn`'s body is filed ([`Ctx::UnsafeFn`]).
        let files = match ctx {
            Ctx::Safe => true,
            Ctx::UnsafeFn => self.body.edition >= Edition::E2024,
            Ctx::Block => false,
        };
        if !files {
            return Ok(());
        }
        let Some(sig) = self.reads.fn_sig(k)? else { return Ok(()) };
        if sig.header.safety == Safety::Unsafe {
            self.refuse(ctx, At::Expr(e), UnsafeOp::Call(Some(k)));
            return Ok(());
        }
        let attrs = self.reads.attrs(k)?;
        match self.feature_gate(attrs, sig.header.safety)? {
            Gate::Open => {}
            Gate::Unsafe => self.refuse(ctx, At::Expr(e), UnsafeOp::TargetFeatureCall(k)),
            Gate::FeatureBody => {
                let at = self.range_of(At::Expr(e));
                self.undecided.push(Undecided::TargetFeatureCall { callee: k, at });
            }
        }
        Ok(())
    }

    /// Whether a callee of type `t` is an `unsafe fn` pointer, through references (a call
    /// autoderefs its callee: rules/call.rs).
    fn unsafe_fn_ptr(&mut self, mut t: Ty) -> Result<bool, Stop> {
        for _ in 0..MOST_PEELS {
            match self.view(t) {
                TyView::FnPtr(_, h) => return Ok(h.safety == Safety::Unsafe),
                TyView::Ref(_, inner) => t = inner,
                _ => return Ok(false),
            }
        }
        Err(CheckError::Limit(Limit::Table).into())
    }

    /// The nodes of the place under `&raw` whose operation is no read, pushed onto `out`:
    /// through parentheses and the union fields of the chain, the dereference of a raw pointer
    /// or the static at its base (the module doc).
    fn raw_place(&self, mut e: ExprId, union_fields: &[ExprId], out: &mut Vec<ExprId, Alloc<'a>>) -> Result<(), Stop> {
        for _ in 0..MOST_PEELS {
            match self.body.expr(e)? {
                Expr::Paren(inner) => e = inner,
                Expr::Field { base, .. } | Expr::TupleField { base, .. } if union_fields.binary_search(&e).is_ok() => {
                    out.push(e);
                    e = base;
                }
                Expr::Unary { op: UnOp::Deref, .. } | Expr::Path(_) => {
                    out.push(e);
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        Err(CheckError::Limit(Limit::Table).into())
    }

    /// E0133 for each pattern under `p` that reads a union's field outside `unsafe`: a field
    /// pattern of a union struct pattern that is not `_` ("Unions", "Pattern matching on
    /// unions").
    fn union_pats(&mut self, p: PatId, ctx: Ctx, stack: &mut Vec<PatId, Alloc<'a>>) -> Result<(), Stop> {
        if ctx == Ctx::Block || (ctx == Ctx::UnsafeFn && self.body.edition < Edition::E2024) {
            return Ok(());
        }
        let body = self.body;
        stack.clear();
        stack.push(p);
        let mut steps: usize = 0;
        while let Some(p) = stack.pop() {
            steps += 1;
            if steps > 4 * body.pats.len() + 64 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            match body.pat(p)? {
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
                Pat::Bind { sub, .. } => stack.extend(sub),
                Pat::Ref { inner, .. } => stack.push(inner),
                Pat::Tuple { elems, .. } | Pat::TupleStruct { elems, .. } | Pat::Slice { elems, .. } | Pat::Or { alts: elems } => {
                    stack.extend_from_slice(body.pat_list(elems)?);
                }
                Pat::Struct { fields, .. } => {
                    let fields = body.field_pat_list(fields)?;
                    if self.is_union(self.pat_ty[p.ix()])? {
                        if fields.iter().any(|f| !matches!(body.pat(f.pat), Ok(Pat::Wild))) {
                            self.refuse(ctx, At::Pat(p), UnsafeOp::UnionField);
                        }
                    } else {
                        stack.extend(fields.iter().map(|f| f.pat));
                    }
                }
                Pat::Record { fields, .. } => {
                    let fields = body.field_pat_list(fields)?;
                    stack.extend(fields.iter().map(|f| f.pat));
                }
            }
        }
        Ok(())
    }

    /// Whether `t` is a union, through references (a pattern's default binding mode matches
    /// through them).
    fn is_union(&mut self, mut t: Ty) -> Result<bool, Stop> {
        for _ in 0..MOST_PEELS {
            match self.view(t) {
                TyView::Ref(_, inner) => t = inner,
                TyView::Adt(k, _) => return Ok(self.reads.adt(k)?.is_some_and(|a| a.kind == AdtKind::Union)),
                _ => return Ok(false),
            }
        }
        Err(CheckError::Limit(Limit::Table).into())
    }
}
