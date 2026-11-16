//! A body record to blaze-typeck's body form ([`blaze_typeck::BodyView`]): one pass over the
//! columns, each node converted where it stands (indices of expressions, patterns and
//! statements kept), each list copied into the side column blaze-typeck reads it from.
//!
//! What blaze-typeck's form has no place for is refused by name ([`Refused::Form`]), never
//! dropped: a lang-item path (every desugaring's), `let` in a condition, a break or continue
//! with no target (an error already reported), `become`, `yield` and coroutines, raw borrows,
//! `const` and `try` blocks, struct defaults (`..` alone), box, range and rest patterns, a
//! slice pattern with `..`, `b'x'` and `c"..."` literals, format arguments, macro calls,
//! inline assembly, `offset_of!`, `_` as an expression, path anchors (`<T as Tr>::x`) and
//! lifetime arguments. What is dropped because it changes no type: the match and loop
//! sources (diagnostics only), the shadowing links and the hygiene of made names.
//!
//! Types are the caller's ([`Types`]): a written type becomes a portable one only by
//! resolving its paths, which is blaze-scope's work, not this crate's.

use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck as tc;
use blaze_typeck::{CheckError, ItemKey, Name, PTy, PTys, PortableTys};

use crate::columns::{View, head};
use crate::form::*;

/// Why a body has no blaze-typeck form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A form blaze-typeck's body has no place for, and where it is (item-relative).
    Form { what: &'static str, range: blaze_surface::Range },
    /// A row that is no form of its column.
    Bad(BadRow),
    /// blaze-typeck's own table refused (a limit).
    Check(CheckError),
    /// The record is no body (a signature, or an item with none).
    NoBody,
}

impl From<BadRow> for Refused {
    fn from(b: BadRow) -> Refused {
        Refused::Bad(b)
    }
}

impl From<CheckError> for Refused {
    fn from(e: CheckError) -> Refused {
        Refused::Check(e)
    }
}

/// The caller's types: a written type to a portable one, an array length to its value.
pub trait Types {
    /// The portable type of the written type `t` of `view` (whose text is `text`).
    fn ty(&mut self, view: &View<'_>, text: &str, t: TypeId, tys: &mut PortableTys<'_>) -> Result<PTy, Refused>;
    /// The value of the repeat length `len` (an expression of `view`).
    fn repeat_len(&mut self, view: &View<'_>, text: &str, len: ExprId) -> Result<u64, Refused>;
}

/// A body in blaze-typeck's form: the columns [`TypeckBody::view`] lends.
pub struct TypeckBody<'a> {
    pub owner: ItemKey,
    pub edition: tc::Edition,
    pub params: AVec<'a, tc::PatId>,
    pub root: ExprId,
    pub exprs: AVec<'a, tc::Expr>,
    pub expr_ranges: AVec<'a, tc::Range>,
    pub pats: AVec<'a, tc::Pat>,
    pub pat_ranges: AVec<'a, tc::Range>,
    pub stmts: AVec<'a, tc::Stmt>,
    pub expr_lists: AVec<'a, ExprId>,
    pub pat_lists: AVec<'a, PatId>,
    pub stmt_lists: AVec<'a, StmtId>,
    pub arms: AVec<'a, tc::Arm>,
    pub field_inits: AVec<'a, tc::FieldInit>,
    pub field_pats: AVec<'a, tc::FieldPat>,
    pub paths: AVec<'a, tc::BodyPath>,
    pub segments: AVec<'a, tc::Segment>,
    pub type_lists: AVec<'a, PTy>,
    pub opt_types: AVec<'a, Option<PTy>>,
}

impl<'a> TypeckBody<'a> {
    /// The body as blaze-typeck reads it, its types in `types` (the table [`to_typeck`] filled).
    pub fn view<'b>(&'b self, types: PTys<'b>) -> tc::BodyView<'b> {
        tc::BodyView {
            owner: self.owner,
            edition: self.edition,
            params: &self.params,
            root: self.root,
            exprs: &self.exprs,
            expr_ranges: &self.expr_ranges,
            pats: &self.pats,
            pat_ranges: &self.pat_ranges,
            stmts: &self.stmts,
            expr_lists: &self.expr_lists,
            pat_lists: &self.pat_lists,
            stmt_lists: &self.stmt_lists,
            arms: &self.arms,
            field_inits: &self.field_inits,
            field_pats: &self.field_pats,
            paths: &self.paths,
            segments: &self.segments,
            types,
            type_lists: &self.type_lists,
            opt_types: &self.opt_types,
        }
    }
}

const fn range(r: blaze_surface::Range) -> tc::Range {
    tc::Range { start: r.start, end: r.end }
}

const fn edition(e: blaze_surface::Edition) -> tc::Edition {
    match e {
        blaze_surface::Edition::E2015 => tc::Edition::E2015,
        blaze_surface::Edition::E2018 => tc::Edition::E2018,
        blaze_surface::Edition::E2021 => tc::Edition::E2021,
        blaze_surface::Edition::E2024 => tc::Edition::E2024,
    }
}

fn len(n: usize) -> u32 {
    u32::try_from(n).expect("a body's column under 4G rows")
}

struct Conv<'v, 't, 'x, 'y, 'a> {
    v: &'v View<'v>,
    text: &'v str,
    tys: &'x mut PortableTys<'y>,
    types: &'t mut dyn Types,
    out: TypeckBody<'a>,
}

impl Conv<'_, '_, '_, '_, '_> {
    fn refuse<T>(&self, what: &'static str, r: blaze_surface::Range) -> Result<T, Refused> {
        Err(Refused::Form { what, range: r })
    }

    fn name(&self, n: NameIdx) -> Result<Name, Refused> {
        Ok(Name(self.v.name(n)?))
    }

    fn ty(&mut self, t: TypeId) -> Result<PTy, Refused> {
        self.types.ty(self.v, self.text, t, self.tys)
    }

    fn opt_ty(&mut self, t: Option<TypeId>) -> Result<Option<PTy>, Refused> {
        t.map(|t| self.ty(t)).transpose()
    }

    fn exprs(&mut self, l: List) -> Result<List, Refused> {
        let ids = self.v.ids(l)?;
        let start = len(self.out.expr_lists.len());
        self.out.expr_lists.extend(ids.iter().map(|&i| ExprId(i)));
        Ok(List { start, len: l.len })
    }

    fn pats(&mut self, l: List) -> Result<List, Refused> {
        let ids = self.v.ids(l)?;
        let start = len(self.out.pat_lists.len());
        self.out.pat_lists.extend(ids.iter().map(|&i| PatId(i)));
        Ok(List { start, len: l.len })
    }

    fn stmts(&mut self, l: List) -> Result<List, Refused> {
        let ids = self.v.ids(l)?;
        let start = len(self.out.stmt_lists.len());
        self.out.stmt_lists.extend(ids.iter().map(|&i| StmtId(i)));
        Ok(List { start, len: l.len })
    }

    /// Generic arguments of a segment or a method call: types only.
    fn type_args(&mut self, l: List, r: blaze_surface::Range) -> Result<List, Refused> {
        let rows = self.v.rows(l)?;
        let mut at: AVec<'_, PTy> = Vec::new_in(*self.out.type_lists.allocator());
        for &n in rows {
            match GenericArg::decode(n)? {
                GenericArg::Type(t) => at.push(self.ty(t)?),
                GenericArg::Lifetime(_) => return self.refuse("a lifetime argument", r),
                GenericArg::Const(_) => return self.refuse("a const argument", r),
                _ => return self.refuse("an associated item constraint", r),
            }
        }
        let start = len(self.out.type_lists.len());
        self.out.type_lists.extend_from_slice(&at);
        Ok(List { start, len: len(at.len()) })
    }

    fn path(&mut self, p: PathIdx) -> Result<PathIdx, Refused> {
        let r = self.v.path_ranges.get(p.0 as usize).copied().unwrap_or_default();
        let path = self.v.path(p)?;
        if path.lang.is_some() {
            return self.refuse("a lang item's path (a desugaring's)", r);
        }
        if path.absolute {
            return self.refuse("a path from the crate root (`::x`)", r);
        }
        let Some(scope) = path.scope else { return self.refuse("a path with no scope", r) };
        let mut segs: AVec<'_, tc::Segment> = Vec::new_in(*self.out.segments.allocator());
        for &n in self.v.segments(path)? {
            let s = Segment::decode(n)?;
            let name = match (s.kind, s.name) {
                (SegKind::Ident, Some(n)) => self.name(n)?,
                (SegKind::SelfMod, _) => Name::of("self"),
                (SegKind::Super, _) => Name::of("super"),
                (SegKind::Crate, _) => Name::of("crate"),
                (SegKind::SelfTy, _) => Name::of("Self"),
                (SegKind::DollarCrate, _) => Name::of("$crate"),
                (SegKind::Anchor, _) => return self.refuse("a path anchor (`<T as Tr>::x`)", r),
                (SegKind::Ident, None) => return self.refuse("a segment with no name", r),
            };
            if s.parenthesized || s.return_notation {
                return self.refuse("parenthesized arguments in a body path", r);
            }
            let generic_args = if s.has_args { self.type_args(s.args, r)? } else { List::EMPTY };
            segs.push(tc::Segment { name, generic_args, has_args: s.has_args });
        }
        let start = len(self.out.segments.len());
        self.out.segments.extend_from_slice(&segs);
        let at = PathIdx(len(self.out.paths.len()));
        self.out.paths.push(tc::BodyPath { scope, segments: List { start, len: len(segs.len()) } });
        Ok(at)
    }

    fn lit(&self, l: Lit, r: blaze_surface::Range) -> Result<tc::Lit, Refused> {
        Ok(match l {
            Lit::Bool(b) => tc::Lit::Bool(b),
            Lit::Char => tc::Lit::Char,
            Lit::Str => tc::Lit::Str,
            Lit::ByteStr(n) => tc::Lit::ByteStr(n),
            Lit::Int(s) => tc::Lit::Int(s),
            Lit::Float(f) => tc::Lit::Float(f),
            Lit::Byte => return self.refuse("a byte literal", r),
            Lit::CStr => return self.refuse("a C string literal", r),
        })
    }

    fn expr(&mut self, e: ExprId) -> Result<tc::Expr, Refused> {
        let r = self.v.expr_range(e);
        Ok(match self.v.expr(e)? {
            Expr::Missing => tc::Expr::Missing,
            Expr::Lit(l) => tc::Expr::Lit(self.lit(l, r)?),
            Expr::Local { pat, .. } => tc::Expr::Local(pat),
            Expr::Path(p) => tc::Expr::Path(self.path(p)?),
            Expr::Paren(i) => tc::Expr::Paren(i),
            Expr::Block { stmts, tail, scope, kind } => match kind {
                BlockKind::Plain | BlockKind::Unsafe => tc::Expr::Block { stmts: self.stmts(stmts)?, tail, scope },
                BlockKind::Const => return self.refuse("a const block", r),
                BlockKind::Try => return self.refuse("a try block", r),
            },
            Expr::If { cond, then, else_ } => tc::Expr::If { cond, then, else_ },
            Expr::Let { .. } => return self.refuse("`let` in a condition", r),
            Expr::Match { scrutinee, arms, .. } => {
                let start = len(self.out.arms.len());
                for &n in self.v.rows(arms)? {
                    let a = Arm::decode(n)?;
                    self.out.arms.push(tc::Arm { pat: a.pat, guard: a.guard, body: a.body });
                }
                tc::Expr::Match { scrutinee, arms: List { start, len: arms.len } }
            }
            Expr::Loop { body, .. } => tc::Expr::Loop { body },
            Expr::Break { value, target: Some(target) } => tc::Expr::Break { value, target },
            Expr::Break { target: None, .. } => return self.refuse("a break with no target", r),
            Expr::Continue { target: Some(target) } => tc::Expr::Continue { target },
            Expr::Continue { target: None } => return self.refuse("a continue with no target", r),
            Expr::Return { value } => tc::Expr::Return { value },
            Expr::Become { .. } => return self.refuse("`become`", r),
            Expr::Yield { .. } => return self.refuse("`yield`", r),
            Expr::Call { callee, args } => tc::Expr::Call { callee, args: self.exprs(args)? },
            Expr::MethodCall { row, scope } => {
                let m = MethodCall::decode(self.v.row(row)?)?;
                let generic_args = if m.has_args { self.type_args(m.generic_args, r)? } else { List::EMPTY };
                tc::Expr::MethodCall {
                    receiver: m.receiver,
                    name: self.name(m.name)?,
                    generic_args,
                    args: self.exprs(m.args)?,
                    scope,
                }
            }
            Expr::Field { base, name } => tc::Expr::Field { base, name: self.name(name)? },
            Expr::TupleField { base, index } => tc::Expr::TupleField { base, index },
            Expr::Unary { op, operand } => tc::Expr::Unary { op, operand },
            Expr::Binary { op, lhs, rhs } => tc::Expr::Binary { op, lhs, rhs },
            Expr::Assign { lhs, rhs } => tc::Expr::Assign { lhs, rhs },
            Expr::AssignOp { op, lhs, rhs } => tc::Expr::AssignOp { op, lhs, rhs },
            Expr::Ref { raw: true, .. } => return self.refuse("a raw borrow", r),
            Expr::Ref { mutbl, operand, raw: false } => tc::Expr::Ref { mutbl, operand },
            Expr::Cast { operand, ty } => tc::Expr::Cast { operand, ty: self.ty(ty)? },
            Expr::Tuple { elems } => tc::Expr::Tuple { elems: self.exprs(elems)? },
            Expr::Array { elems } => tc::Expr::Array { elems: self.exprs(elems)? },
            Expr::Repeat { elem, len } => tc::Expr::Repeat { elem, len: self.types.repeat_len(self.v, self.text, len)? },
            Expr::Struct { path, fields, base } => {
                let base = match base {
                    StructBase::None => None,
                    StructBase::Expr(b) => Some(b),
                    StructBase::Defaults => return self.refuse("struct field defaults (`..`)", r),
                };
                let path = self.path(path)?;
                let start = len(self.out.field_inits.len());
                for &n in self.v.rows(fields)? {
                    let f = FieldInit::decode(n)?;
                    let name = self.name(f.name)?;
                    self.out.field_inits.push(tc::FieldInit { name, expr: f.expr });
                }
                tc::Expr::Struct { path, fields: List { start, len: fields.len }, base }
            }
            Expr::Index { base, index } => tc::Expr::Index { base, index },
            Expr::Closure { row } => {
                let c = Closure::decode(self.v.row(row)?)?;
                if c.kind != CoroutineKind::None {
                    return self.refuse("a coroutine (async or gen)", r);
                }
                let pstart = len(self.out.pat_lists.len());
                let tstart = len(self.out.opt_types.len());
                for &n in self.v.rows(c.params)? {
                    let p = Param::decode(n)?;
                    let Some(pat) = p.pat else { return self.refuse("a closure parameter with no pattern", r) };
                    let ty = self.opt_ty(p.ty)?;
                    self.out.pat_lists.push(pat);
                    self.out.opt_types.push(ty);
                }
                let ret = self.opt_ty(c.ret)?;
                tc::Expr::Closure {
                    params: List { start: pstart, len: c.params.len },
                    param_tys: List { start: tstart, len: c.params.len },
                    ret,
                    body: c.body,
                }
            }
            Expr::FormatArgs { .. } => return self.refuse("format arguments", r),
            Expr::MacroCall { .. } => return self.refuse("a macro call", r),
            Expr::Asm => return self.refuse("inline assembly", r),
            Expr::OffsetOf { .. } => return self.refuse("`offset_of!`", r),
            Expr::Underscore => return self.refuse("`_` as an expression", r),
        })
    }

    fn pat(&mut self, p: PatId) -> Result<tc::Pat, Refused> {
        let r = self.v.pat_ranges.get(p.0 as usize).copied().unwrap_or_default();
        Ok(match self.v.pat(p)? {
            Pat::Missing => tc::Pat::Missing,
            Pat::Wild => tc::Pat::Wild,
            Pat::Bind { ann, name, sub, .. } => tc::Pat::Bind { ann, name: self.name(name)?, sub },
            Pat::Tuple { elems, rest } => tc::Pat::Tuple { elems: self.pats(elems)?, rest },
            Pat::Ref { mutbl, inner } => tc::Pat::Ref { mutbl, inner },
            Pat::Lit(l) => tc::Pat::Lit(self.lit(l, r)?),
            Pat::NegLit(l) => tc::Pat::NegLit(self.lit(l, r)?),
            Pat::Path(x) => tc::Pat::Path(self.path(x)?),
            Pat::TupleStruct { path, elems, rest } => {
                tc::Pat::TupleStruct { path: self.path(path)?, elems: self.pats(elems)?, rest }
            }
            Pat::Struct { path, fields, rest } => {
                let path = self.path(path)?;
                let start = len(self.out.field_pats.len());
                for &n in self.v.rows(fields)? {
                    let f = FieldPat::decode(n)?;
                    let name = self.name(f.name)?;
                    self.out.field_pats.push(tc::FieldPat { name, pat: f.pat });
                }
                tc::Pat::Struct { path, fields: List { start, len: fields.len }, rest }
            }
            Pat::Or { alts } => tc::Pat::Or { alts: self.pats(alts)? },
            Pat::Slice { elems, rest: None } => tc::Pat::Slice { elems: self.pats(elems)? },
            Pat::Slice { rest: Some(_), .. } => return self.refuse("a slice pattern with `..`", r),
            Pat::Box(_) => return self.refuse("a box pattern", r),
            Pat::Range { .. } => return self.refuse("a range pattern", r),
            Pat::Rest => return self.refuse("a rest pattern", r),
            Pat::MacroCall { .. } => return self.refuse("a macro call", r),
        })
    }

    fn stmt(&mut self, s: StmtId) -> Result<tc::Stmt, Refused> {
        Ok(match self.v.stmt(s)? {
            Stmt::Let { pat, ty, init, else_ } => tc::Stmt::Let { pat, ty: self.opt_ty(ty)?, init, else_ },
            Stmt::Expr { expr, semi } => tc::Stmt::Expr { expr, semi },
            Stmt::Item { .. } => tc::Stmt::Item,
        })
    }
}

/// Converts the body record `view` (of the item `text`, filed as `owner`) to blaze-typeck's
/// form, its written types interned into `tys` by `types`; every column in `alloc`.
pub fn to_typeck<'a>(
    view: &View<'_>,
    text: &str,
    owner: ItemKey,
    tys: &mut PortableTys<'_>,
    types: &mut dyn Types,
    alloc: Alloc<'a>,
) -> Result<TypeckBody<'a>, Refused> {
    if view.head[head::MODE] == 0 {
        return Err(Refused::NoBody);
    }
    let Some((root, params)) = view.body() else { return Err(Refused::NoBody) };
    let ed = match view.head[head::EDITION] {
        0 => blaze_surface::Edition::E2015,
        1 => blaze_surface::Edition::E2018,
        2 => blaze_surface::Edition::E2021,
        _ => blaze_surface::Edition::E2024,
    };
    let out = TypeckBody {
        owner,
        edition: edition(ed),
        params: Vec::new_in(alloc),
        root,
        exprs: Vec::with_capacity_in(view.exprs.len(), alloc),
        expr_ranges: Vec::from_iter_in(view.expr_ranges.iter().map(|r| range(*r)), alloc),
        pats: Vec::with_capacity_in(view.pats.len(), alloc),
        pat_ranges: Vec::from_iter_in(view.pat_ranges.iter().map(|r| range(*r)), alloc),
        stmts: Vec::with_capacity_in(view.stmts.len(), alloc),
        expr_lists: Vec::new_in(alloc),
        pat_lists: Vec::new_in(alloc),
        stmt_lists: Vec::new_in(alloc),
        arms: Vec::new_in(alloc),
        field_inits: Vec::new_in(alloc),
        field_pats: Vec::new_in(alloc),
        paths: Vec::new_in(alloc),
        segments: Vec::new_in(alloc),
        type_lists: Vec::new_in(alloc),
        opt_types: Vec::new_in(alloc),
    };
    let mut c = Conv { v: view, text, tys, types, out };
    for &p in params {
        match unopt(p) {
            Some(p) => c.out.params.push(PatId(p)),
            None => return Err(Refused::Form { what: "a parameter with no pattern", range: Default::default() }),
        }
    }
    for i in 0..view.exprs.len() {
        let e = c.expr(ExprId(len(i)))?;
        c.out.exprs.push(e);
    }
    for i in 0..view.pats.len() {
        let p = c.pat(PatId(len(i)))?;
        c.out.pats.push(p);
    }
    for i in 0..view.stmts.len() {
        let s = c.stmt(StmtId(len(i)))?;
        c.out.stmts.push(s);
    }
    Ok(c.out)
}
