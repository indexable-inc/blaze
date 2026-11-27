//! RFC 2229 uncaptured outer locals in non-reading place contexts inside closures and coroutines
//! (the Reference, "Closure types", "Capture precision"; `blaze-typeck/src/capture_places.rs`).
//!
//! When a closure or `async` block mentions an outer binding in a place context that neither reads
//! nor moves nor borrows any byte of it — such as `let _ = outer;`, `_ = outer.field;`,
//! `match prefix { CharPrefix::Binary => ... }` on a 1-variant non-`#[non_exhaustive]` enum, or
//! `match outer { Wrap(_) => ... }` — `capture_places.rs` correctly omits it from the environment
//! (`EnvField`s), so `self.slots[p.ix()]` stays `None`.
//!
//! This module distinguishes genuine local-before-binding bugs (any binding declared inside the
//! current body or outside a closure/coroutine MUST still fail with `"a local used before its
//! binding"`) from uncaptured outer bindings, and verifies that a pattern context truly performs
//! no reads before skipping evaluation of the outer place.

use core::alloc::Allocator;

use blaze_lir::{Flags, Op, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::env_fields::{Cover, cover, use_path};
use blaze_typeck::{
    AdjustKind, AdtKind, Def, Expr, ExprId, LangItem, List, PNode, PTy, Pat, PatId, Stmt, UnOp,
};

use super::{NO_EXPR, Shape, Task, Walk, malformed};
use crate::BuildError;
use crate::layout::Variants;

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Marks every pattern declared inside closure or coroutine `owner` (its own parameters, its
    /// local bindings, and any nested closures'/blocks' parameters and locals), matching
    /// `blaze-typeck/src/capture_places.rs` `mark_owner_bound`.
    pub(super) fn mark_owner_bound(&mut self, owner: ExprId) -> Result<(), BuildError> {
        self.owner_expr = Some(owner);
        self.body_bound.clear();
        self.body_bound.resize(self.body.pats.len(), false);
        let mut pat_stack: Vec<PatId, A> = Vec::new_in(self.alloc);
        let mut expr_stack: Vec<ExprId, A> = Vec::new_in(self.alloc);
        pat_stack.extend_from_slice(self.body.params);
        expr_stack.push(self.body.root);
        let mut steps = 0usize;
        while let Some(e) = expr_stack.pop() {
            steps += 1;
            if steps > self.body.exprs.len() + 1 {
                return Err(malformed("a cycle among a body's expressions"));
            }
            match self.expr(e)? {
                Expr::Missing
                | Expr::Lit(_)
                | Expr::Local(_)
                | Expr::Path(_)
                | Expr::Continue { .. }
                | Expr::Underscore
                | Expr::OffsetOf { .. }
                | Expr::ConstBlock { .. } => {}
                Expr::Paren(c)
                | Expr::Loop { body: c }
                | Expr::Field { base: c, .. }
                | Expr::TupleField { base: c, .. }
                | Expr::Unary { operand: c, .. }
                | Expr::Ref { operand: c, .. }
                | Expr::RawRef { operand: c, .. }
                | Expr::Cast { operand: c, .. }
                | Expr::Repeat { elem: c, .. }
                | Expr::Await(c)
                | Expr::Become { value: c }
                | Expr::AsyncBlock { body: c, .. } => expr_stack.push(c),
                Expr::Closure { params, body, .. } => {
                    let ps = self
                        .body
                        .pat_lists
                        .get(params.range())
                        .ok_or(malformed("a nested closure's parameters past `pat_lists`"))?;
                    pat_stack.extend_from_slice(ps);
                    expr_stack.push(body);
                }
                Expr::Block { stmts, tail, .. } => {
                    let ss = self
                        .body
                        .stmt_lists
                        .get(stmts.range())
                        .ok_or(malformed("a block's statements past `stmt_lists`"))?;
                    for &s in ss {
                        match self
                            .body
                            .stmts
                            .get(s.ix())
                            .copied()
                            .ok_or(malformed("a statement past `stmts`"))?
                        {
                            Stmt::Let { pat, init, else_, .. } => {
                                pat_stack.push(pat);
                                expr_stack.extend(init);
                                expr_stack.extend(else_);
                            }
                            Stmt::Expr { expr, .. } => expr_stack.push(expr),
                            Stmt::Item => {}
                        }
                    }
                    expr_stack.extend(tail);
                }
                Expr::If { cond, then, else_ } => {
                    expr_stack.push(cond);
                    expr_stack.push(then);
                    expr_stack.extend(else_);
                }
                Expr::Let { pat, init } => {
                    pat_stack.push(pat);
                    expr_stack.push(init);
                }
                Expr::Match { scrutinee, arms } => {
                    expr_stack.push(scrutinee);
                    let list = self
                        .body
                        .arms
                        .get(arms.range())
                        .ok_or(malformed("a match's arms past `arms`"))?;
                    for arm in list {
                        pat_stack.push(arm.pat);
                        expr_stack.extend(arm.guard);
                        expr_stack.push(arm.body);
                    }
                }
                Expr::Break { value, .. } | Expr::Return { value } => expr_stack.extend(value),
                Expr::Call { callee, args } => {
                    expr_stack.push(callee);
                    let list = self
                        .body
                        .expr_lists
                        .get(args.range())
                        .ok_or(malformed("a call's arguments past `expr_lists`"))?;
                    expr_stack.extend_from_slice(list);
                }
                Expr::MethodCall { receiver, args, .. } => {
                    expr_stack.push(receiver);
                    let list = self
                        .body
                        .expr_lists
                        .get(args.range())
                        .ok_or(malformed("a method call's arguments past `expr_lists`"))?;
                    expr_stack.extend_from_slice(list);
                }
                Expr::Binary { lhs, rhs, .. }
                | Expr::Assign { lhs, rhs }
                | Expr::AssignOp { lhs, rhs, .. }
                | Expr::Index { base: lhs, index: rhs } => {
                    expr_stack.push(lhs);
                    expr_stack.push(rhs);
                }
                Expr::Tuple { elems } | Expr::Array { elems } | Expr::FormatArgs { args: elems, .. } => {
                    let list = self
                        .body
                        .expr_lists
                        .get(elems.range())
                        .ok_or(malformed("an expression list past `expr_lists`"))?;
                    expr_stack.extend_from_slice(list);
                }
                Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                    let inits = self
                        .body
                        .field_inits
                        .get(fields.range())
                        .ok_or(malformed("a struct literal's fields past `field_inits`"))?;
                    for fi in inits {
                        expr_stack.push(fi.expr);
                    }
                    expr_stack.extend(base.expr());
                }
                Expr::Asm { operands, .. } => {
                    let ops = self
                        .body
                        .asm_operands
                        .get(operands.range())
                        .ok_or(malformed("an `asm!`'s operands past `asm_operands`"))?;
                    for op in ops {
                        match op.kind {
                            blaze_typeck::AsmKind::In
                            | blaze_typeck::AsmKind::Out
                            | blaze_typeck::AsmKind::LateOut
                            | blaze_typeck::AsmKind::Label => expr_stack.push(op.expr),
                            blaze_typeck::AsmKind::InOut | blaze_typeck::AsmKind::InLateOut => {
                                expr_stack.push(op.expr);
                                expr_stack.extend(op.out_expr);
                            }
                            blaze_typeck::AsmKind::Sym | blaze_typeck::AsmKind::Const => {}
                        }
                    }
                }
            }
        }
        let mut pat_steps = 0usize;
        while let Some(p) = pat_stack.pop() {
            pat_steps += 1;
            if pat_steps > self.body.pats.len() + 1 {
                return Err(malformed("a cycle among a body's patterns"));
            }
            let slot = self
                .body_bound
                .get_mut(p.ix())
                .ok_or(malformed("a pattern past `pats`"))?;
            *slot = true;
            match self.pat(p)? {
                Pat::Bind { sub, .. } => pat_stack.extend(sub),
                Pat::Ref { inner, .. } => pat_stack.push(inner),
                Pat::Tuple { elems, .. }
                | Pat::TupleStruct { elems, .. }
                | Pat::Slice { elems, .. }
                | Pat::Or { alts: elems } => {
                    let list = self
                        .body
                        .pat_lists
                        .get(elems.range())
                        .ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    pat_stack.extend_from_slice(list);
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    let fps = self
                        .body
                        .field_pats
                        .get(fields.range())
                        .ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    for fp in fps {
                        pat_stack.push(fp.pat);
                    }
                }
                Pat::Missing | Pat::Wild | Pat::Lit(_) | Pat::NegLit(_) | Pat::Path(_) | Pat::Range { .. } => {}
            }
        }
        Ok(())
    }

    /// Whether `p` is a binding of an enclosing body (never a local declared inside the current
    /// fn, closure, or coroutine body).
    pub(super) fn is_outer_local(&self, p: PatId) -> Result<bool, BuildError> {
        if self.owner_expr.is_none() {
            return Ok(false);
        }
        let Some(&bound) = self.body_bound.get(p.ix()) else {
            return Err(malformed("a local past `pats`"));
        };
        Ok(!bound && matches!(self.pat(p)?, Pat::Bind { .. }))
    }

    /// Whether `e` is a pure place expression rooted at an outer binding that this closure or
    /// coroutine did NOT capture (neither whole in `self.slots` nor covered by `self.env_places`,
    /// and with no non-`Box` overloaded `Deref` or value-producing adjustment along its chain).
    pub(super) fn is_uncaptured_outer_place(&mut self, e: ExprId) -> Result<bool, BuildError> {
        if self.owner_expr.is_none() {
            return Ok(false);
        }
        let Ok(Some(path)) = use_path(self.body, self.checked, e, u32::MAX) else {
            return Ok(false);
        };
        if !self.is_outer_local(path.root)? {
            return Ok(false);
        }
        if self.slots.get(path.root.ix()).copied().flatten().is_some() {
            return Ok(false);
        }
        if !self.env_places.is_empty()
            && matches!(
                cover(self.checked, self.env_places.fields(), &path),
                Cover::Exact(_) | Cover::Under { .. } | Cover::RefOf(_)
            )
        {
            return Ok(false);
        }
        self.place_chain_has_no_overloaded_deref(e)
    }

    /// Verifies that every adjustment along place expression `e`'s chain is a builtin dereference
    /// or `Box`'s dereference (never a user `Deref::deref` call, which runs code and borrows its
    /// receiver in `capture_places.rs`).
    fn place_chain_has_no_overloaded_deref(&mut self, e: ExprId) -> Result<bool, BuildError> {
        let mut x = e;
        let owned_box = self.adts.lang_adt(LangItem::OwnedBox);
        for _ in 0..=self.body.exprs.len() {
            let rows = self.adjust_rows(x);
            for (k, r) in rows.enumerate() {
                match self.checked.adjusts.column::<1>().get(r).copied() {
                    Some(AdjustKind::Deref { overloaded: None }) => {}
                    Some(AdjustKind::Deref { overloaded: Some(_) }) => {
                        let before = self.ty_before(x, k)?;
                        let g = self.ground_ty(before).map_err(|error| Self::ty_err(x, error))?;
                        let is_box = matches!(
                            self.ground_node(g).map_err(|error| Self::ty_err(x, error))?,
                            PNode::Adt(adt, _) if owned_box == Some(adt)
                        );
                        if !is_box {
                            return Ok(false);
                        }
                    }
                    _ => return Ok(false),
                }
            }
            match self.expr(x)? {
                Expr::Paren(inner) => x = inner,
                Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Index { base, .. } => x = base,
                Expr::Unary { op: UnOp::Deref, operand } => x = operand,
                Expr::Local(_) => return Ok(true),
                _ => return Ok(false),
            }
        }
        Err(malformed("a cycle among a body's expressions"))
    }

    /// Evaluates and discards any `index` subexpressions along an uncaptured outer place chain
    /// (left to right: `a[i()][j()]` runs `i()` then `j()`), without reading the place itself.
    pub(super) fn discard_uncaptured_place_effects(&mut self, e: ExprId) -> Result<(), BuildError> {
        let mut x = e;
        for _ in 0..=self.body.exprs.len() {
            match self.expr(x)? {
                Expr::Paren(inner) => x = inner,
                Expr::Field { base, .. } | Expr::TupleField { base, .. } => x = base,
                Expr::Unary { op: UnOp::Deref, operand } => x = operand,
                Expr::Index { base, index } => {
                    self.work.push(Task::Discard);
                    self.work.push(Task::Eval(index));
                    x = base;
                }
                Expr::Local(_) => return Ok(()),
                _ => return Err(malformed("an uncaptured outer place ending in other than a local")),
            }
        }
        Err(malformed("a cycle among a body's expressions"))
    }

    /// Whether `g` (after peeling references) is a multivariant enum or multi-member union whose
    /// discriminant a pattern would have to read (`capture_places.rs` `multivariant`).
    fn is_multivariant_ground(&mut self, p: PatId, g: PTy) -> Result<bool, BuildError> {
        match self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))? {
            PNode::Adt(k, _) => {
                let Some(adt) = self.adts.adt(k) else {
                    return Ok(true);
                };
                if adt.kind == AdtKind::Enum && adt.variants.len() != 1 {
                    return Ok(true);
                }
                Ok(false)
            }
            PNode::Union(l) => Ok(l.len != 1),
            _ => Ok(false),
        }
    }

    /// Whether matching pattern `root` against a place of ground type `root_g` reads, moves,
    /// borrows, or binds NOTHING (`capture_places.rs` `walk_pat`): every subpattern is an
    /// irrefutable structural pattern (wildcard, struct, 1-variant enum, tuple, array, `[..]`,
    /// reference peel, or-pattern of non-reading patterns) that matches unconditionally without
    /// touching a single byte of memory.
    pub(super) fn pat_does_not_read(&mut self, root: PatId, root_g: PTy) -> Result<bool, BuildError> {
        let mut todo: Vec<(PatId, PTy), A> = Vec::new_in(self.alloc);
        todo.push((root, root_g));
        let mut steps = 0usize;
        while let Some((p, mut g)) = todo.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return Err(malformed("a cycle among a body's patterns"));
            }
            let pat = self.pat(p)?;
            if Self::peels(pat) && !(matches!(pat, Pat::Path(_)) && self.pat_is_ref(p)?) {
                let mut peels = 0usize;
                while let PNode::Ref(_, inner) = self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))? {
                    peels += 1;
                    if peels > 256 {
                        return Ok(false);
                    }
                    g = inner;
                }
            }
            match pat {
                Pat::Wild => {}
                Pat::Bind { .. } | Pat::Lit(_) | Pat::NegLit(_) | Pat::Range { .. } | Pat::Missing => {
                    return Ok(false);
                }
                Pat::Ref { inner, .. } => {
                    let PNode::Ref(_, to) = self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))? else {
                        return Ok(false);
                    };
                    todo.push((inner, to));
                }
                Pat::Or { alts } => {
                    let list = self
                        .body
                        .pat_lists
                        .get(alts.range())
                        .ok_or(malformed("an or-pattern past `pat_lists`"))?;
                    for &alt in list {
                        todo.push((alt, g));
                    }
                }
                Pat::Path(_) => {
                    let Some(def) = self.pat_def(p) else {
                        return Ok(false);
                    };
                    if matches!(def, Def::Const(_)) {
                        return Ok(false);
                    }
                    if self.is_multivariant_ground(p, g)? {
                        return Ok(false);
                    }
                    let Some(variant) = Self::def_variant(Some(def)) else {
                        return Ok(false);
                    };
                    let l = self.layout(g).map_err(|error| Self::pat_ty_err(p, error))?;
                    let l_ix = usize::try_from(l).map_err(|_| malformed("a layout index past `usize`"))?;
                    if let Some(Shape::Enum { id, .. }) = self.layouts.get(l_ix).map(|row| row.shape)
                        && !matches!(self.ls.get(id).variants, Variants::Single { index } if index == variant)
                    {
                        return Ok(false);
                    }
                }
                Pat::Tuple { elems, rest } | Pat::TupleStruct { elems, rest, .. } => {
                    if matches!(pat, Pat::TupleStruct { .. }) && self.is_multivariant_ground(p, g)? {
                        return Ok(false);
                    }
                    let subs = self
                        .body
                        .pat_lists
                        .get(elems.range())
                        .ok_or(malformed("a pattern's elements past `pat_lists`"))?;
                    let l = self.layout(g).map_err(|error| Self::pat_ty_err(p, error))?;
                    let l_ix = usize::try_from(l).map_err(|_| malformed("a layout index past `usize`"))?;
                    let (first, len) = if matches!(pat, Pat::TupleStruct { .. }) {
                        let Some(variant) = Self::def_variant(self.pat_def(p)) else {
                            return Ok(false);
                        };
                        if let Some(Shape::Enum { id, .. }) = self.layouts.get(l_ix).map(|row| row.shape)
                            && !matches!(self.ls.get(id).variants, Variants::Single { index } if index == variant)
                        {
                            return Ok(false);
                        }
                        let (first, len, _) = self.variant_rows(l, variant)?;
                        (first, len)
                    } else {
                        match self.layouts.get(l_ix).map(|row| row.shape) {
                            Some(Shape::Fields { first, len }) => (first, len),
                            _ => return Ok(false),
                        }
                    };
                    let first = usize::try_from(first).map_err(|_| malformed("a field offset past `usize`"))?;
                    let n = usize::try_from(len).map_err(|_| malformed("a field count past `usize`"))?;
                    let rest = match rest {
                        Some(r) => Some(usize::try_from(r).map_err(|_| malformed("a `..` position past `usize`"))?),
                        None => None,
                    };
                    let fits = match rest {
                        Some(_) => subs.len() <= n,
                        None => subs.len() == n,
                    };
                    if !fits {
                        return Ok(false);
                    }
                    for (j, &sub) in subs.iter().enumerate() {
                        let k = match rest {
                            Some(r) if j >= r => n.checked_sub(subs.len() - j).ok_or(malformed("a pattern longer than its type"))?,
                            _ => j,
                        };
                        let Some(&fg) = self.layout_field_tys.get(first + k) else {
                            return Ok(false);
                        };
                        todo.push((sub, fg));
                    }
                }
                Pat::Struct { fields, .. } | Pat::Record { fields, .. } => {
                    if matches!(pat, Pat::Struct { .. }) && self.is_multivariant_ground(p, g)? {
                        return Ok(false);
                    }
                    let fps = self
                        .body
                        .field_pats
                        .get(fields.range())
                        .ok_or(malformed("a struct pattern's fields past `field_pats`"))?;
                    let l = self.layout(g).map_err(|error| Self::pat_ty_err(p, error))?;
                    let l_ix = usize::try_from(l).map_err(|_| malformed("a layout index past `usize`"))?;
                    let variant = if matches!(pat, Pat::Record { .. }) {
                        0
                    } else {
                        let Some(v) = Self::def_variant(self.pat_def(p)) else {
                            return Ok(false);
                        };
                        if let Some(Shape::Enum { id, .. }) = self.layouts.get(l_ix).map(|row| row.shape)
                            && !matches!(self.ls.get(id).variants, Variants::Single { index } if index == v)
                        {
                            return Ok(false);
                        }
                        v
                    };
                    let (first, len, _) = self.variant_rows(l, variant)?;
                    let first = usize::try_from(first).map_err(|_| malformed("a field offset past `usize`"))?;
                    let len = usize::try_from(len).map_err(|_| malformed("a field count past `usize`"))?;
                    let Some(names) = self.layout_fields.get(first..first + len) else {
                        return Ok(false);
                    };
                    let Some(tys) = self.layout_field_tys.get(first..first + len) else {
                        return Ok(false);
                    };
                    for fp in fps {
                        let Some(at) = names.iter().position(|row| row.0 == fp.name) else {
                            return Ok(false);
                        };
                        let Some(&fg) = tys.get(at) else {
                            return Ok(false);
                        };
                        todo.push((fp.pat, fg));
                    }
                }
                Pat::Slice { elems, rest } => {
                    let subs = self
                        .body
                        .pat_lists
                        .get(elems.range())
                        .ok_or(malformed("a slice pattern's elements past `pat_lists`"))?;
                    match self.ground_node(g).map_err(|error| Self::pat_ty_err(p, error))? {
                        PNode::Array(elem, _) => {
                            let rest = match rest {
                                Some(r) => Some(usize::try_from(r).map_err(|_| malformed("a slice `..` past `usize`"))?),
                                None => None,
                            };
                            for (j, &sub) in subs.iter().enumerate() {
                                let sg = if rest == Some(j) { self.rest_ty(sub)? } else { elem };
                                todo.push((sub, sg));
                            }
                        }
                        PNode::Slice(_) => {
                            if subs.len() == 1 && rest == Some(0) {
                                todo.push((subs[0], g));
                            } else {
                                return Ok(false);
                            }
                        }
                        _ => return Ok(false),
                    }
                }
            }
        }
        Ok(true)
    }

    /// Whether every arm of `match scrutinee { arms.. }` is non-reading (`pat_does_not_read`).
    pub(super) fn match_arms_do_not_read(&mut self, scrutinee: ExprId, arms: List) -> Result<bool, BuildError> {
        let t = self.adjusted_ty(scrutinee)?;
        let g = self.ground_ty(t).map_err(|error| Self::ty_err(scrutinee, error))?;
        let list = self
            .body
            .arms
            .get(arms.range())
            .ok_or(malformed("a match's arms past `arms`"))?;
        for arm in list {
            if !self.pat_does_not_read(arm.pat, g)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Aligned non-null dangling place for an uncaptured outer ZST local (`Repr::None`): zero
    /// bytes, no drop glue, and allocates no stack slot (safe across coroutine suspension points).
    pub(super) fn uncaptured_zst_place(&mut self, p: PatId) -> Result<(Value, i32), BuildError> {
        let t = *self
            .checked
            .local_ty
            .get(p.ix())
            .ok_or(malformed("a binding without a type in the record"))?;
        let g = self.ground_ty(t).map_err(|error| Self::pat_ty_err(p, error))?;
        let align = self.ls_of(g).map(|id| self.ls.get(id).align).unwrap_or(0);
        let addr = self.b.iconst(Ty::I64, 1u128 << align);
        Ok((self.b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, addr), 0))
    }

    /// For `coroutine.rs` `uncaptured`: whether `local_expr` (an `Expr::Local(p)` inside an
    /// `async` block) stands purely in a non-reading place context (`let _ = p;`, `_ = p;`,
    /// `match p { CharPrefix::Binary => ... }`, `if let Wrap(_) = p { ... }`) and therefore
    /// legitimately has no capture row in `Checked::captures` / `Checked::capture_places`.
    pub(super) fn is_non_reading_local_use(&mut self, local_expr: ExprId) -> Result<bool, BuildError> {
        let mut top = local_expr;
        for _ in 0..=self.body.exprs.len() {
            let Some(&(up, _)) = self.expr_parent.get(top.ix()) else {
                return Ok(false);
            };
            if up == NO_EXPR {
                return Ok(false);
            }
            let up_id = ExprId(up);
            match self.expr(up_id)? {
                Expr::Paren(_)
                | Expr::Field { .. }
                | Expr::TupleField { .. }
                | Expr::Unary { op: UnOp::Deref, .. } => {
                    top = up_id;
                }
                Expr::Index { base, .. } if base == top => {
                    top = up_id;
                }
                Expr::Assign { lhs, rhs } if rhs == top && matches!(self.expr(lhs)?, Expr::Underscore) => {
                    return self.place_chain_has_no_overloaded_deref(top);
                }
                Expr::Block { stmts, .. } => {
                    let ss = self
                        .body
                        .stmt_lists
                        .get(stmts.range())
                        .ok_or(malformed("a block's statements past `stmt_lists`"))?;
                    for &s in ss {
                        if let Stmt::Let { pat, init: Some(init), .. } = self
                            .body
                            .stmts
                            .get(s.ix())
                            .copied()
                            .ok_or(malformed("a statement past `stmts`"))?
                            && init == top
                        {
                            if !self.place_chain_has_no_overloaded_deref(top)? {
                                return Ok(false);
                            }
                            let t = self.adjusted_ty(top)?;
                            let g = self.ground_ty(t).map_err(|error| Self::ty_err(top, error))?;
                            return self.pat_does_not_read(pat, g);
                        }
                    }
                    return Ok(false);
                }
                Expr::Match { scrutinee, arms } if scrutinee == top => {
                    if !self.place_chain_has_no_overloaded_deref(top)? {
                        return Ok(false);
                    }
                    return self.match_arms_do_not_read(scrutinee, arms);
                }
                Expr::Let { pat, init } if init == top => {
                    if !self.place_chain_has_no_overloaded_deref(top)? {
                        return Ok(false);
                    }
                    let t = self.adjusted_ty(top)?;
                    let g = self.ground_ty(t).map_err(|error| Self::ty_err(top, error))?;
                    return self.pat_does_not_read(pat, g);
                }
                _ => return Ok(false),
            }
        }
        Ok(false)
    }
}
