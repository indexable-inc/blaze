//! rustc's desugarings, made as the machine closes the node that needs one: the same shapes
//! `rustc_ast_lowering` builds (its `lower_expr_for`, `lower_expr_while_in_loop_scope`,
//! `lower_expr_try`, `lower_expr_range` and `lower_coroutine_body_with_moved_arguments`), in
//! this crate's columns. A path a desugaring names is a lang item's ([`PathRef::lang`]); the
//! body's set of them is its read list's. `.await` is not desugared: it stays one node
//! ([`Expr::Await`]), since its poll loop is a state machine's, made where code is.
//!
//! Every node made here is written after its children (post-order holds), and every name made
//! here (`iter`, `val`, `residual`, `__argN`) is
//! hygienic: no path of the source finds it, because no binding made here enters the scope
//! map; the desugared expressions name their bindings by pattern directly.
//!
//! Each expansion, as rustc documents it:
//!
//! ```text
//! for PAT in HEAD BODY
//!   => match IntoIterator::into_iter(HEAD) {
//!          mut iter => loop {
//!              match Iterator::next(&mut iter) {
//!                  None => break,
//!                  Some(PAT) => BODY,
//!              }
//!          },
//!      }
//! while COND BODY
//!   => loop { if COND BODY else { break } }
//! EXPR?
//!   => match Try::branch(EXPR) {
//!          ControlFlow::Break(residual) => return FromResidual::from_residual(residual),
//!          ControlFlow::Continue(val) => val,
//!      }
//!      (inside `try { }`: `break 'try FromResidual::from_residual(residual)`)
//! try { STMTS TAIL }
//!   => 'try: { STMTS Try::from_output(TAIL) }   (no tail: `Try::from_output(())`)
//! a..b => Range { start: a, end: b }      a.. => RangeFrom { start: a }
//! ..b  => RangeTo { end: b }              ..  => RangeFull {}
//! a..=b => RangeInclusive::new(a, b)      ..=b => RangeToInclusive { end: b }
//! async fn f(PAT: T) BODY
//!   => fn f(__arg0: T) { async move { let PAT = __arg0; BODY } }, the body's head marked async
//!      (the parameters move into the future; their drop order is the `let`s')
//! ```

use blaze_surface::Range;

use crate::form::*;
use crate::machine::{Machine, idx};

/// A `for` loop's two nodes: the expression it is (the outer `match`) and the loop its
/// `break` and `continue` leave.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForParts {
    pub(crate) outer: ExprId,
    pub(crate) inner_loop: ExprId,
}

impl Machine<'_, '_, '_> {
    /// A path to a lang item, read by the body.
    fn lang_path(&mut self, lang: LangItem, r: Range) -> PathIdx {
        self.out.langs |= lang.bit();
        let segments = List { start: idx(self.out.rows.len()), len: 0 };
        let p = PathRef { segments, absolute: false, scope: Some(self.scope()), lang: Some(lang) };
        self.put_path(p, r)
    }

    fn lang_call(&mut self, lang: LangItem, args: &[ExprId], r: Range) -> ExprId {
        let path = self.lang_path(lang, r);
        let callee = self.put_expr(Expr::Path(path), r);
        let start = idx(self.out.ids.len());
        self.out.ids.extend(args.iter().map(|a| a.0));
        self.put_expr(Expr::Call { callee, args: List { start, len: idx(args.len()) } }, r)
    }

    /// An expression naming a binding made here: a local with a path of no segments.
    pub(crate) fn hygienic_local(&mut self, pat: PatId, r: Range) -> ExprId {
        let segments = List { start: idx(self.out.rows.len()), len: 0 };
        let path = self.put_path(PathRef { segments, absolute: false, scope: Some(self.scope()), lang: None }, r);
        self.put_expr(Expr::Local { pat, path }, r)
    }

    fn hygienic_bind(&mut self, name: &str, mutable: bool, r: Range) -> PatId {
        let name = self.synth_name(name, r);
        let ann = BindAnn { by_ref: None, mutable };
        self.put_pat(Pat::Bind { ann, name, sub: None, shadows: None, ty: None }, r)
    }

    /// `Lang(elems..)` as a pattern.
    fn lang_variant_pat(&mut self, lang: LangItem, elems: &[PatId], r: Range) -> PatId {
        let path = self.lang_path(lang, r);
        if elems.is_empty() {
            return self.put_pat(Pat::Path(path), r);
        }
        let start = idx(self.out.ids.len());
        self.out.ids.extend(elems.iter().map(|p| p.0));
        self.put_pat(Pat::TupleStruct { path, elems: List { start, len: idx(elems.len()) }, rest: None }, r)
    }

    fn arms(&mut self, arms: &[Arm], r: Range) -> List {
        let start = idx(self.out.rows.len());
        for a in arms {
            self.out.rows.push(a.encode());
            self.out.row_ranges.push(r);
        }
        List { start, len: idx(arms.len()) }
    }

    fn block_of(&mut self, stmts: &[StmtId], tail: Option<ExprId>, kind: BlockKind, r: Range) -> ExprId {
        let start = idx(self.out.ids.len());
        self.out.ids.extend(stmts.iter().map(|s| s.0));
        let scope = self.scope();
        self.put_expr(Expr::Block { stmts: List { start, len: idx(stmts.len()) }, tail, scope, kind }, r)
    }

    /// `for PAT in HEAD BODY`; the user's `break`s and `continue`s target `inner_loop`.
    pub(crate) fn for_loop(&mut self, pat: PatId, head: ExprId, body: ExprId, r: Range) -> ForParts {
        let head_r = self.out.expr_ranges[head.0 as usize];
        // `None => break`
        let break_ = if plant_for_drops_break() {
            self.put_expr(Expr::Tuple { elems: List { start: 0, len: 0 } }, r)
        } else {
            self.put_expr(Expr::Break { value: None, target: None }, r)
        };
        let none = self.lang_variant_pat(LangItem::OptionNone, &[], r);
        // `Some(PAT) => BODY`
        let pat_r = self.out.pat_ranges[pat.0 as usize];
        let some = self.lang_variant_pat(LangItem::OptionSome, &[pat], pat_r);
        // `mut iter`
        let iter = self.hygienic_bind("iter", true, head_r);
        // `Iterator::next(&mut iter)`
        let iter_ref = self.hygienic_local(iter, head_r);
        let borrow = self.put_expr(Expr::Ref { mutbl: Mutbl::Mut, raw: false, operand: iter_ref }, head_r);
        let next = self.lang_call(LangItem::IteratorNext, &[borrow], head_r);
        let arms = self.arms(&[Arm { pat: none, guard: None, body: break_ }, Arm { pat: some, guard: None, body }], r);
        let inner = self.put_expr(Expr::Match { scrutinee: next, arms, source: MatchSource::ForLoop }, head_r);
        let stmt = self.put_stmt(Stmt::Expr { expr: inner, semi: false });
        let loop_body = self.block_of(&[stmt], None, BlockKind::Plain, r);
        let inner_loop = self.put_expr(Expr::Loop { body: loop_body, source: LoopSource::ForLoop }, r);
        if let Ok(Expr::Break { value, .. }) = Expr::decode(self.out.exprs[break_.0 as usize]) {
            self.out.exprs[break_.0 as usize] = Expr::Break { value, target: Some(inner_loop) }.encode();
        }
        // `match IntoIterator::into_iter(HEAD) { mut iter => loop { .. } }`
        let into_iter = self.lang_call(LangItem::IntoIterIntoIter, &[head], head_r);
        let arms = self.arms(&[Arm { pat: iter, guard: None, body: inner_loop }], r);
        let outer = self.put_expr(Expr::Match { scrutinee: into_iter, arms, source: MatchSource::ForLoop }, r);
        ForParts { outer, inner_loop }
    }

    /// `while COND BODY`: `loop { if COND BODY else { break } }`. Returns the loop.
    pub(crate) fn while_loop(&mut self, cond: ExprId, body: ExprId, r: Range) -> ExprId {
        let break_ = self.put_expr(Expr::Break { value: None, target: None }, r);
        let stmt = self.put_stmt(Stmt::Expr { expr: break_, semi: false });
        let else_ = self.block_of(&[stmt], None, BlockKind::Plain, r);
        let if_ = self.put_expr(Expr::If { cond, then: body, else_: Some(else_) }, r);
        let block = self.block_of(&[], Some(if_), BlockKind::Plain, r);
        let lp = self.put_expr(Expr::Loop { body: block, source: LoopSource::While }, r);
        self.out.exprs[break_.0 as usize] = Expr::Break { value: None, target: Some(lp) }.encode();
        lp
    }

    /// A `try` block's tail: `Try::from_output(TAIL)`, `Try::from_output(())` with none.
    pub(crate) fn try_block_tail(&mut self, tail: Option<ExprId>, r: Range) -> ExprId {
        let value = match tail {
            Some(t) => t,
            None => self.put_expr(Expr::Tuple { elems: List { start: 0, len: 0 } }, r),
        };
        self.lang_call(LangItem::TryTraitFromOutput, &[value], r)
    }

    /// `EXPR?`.
    pub(crate) fn try_op(&mut self, e: ExprId, r: Range) -> ExprId {
        let branch = self.lang_call(LangItem::TryTraitBranch, &[e], r);
        // `ControlFlow::Break(residual) => return FromResidual::from_residual(residual)`
        let residual = self.hygienic_bind("residual", false, r);
        let break_pat = self.lang_variant_pat(LangItem::ControlFlowBreak, &[residual], r);
        let residual_ref = self.hygienic_local(residual, r);
        let from = self.lang_call(LangItem::TryTraitFromResidual, &[residual_ref], r);
        let leave = match self.try_entry() {
            Some(entry) => {
                let b = self.put_expr(Expr::Break { value: Some(from), target: None }, r);
                self.add_jump(entry, b);
                b
            }
            None => self.put_expr(Expr::Return { value: Some(from) }, r),
        };
        // `ControlFlow::Continue(val) => val`
        let val = self.hygienic_bind("val", false, r);
        let continue_pat = self.lang_variant_pat(LangItem::ControlFlowContinue, &[val], r);
        let val_ref = self.hygienic_local(val, r);
        let arms = self.arms(
            &[Arm { pat: break_pat, guard: None, body: leave }, Arm { pat: continue_pat, guard: None, body: val_ref }],
            r,
        );
        self.put_expr(Expr::Match { scrutinee: branch, arms, source: MatchSource::Try }, r)
    }

    /// A range expression, by which ends it has.
    pub(crate) fn range_expr(&mut self, lo: Option<ExprId>, hi: Option<ExprId>, inclusive: bool, r: Range) -> ExprId {
        let lang = match (lo, hi, inclusive) {
            (None, None, false) => LangItem::RangeFull,
            (Some(_), None, false) => LangItem::RangeFrom,
            (None, Some(_), false) => LangItem::RangeTo,
            (Some(_), Some(_), false) => LangItem::Range,
            (None, Some(_), true) => LangItem::RangeToInclusive,
            (Some(lo), Some(hi), true) => return self.lang_call(LangItem::RangeInclusiveNew, &[lo, hi], r),
            (start, None, true) => {
                // E0586; rustc goes on with the half-open range, and so does the lowering.
                self.diag(DiagKind::InclusiveRangeNoEnd, r);
                if start.is_some() { LangItem::RangeFrom } else { LangItem::RangeFull }
            }
        };
        let path = self.lang_path(lang, r);
        let mut inits = [FieldInit { name: NameIdx(0), expr: ExprId(0), shorthand: false }; 2];
        let mut n = 0;
        for (field, e) in [("start", lo), ("end", hi)] {
            if let Some(e) = e {
                let er = self.out.expr_ranges[e.0 as usize];
                inits[n] = FieldInit { name: self.synth_name(field, er), expr: e, shorthand: false };
                n += 1;
            }
        }
        let start = idx(self.out.rows.len());
        for init in &inits[..n] {
            self.out.rows.push(init.encode());
            self.out.row_ranges.push(r);
        }
        self.put_expr(Expr::Struct { path, fields: List { start, len: idx(n) }, base: StructBase::None }, r)
    }

    /// `EXPR.await`.
    ///
    /// One node: what it means (`IntoFuture::into_future`, then polling at a suspension point)
    /// is the checker's to type and the code builder's to make into a state machine; spelling
    /// rustc's poll loop here would make every later stage read it back out.
    pub(crate) fn await_op(&mut self, e: ExprId, r: Range) -> ExprId {
        if !self.in_async() {
            // E0728.
            self.diag(DiagKind::AwaitOutsideAsync, r);
            return e;
        }
        self.put_expr(Expr::Await(e), r)
    }

    /// An `async fn`'s body: its parameters (`pats`, the written patterns) become the coroutine's
    /// moved arguments. Returns the body's root and the fn's parameter patterns, in `ids`.
    pub(crate) fn async_fn(&mut self, pats: &[u32], body: ExprId, r: Range) -> (ExprId, List) {
        let mut stmts: blaze_own::AVec<'_, StmtId> = blaze_own::Vec::new_in(*self.vals.allocator());
        let mut params: blaze_own::AVec<'_, u32> = blaze_own::Vec::new_in(*self.vals.allocator());
        for (n, &orig) in pats.iter().enumerate() {
            let Some(orig) = unopt(orig).map(PatId) else {
                params.push(NONE);
                continue;
            };
            let pr = self.out.pat_ranges[orig.0 as usize];
            let simple = match Pat::decode(self.out.pats[orig.0 as usize]) {
                Ok(Pat::Bind { ann: BindAnn { by_ref: None, .. }, name, sub: None, .. }) => Some(name),
                _ => None,
            };
            match simple {
                // `x: T` => `x: T`, then `let x = x;`, the written pattern kept.
                Some(name) => {
                    let ann = BindAnn { by_ref: None, mutable: false };
                    let param = self.put_pat(Pat::Bind { ann, name, sub: None, shadows: None, ty: None }, pr);
                    let init = self.hygienic_local(param, pr);
                    stmts.push(self.put_stmt(Stmt::Let { pat: orig, ty: None, init: Some(init), else_: None }));
                    params.push(param.0);
                }
                // `PAT: T` => `__argN: T`, then `let mut __argN = __argN; let PAT = __argN;`.
                None => {
                    let mut text = blaze_own::String::new_in(*self.vals.allocator());
                    let _ = core::fmt::Write::write_fmt(&mut text, format_args!("__arg{n}"));
                    let param = self.hygienic_bind(text.as_str(), false, pr);
                    let moved = self.hygienic_bind(text.as_str(), true, pr);
                    let init = self.hygienic_local(param, pr);
                    stmts.push(self.put_stmt(Stmt::Let { pat: moved, ty: None, init: Some(init), else_: None }));
                    let init = self.hygienic_local(moved, pr);
                    stmts.push(self.put_stmt(Stmt::Let { pat: orig, ty: None, init: Some(init), else_: None }));
                    params.push(param.0);
                }
            }
        }
        // The Reference, "Async functions": "async fn example<'a>(x: &'a str) -> usize { .. } is
        // roughly equivalent to: fn example<'a>(x: &'a str) -> impl Future<Output = usize> + 'a
        // { async move { .. } }". The root is that `async move` block (the same node a written
        // one lowers to: `Machine::block`), holding the parameters' `let`s and the written body.
        // The head still says the fn is async (`sig::ASYNC`) and keeps the written return type
        // at `head::TY`: what the block's output is checked against.
        let inner = self.block_of(&stmts, Some(body), BlockKind::Plain, r);
        let c = Closure {
            params: List { start: 0, len: 0 },
            ret: None,
            body: inner,
            kind: CoroutineKind::Async,
            source: CoroutineSource::Block,
            capture_by_value: true,
            konst: false,
            immovable: false,
        };
        let row = self.put_row(c.encode(), r);
        let root = self.put_expr(Expr::Closure { row }, r);
        let params = self.put_ids_of(&params);
        (root, params)
    }

    /// `ASSIGNEE = rhs` where the assignee is a tuple, a slice, a tuple struct or a struct:
    /// `{ let PAT = rhs; place_1 = __lhs; .. }` (the Reference, "Destructuring assignments"),
    /// each place of the assignee replaced in `PAT` by a fresh binding, `_` by `_` and `..` by
    /// `..`, the assignments in the order the places are written. The left side's nodes are
    /// walked breadth first (a node's children consecutive), the patterns made from the last
    /// node back, so every pattern is written after its children.
    pub(crate) fn destructuring_assign(&mut self, lhs: ExprId, rhs: ExprId) -> Expr {
        let alloc = *self.vals.allocator();
        let mut nodes: blaze_own::AVec<'_, Assignee> = blaze_own::Vec::new_in(alloc);
        nodes.push(self.assignee(lhs, Within::Other));
        let mut i = 0;
        while i < nodes.len() {
            let n = nodes[i];
            let within = match n.shape {
                Shape::Tuple | Shape::Slice | Shape::TupleStruct(_) => Within::Seq,
                _ => Within::Other,
            };
            let first = idx(nodes.len());
            for k in 0..n.src.len {
                let at = (n.src.start + k) as usize;
                let kid = match n.shape {
                    Shape::Struct { .. } => match FieldInit::decode(self.out.rows[at]) {
                        Ok(init) => init.expr,
                        Err(_) => continue,
                    },
                    _ => ExprId(self.out.ids[at]),
                };
                let mut a = self.assignee(kid, within);
                a.index = k;
                nodes.push(a);
            }
            nodes[i].first = first;
            nodes[i].kids = idx(nodes.len()) - first;
            i += 1;
        }
        let mut pats: blaze_own::AVec<'_, PatId> = blaze_own::Vec::new_in(alloc);
        pats.resize(nodes.len(), PatId(0));
        let mut kids: blaze_own::AVec<'_, u32> = blaze_own::Vec::new_in(alloc);
        for i in (0..nodes.len()).rev() {
            let n = nodes[i];
            let er = self.out.expr_ranges[n.expr.0 as usize];
            let below = &nodes[n.first as usize..(n.first + n.kids) as usize];
            let below_pats = &pats[n.first as usize..(n.first + n.kids) as usize];
            kids.clear();
            let mut rest = None;
            for (k, p) in below.iter().zip(below_pats) {
                match (n.shape, k.shape) {
                    (Shape::Tuple | Shape::TupleStruct(_), Shape::Rest) => {
                        rest = rest.or(Some(idx(kids.len())));
                    }
                    (Shape::Slice, Shape::Rest) => {
                        rest = rest.or(Some(idx(kids.len())));
                        kids.push(p.0);
                    }
                    _ => kids.push(p.0),
                }
            }
            let pat = match n.shape {
                Shape::Place => self.hygienic_bind("lhs", false, er),
                Shape::Wild => self.put_pat(Pat::Wild, er),
                Shape::Rest => self.put_pat(Pat::Rest, er),
                Shape::Tuple => {
                    let elems = self.put_ids_of(&kids);
                    self.put_pat(Pat::Tuple { elems, rest }, er)
                }
                Shape::Slice => {
                    let elems = self.put_ids_of(&kids);
                    self.put_pat(Pat::Slice { elems, rest }, er)
                }
                Shape::TupleStruct(path) => {
                    let elems = self.put_ids_of(&kids);
                    self.put_pat(Pat::TupleStruct { path, elems, rest }, er)
                }
                Shape::Struct { path, rest } => {
                    let start = idx(self.out.rows.len());
                    for (k, p) in below.iter().zip(below_pats) {
                        let Ok(init) = FieldInit::decode(self.out.rows[(n.src.start + k.index) as usize]) else {
                            continue;
                        };
                        let kr = self.out.expr_ranges[k.expr.0 as usize];
                        self.out.rows.push(FieldPat { name: init.name, pat: *p, shorthand: false }.encode());
                        self.out.row_ranges.push(kr);
                    }
                    let fields = List { start, len: idx(self.out.rows.len()) - start };
                    self.put_pat(Pat::Struct { path, fields, rest }, er)
                }
            };
            pats[i] = pat;
        }
        // The `let`, then each place's assignment, in the order written: a preorder walk.
        let mut stmts: blaze_own::AVec<'_, u32> = blaze_own::Vec::new_in(alloc);
        stmts.push(self.put_stmt(Stmt::Let { pat: pats[0], ty: None, init: Some(rhs), else_: None }).0);
        let mut stack: blaze_own::AVec<'_, u32> = blaze_own::Vec::new_in(alloc);
        stack.push(0);
        while let Some(i) = stack.pop() {
            let n = nodes[i as usize];
            if let Shape::Place = n.shape {
                let er = self.out.expr_ranges[n.expr.0 as usize];
                let value = self.hygienic_local(pats[i as usize], er);
                let assign = self.put_expr(Expr::Assign { lhs: n.expr, rhs: value }, er);
                stmts.push(self.put_stmt(Stmt::Expr { expr: assign, semi: true }).0);
            }
            stack.extend((n.first..n.first + n.kids).rev());
        }
        // The assignee is not a value (the Reference, "Assignee expressions"): its own nodes (the
        // tuple, the struct, a tuple struct's callee path, `_`, `..`, every parenthesis around a
        // node) are now the pattern's and no root reaches them. Each becomes `Missing` so the
        // passes that read every node in order (the conversion to the checker's body, the code
        // builder's scans) see no value there; a place is kept, read by its assignment alone.
        for n in nodes.iter() {
            let mut cur = n.outer;
            for _ in 0..=self.out.exprs.len() {
                if cur == n.expr {
                    break;
                }
                let Ok(Expr::Paren(inner)) = Expr::decode(self.out.exprs[cur.0 as usize]) else { break };
                self.out.exprs[cur.0 as usize] = Expr::Missing.encode();
                cur = inner;
            }
            if matches!(n.shape, Shape::Place) {
                continue;
            }
            if let Ok(Expr::Call { callee, .. }) = Expr::decode(self.out.exprs[n.expr.0 as usize]) {
                self.out.exprs[callee.0 as usize] = Expr::Missing.encode();
            }
            self.out.exprs[n.expr.0 as usize] = Expr::Missing.encode();
        }
        let stmts = self.put_ids_of(&stmts);
        Expr::Block { stmts, tail: None, scope: self.scope(), kind: BlockKind::Plain }
    }

    /// What one node of an assignee is (parentheses looked through).
    fn assignee(&mut self, e: ExprId, within: Within) -> Assignee {
        let mut cur = e;
        let mut shape = Shape::Place;
        let mut src = List { start: 0, len: 0 };
        for _ in 0..=self.out.exprs.len() {
            match Expr::decode(self.out.exprs[cur.0 as usize]) {
                Ok(Expr::Paren(inner)) => {
                    cur = inner;
                    continue;
                }
                Ok(Expr::Tuple { elems }) => (shape, src) = (Shape::Tuple, elems),
                Ok(Expr::Array { elems }) => (shape, src) = (Shape::Slice, elems),
                Ok(Expr::Call { callee, args }) => {
                    if let Ok(Expr::Path(path)) = Expr::decode(self.out.exprs[callee.0 as usize]) {
                        (shape, src) = (Shape::TupleStruct(path), args);
                    }
                }
                Ok(Expr::Underscore) => shape = Shape::Wild,
                Ok(Expr::Struct { path, fields, base }) => {
                    let range_full = PathRef::decode(self.out.paths[path.0 as usize])
                        .is_ok_and(|p| p.lang == Some(LangItem::RangeFull));
                    if range_full && fields.len == 0 {
                        // `..`: a rest only among a tuple's, a slice's or a tuple struct's.
                        if within == Within::Seq {
                            shape = Shape::Rest;
                        }
                    } else {
                        let rest = match base {
                            StructBase::None => false,
                            StructBase::Defaults => true,
                            StructBase::Expr(b) => {
                                // rustc: "functional record updates are not allowed in
                                // destructuring assignments".
                                let br = self.out.expr_ranges[b.0 as usize];
                                self.diag(DiagKind::FruInDestructuringAssignment, br);
                                true
                            }
                        };
                        (shape, src) = (Shape::Struct { path, rest }, fields);
                    }
                }
                _ => {}
            }
            break;
        }
        Assignee { outer: e, expr: cur, shape, src, first: 0, kids: 0, index: 0 }
    }
}

/// Where an assignee node stands: `..` is a rest only in a sequence.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Within {
    Seq,
    Other,
}

/// One node of a destructuring assignment's left side; its children are `kids` nodes from
/// `first`, made from `src` (ids, or for a struct rows of [`FieldInit`]).
#[derive(Clone, Copy)]
struct Assignee {
    /// The node as written, parentheses and all; `expr` is the node inside them.
    outer: ExprId,
    expr: ExprId,
    shape: Shape,
    src: List,
    first: u32,
    kids: u32,
    /// Its position in its parent's `src`.
    index: u32,
}

#[derive(Clone, Copy)]
enum Shape {
    /// A place expression: a fresh binding in the pattern, then `place = binding`.
    Place,
    Wild,
    Rest,
    Tuple,
    Slice,
    TupleStruct(PathIdx),
    Struct { path: PathIdx, rest: bool },
}

#[cfg(test)]
fn plant_for_drops_break() -> bool {
    crate::plant::on(crate::plant::Plant::ForDropsBreak)
}

#[cfg(not(test))]
const fn plant_for_drops_break() -> bool {
    false
}
