//! A readable dump of a record: an S-expression per signature and per body, printed from an
//! explicit work stack (a body of 100,000 nested parentheses prints in constant stack).
//!
//! Names print as written; a binding as `name@p3` (its pattern), a local as the binding it
//! resolved to, a lang item's path as `#into_iter`, a break's target as `->e9`.

use core::fmt::{self, Write};

use blaze_own::{AVec, Alloc, Vec};

use crate::columns::{View, head};
use crate::form::*;

use blaze_typeck::Name;

/// The names desugarings make, by their hashes.
const MADE: [(&str, Name); 5] = [
    ("$iter", Name::of("iter")),
    ("$val", Name::of("val")),
    ("$residual", Name::of("residual")),
    ("$start", Name::of("start")),
    ("$end", Name::of("end")),
];

#[derive(Clone, Copy)]
enum W {
    S(&'static str),
    Open(&'static str),
    Close,
    Expr(ExprId),
    Pat(PatId),
    Ty(TypeId),
    Path(PathIdx),
    Stmt(StmtId),
    Name(NameIdx),
    Src(BodyRange),
    Num(u32),
    Target(Option<ExprId>),
    Exprs(List),
    /// Names in `ids` (`offset_of!`'s field path).
    Names(List),
    Pats(List),
    Stmts(List),
    Types(List),
    /// Rows of one kind, printed by their tag.
    Rows(List),
    Row(RowIdx),
    Bad(BadRow),
}

struct Printer<'v, 'b, 'o> {
    v: &'v View<'b>,
    out: &'o mut dyn Write,
    space: bool,
}

impl<'b> Printer<'_, 'b, '_> {
    fn atom(&mut self, s: &str) -> fmt::Result {
        if self.space {
            self.out.write_char(' ')?;
        }
        self.space = true;
        self.out.write_str(s)
    }

    fn atom_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        if self.space {
            self.out.write_char(' ')?;
        }
        self.space = true;
        self.out.write_fmt(args)
    }

    fn open(&mut self, s: &str) -> fmt::Result {
        if self.space {
            self.out.write_char(' ')?;
        }
        self.out.write_char('(')?;
        self.out.write_str(s)?;
        self.space = true;
        Ok(())
    }

    fn close(&mut self) -> fmt::Result {
        self.out.write_char(')')?;
        self.space = true;
        Ok(())
    }

    fn name(&self, n: NameIdx) -> &'b str {
        match self.v.name_range(n) {
            // A desugaring's name: printed `$name`, its hygiene shown.
            Ok(r) if r.is_empty() => {
                let hash = self.v.names.get(n.0 as usize).copied();
                MADE.iter().find(|(_, h)| Some(h.0) == hash).map_or("$made", |(s, _)| s)
            }
            Ok(r) => self.src(r),
            Err(_) => "<bad name>",
        }
    }

    /// The text at `r`, through the view; a range it refuses prints as a mark, so a dump
    /// shows a malformed record rather than stopping.
    fn src(&self, r: BodyRange) -> &'b str {
        self.v.text_at(r).unwrap_or("<no text at this range>")
    }
}

/// Dumps the record `v` to `out`, using `alloc` for its work stack.
pub fn dump(v: &View<'_>, alloc: Alloc<'_>, out: &mut dyn Write) -> fmt::Result {
    let mut p = Printer { v, out, space: false };
    let mut work: AVec<'_, W> = Vec::new_in(alloc);
    match v.head[head::MODE] {
        0 => signature(&mut p, &mut work)?,
        3 => match v.type_root() {
            Some(t) => {
                p.open("type")?;
                work.push(W::Close);
                work.push(W::Ty(t));
                run(&mut p, &mut work)?;
            }
            None => p.atom("(no type)")?,
        },
        _ => match v.body() {
            Some((root, params)) => {
                p.open("body")?;
                work.push(W::Close);
                work.push(W::Expr(root));
                for &id in params.iter().rev() {
                    work.push(W::Pat(PatId(id)));
                }
                run(&mut p, &mut work)?;
            }
            None => p.atom("(no body)")?,
        },
    }
    p.out.write_char('\n')?;
    for u in v.unhandled() {
        let kind = syntax_name(u.syntax);
        let text = p.src(u.range);
        writeln!(p.out, "unhandled {kind:?} {:?} {text:?}", u.why)?;
    }
    for d in v.diags() {
        match d.map(|d| (d, v.site(d.range))) {
            Ok((d, Ok(at))) => writeln!(p.out, "diag {:?} {}..{}", d.kind, at.start, at.end)?,
            Ok((d, Err(e))) => writeln!(p.out, "diag {:?} at no place: {e:?}", d.kind)?,
            Err(e) => writeln!(p.out, "bad diag {e:?}")?,
        }
    }
    let mut langs = v.langs().peekable();
    if langs.peek().is_some() {
        p.out.write_str("langs")?;
        for l in langs {
            write!(p.out, " {}", l.name())?;
        }
        p.out.write_char('\n')?;
    }
    Ok(())
}

fn syntax_name(code: u16) -> parser::SyntaxKind {
    if code <= parser::SyntaxKind::__LAST as u16 { parser::SyntaxKind::from(code) } else { parser::SyntaxKind::ERROR }
}

/// Dumps the type `t` of `v` to `out`, as [`dump`] prints it inside any record: the one
/// printing a lone type's record and a signature's types share.
pub fn dump_type(v: &View<'_>, t: TypeId, alloc: Alloc<'_>, out: &mut dyn Write) -> fmt::Result {
    let mut p = Printer { v, out, space: false };
    let mut work: AVec<'_, W> = Vec::new_in(alloc);
    work.push(W::Ty(t));
    run(&mut p, &mut work)
}

fn signature(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>) -> fmt::Result {
    let h = p.v.head;
    if p.v.cfg_out() {
        return p.atom("(cfg-out)");
    }
    let kind = syntax_name(h[head::SYNTAX].min(u32::from(u16::MAX)) as u16);
    p.open("sig")?;
    p.atom_fmt(format_args!("{kind:?}"))?;
    if let Some(n) = p.v.head_opt(head::NAME) {
        let n = p.name(NameIdx(n));
        p.atom(n)?;
    }
    if h[head::FLAGS] != 0 {
        p.atom_fmt(format_args!("flags={:#x}", h[head::FLAGS]))?;
    }
    let vis = Vis::decode(h[head::VIS] as u16, h[head::VIS + 1]);
    if vis != Vis::Private {
        p.atom_fmt(format_args!("{vis:?}"))?;
    }
    if p.v.head_opt(head::ABI).is_some() {
        match p.v.abi() {
            None => p.atom("extern")?,
            Some(r) => {
                let abi = p.src(r);
                p.atom_fmt(format_args!("extern={abi}"))?;
            }
        }
    }
    work.push(W::Close);
    let lists: [(&'static str, usize); 7] = [
        ("items", head::ITEMS),
        ("fields", head::FIELDS),
        ("bounds", head::BOUNDS),
        ("params", head::PARAMS),
        ("where", head::WHERE),
        ("generics", head::GENERICS),
        ("attrs", head::ATTRS),
    ];
    for (word, at) in [("ty", head::TY), ("self", head::SELF_TY), ("trait", head::TRAIT)] {
        if let Some(t) = p.v.head_opt(at) {
            work.push(W::Close);
            work.push(W::Ty(TypeId(t)));
            work.push(W::Open(word));
        }
    }
    for (word, at) in lists {
        let l = p.v.list(at);
        if l.len > 0 {
            work.push(W::Close);
            work.push(W::Rows(l));
            work.push(W::Open(word));
        }
    }
    run(p, work)
}

fn push_ids(work: &mut AVec<'_, W>, ids: &[u32], make: fn(u32) -> W) {
    for &id in ids.iter().rev() {
        work.push(make(id));
    }
}

fn run(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>) -> fmt::Result {
    while let Some(w) = work.pop() {
        match w {
            W::S(s) => p.atom(s)?,
            W::Open(s) => p.open(s)?,
            W::Close => p.close()?,
            W::Num(n) => p.atom_fmt(format_args!("{n}"))?,
            W::Name(n) => {
                let s = p.name(n);
                p.atom(s)?;
            }
            W::Src(r) => {
                let s = p.src(r);
                p.atom(s)?;
            }
            W::Target(t) => match t {
                Some(e) => p.atom_fmt(format_args!("->e{}", e.0))?,
                None => p.atom("->none")?,
            },
            W::Bad(b) => p.atom_fmt(format_args!("<bad {} {}>", b.column, b.tag))?,
            W::Exprs(l) => match p.v.ids(l) {
                Ok(ids) => push_ids(work, ids, |i| W::Expr(ExprId(i))),
                Err(b) => work.push(W::Bad(b)),
            },
            W::Names(l) => match p.v.ids(l) {
                Ok(ids) => push_ids(work, ids, |i| W::Name(NameIdx(i))),
                Err(b) => work.push(W::Bad(b)),
            },
            W::Pats(l) => match p.v.ids(l) {
                Ok(ids) => push_ids(work, ids, |i| W::Pat(PatId(i))),
                Err(b) => work.push(W::Bad(b)),
            },
            W::Stmts(l) => match p.v.ids(l) {
                Ok(ids) => push_ids(work, ids, |i| W::Stmt(StmtId(i))),
                Err(b) => work.push(W::Bad(b)),
            },
            W::Types(l) => match p.v.ids(l) {
                Ok(ids) => push_ids(work, ids, |i| W::Ty(TypeId(i))),
                Err(b) => work.push(W::Bad(b)),
            },
            W::Rows(l) => {
                for i in (l.start..l.start + l.len).rev() {
                    work.push(W::Row(RowIdx(i)));
                }
            }
            W::Row(r) => row(p, work, r)?,
            W::Expr(e) => expr(p, work, e)?,
            W::Pat(x) => pat(p, work, x)?,
            W::Ty(t) => ty(p, work, t)?,
            W::Path(x) => path(p, work, x)?,
            W::Stmt(s) => match p.v.stmt(s) {
                Ok(Stmt::Let { pat, ty, init, else_ }) => {
                    p.open("let")?;
                    work.push(W::Close);
                    if let Some(e) = else_ {
                        work.push(W::Close);
                        work.push(W::Expr(e));
                        work.push(W::Open("else"));
                    }
                    if let Some(e) = init {
                        work.push(W::Expr(e));
                    }
                    if let Some(t) = ty {
                        work.push(W::Ty(t));
                    }
                    work.push(W::Pat(pat));
                }
                Ok(Stmt::Expr { expr, semi }) => {
                    work.push(W::Close);
                    work.push(W::Expr(expr));
                    work.push(W::Open(if semi { "semi" } else { "stmt" }));
                }
                Ok(Stmt::Item { row }) => {
                    work.push(W::Row(row));
                }
                Err(b) => work.push(W::Bad(b)),
            },
        }
    }
    Ok(())
}

/// `(open children.. )`: pushes the close, then the children in reverse.
fn node(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>, head: &'static str, kids: &[W]) -> fmt::Result {
    p.open(head)?;
    work.push(W::Close);
    for k in kids.iter().rev() {
        work.push(*k);
    }
    Ok(())
}

fn expr(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>, e: ExprId) -> fmt::Result {
    let x = match p.v.expr(e) {
        Ok(x) => x,
        Err(b) => return p.atom_fmt(format_args!("<bad {} {}>", b.column, b.tag)),
    };
    let r = match p.v.expr_range(e) {
        Ok(r) => r,
        Err(b) => return p.atom_fmt(format_args!("<bad {} {}>", b.column, b.tag)),
    };
    match x {
        Expr::Missing => p.atom("(missing)"),
        Expr::Lit(_) => node(p, work, "lit", &[W::Src(r)]),
        Expr::Local { pat, .. } => {
            let name = match p.v.pat(pat) {
                Ok(Pat::Bind { name, .. }) => p.name(name),
                _ => "<not a binding>",
            };
            p.atom_fmt(format_args!("{name}@p{}", pat.0))
        }
        Expr::Path(x) => {
            work.push(W::Path(x));
            Ok(())
        }
        Expr::Paren(i) => node(p, work, "paren", &[W::Expr(i)]),
        Expr::Block { stmts, tail, kind, .. } => {
            let head = match kind {
                BlockKind::Plain => "block",
                BlockKind::Unsafe => "unsafe",
                BlockKind::Const => "const",
                BlockKind::Try => "try",
            };
            p.open(head)?;
            p.atom_fmt(format_args!("e{}", e.0))?;
            work.push(W::Close);
            if let Some(t) = tail {
                work.push(W::Close);
                work.push(W::Expr(t));
                work.push(W::Open("tail"));
            }
            work.push(W::Stmts(stmts));
            Ok(())
        }
        Expr::If { cond, then, else_ } => match else_ {
            Some(el) => node(p, work, "if", &[W::Expr(cond), W::Expr(then), W::Expr(el)]),
            None => node(p, work, "if", &[W::Expr(cond), W::Expr(then)]),
        },
        Expr::Let { pat, init } => node(p, work, "let", &[W::Pat(pat), W::Expr(init)]),
        Expr::Match { scrutinee, arms, source } => {
            let head = match source {
                MatchSource::Normal => "match",
                MatchSource::ForLoop => "match/for",
                MatchSource::Try => "match/try",
            };
            node(p, work, head, &[W::Expr(scrutinee), W::Rows(arms)])
        }
        Expr::Loop { body, source } => {
            let head = match source {
                LoopSource::Loop => "loop",
                LoopSource::While => "loop/while",
                LoopSource::ForLoop => "loop/for",
            };
            p.open(head)?;
            p.atom_fmt(format_args!("e{}", e.0))?;
            work.push(W::Close);
            work.push(W::Expr(body));
            Ok(())
        }
        Expr::Break { value, target } => match value {
            Some(v) => node(p, work, "break", &[W::Target(target), W::Expr(v)]),
            None => node(p, work, "break", &[W::Target(target)]),
        },
        Expr::Continue { target } => node(p, work, "continue", &[W::Target(target)]),
        Expr::Return { value } => match value {
            Some(v) => node(p, work, "return", &[W::Expr(v)]),
            None => p.atom("(return)"),
        },
        Expr::Become { value } => node(p, work, "become", &[W::Expr(value)]),
        Expr::Yield { value } => match value {
            Some(v) => node(p, work, "yield", &[W::Expr(v)]),
            None => p.atom("(yield)"),
        },
        Expr::Call { callee, args } => node(p, work, "call", &[W::Expr(callee), W::Exprs(args)]),
        Expr::MethodCall { row, .. } => {
            work.push(W::Row(row));
            Ok(())
        }
        Expr::Field { base, name } => node(p, work, "field", &[W::Expr(base), W::Name(name)]),
        Expr::TupleField { base, index } => node(p, work, "field", &[W::Expr(base), W::Num(index)]),
        Expr::Unary { op, operand } => {
            let head = match op {
                UnOp::Neg => "neg",
                UnOp::Not => "not",
                UnOp::Deref => "deref",
            };
            node(p, work, head, &[W::Expr(operand)])
        }
        Expr::Binary { op, lhs, rhs } => {
            p.open("bin")?;
            p.atom_fmt(format_args!("{op:?}"))?;
            work.push(W::Close);
            work.push(W::Expr(rhs));
            work.push(W::Expr(lhs));
            Ok(())
        }
        Expr::Assign { lhs, rhs } => node(p, work, "assign", &[W::Expr(lhs), W::Expr(rhs)]),
        Expr::AssignOp { op, lhs, rhs } => {
            p.open("assign-op")?;
            p.atom_fmt(format_args!("{op:?}"))?;
            work.push(W::Close);
            work.push(W::Expr(rhs));
            work.push(W::Expr(lhs));
            Ok(())
        }
        Expr::Ref { mutbl, raw, operand } => {
            let head = match (raw, mutbl) {
                (false, Mutbl::Not) => "ref",
                (false, Mutbl::Mut) => "ref-mut",
                (true, Mutbl::Not) => "raw-const",
                (true, Mutbl::Mut) => "raw-mut",
            };
            node(p, work, head, &[W::Expr(operand)])
        }
        Expr::Cast { operand, ty } => node(p, work, "cast", &[W::Expr(operand), W::Ty(ty)]),
        Expr::Tuple { elems } => node(p, work, "tuple", &[W::Exprs(elems)]),
        Expr::Array { elems } => node(p, work, "array", &[W::Exprs(elems)]),
        Expr::Repeat { elem, len } => node(p, work, "repeat", &[W::Expr(elem), W::Expr(len)]),
        Expr::Struct { path, fields, base } => match base {
            StructBase::None => node(p, work, "struct", &[W::Path(path), W::Rows(fields)]),
            StructBase::Expr(b) => node(p, work, "struct", &[W::Path(path), W::Rows(fields), W::S(".."), W::Expr(b)]),
            StructBase::Defaults => node(p, work, "struct", &[W::Path(path), W::Rows(fields), W::S("..")]),
        },
        Expr::Record { fields, base } => match base {
            StructBase::None => node(p, work, "record", &[W::Rows(fields)]),
            StructBase::Expr(b) => node(p, work, "record", &[W::Rows(fields), W::S(".."), W::Expr(b)]),
            StructBase::Defaults => node(p, work, "record", &[W::Rows(fields), W::S("..")]),
        },
        Expr::Index { base, index } => node(p, work, "index", &[W::Expr(base), W::Expr(index)]),
        Expr::Closure { row } | Expr::FormatArgs { row } | Expr::MacroCall { row } => {
            work.push(W::Row(row));
            Ok(())
        }
        Expr::Asm { .. } => node(p, work, "asm", &[W::Src(r)]),
        Expr::OffsetOf { ty, fields } => node(p, work, "offset-of", &[W::Ty(ty), W::Names(fields)]),
        Expr::Underscore => p.atom("_"),
        Expr::Await(e) => node(p, work, "await", &[W::Expr(e)]),
        Expr::InlineFor { pat, iter, body } => {
            node(p, work, "inline-for", &[W::Pat(pat), W::Expr(iter), W::Expr(body)])
        }
    }
}

fn pat(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>, x: PatId) -> fmt::Result {
    let q = match p.v.pat(x) {
        Ok(q) => q,
        Err(b) => return p.atom_fmt(format_args!("<bad {} {}>", b.column, b.tag)),
    };
    match q {
        Pat::Missing => p.atom("(missing)"),
        Pat::Wild => p.atom("_"),
        Pat::Rest => p.atom(".."),
        Pat::Bind { ann, name, sub, shadows, .. } => {
            let mut prefix = "";
            if ann.by_ref == Some(Mutbl::Not) {
                prefix = "ref ";
            } else if ann.by_ref == Some(Mutbl::Mut) {
                prefix = "ref mut ";
            } else if ann.mutable {
                prefix = "mut ";
            }
            let n = p.name(name);
            match shadows {
                Some(s) => p.atom_fmt(format_args!("{prefix}{n}@p{}^p{}", x.0, s.0))?,
                None => p.atom_fmt(format_args!("{prefix}{n}@p{}", x.0))?,
            }
            if let Some(s) = sub {
                node(p, work, "@", &[W::Pat(s)])?;
            }
            Ok(())
        }
        Pat::Tuple { elems, .. } => node(p, work, "tuple", &[W::Pats(elems)]),
        Pat::Ref { inner, .. } => node(p, work, "ref", &[W::Pat(inner)]),
        Pat::Box(inner) => node(p, work, "box", &[W::Pat(inner)]),
        Pat::Lit(_) | Pat::NegLit(_) => {
            let word = if matches!(q, Pat::Lit(_)) { "lit" } else { "neg-lit" };
            match p.v.pat_range(x) {
                Ok(r) => node(p, work, word, &[W::Src(r)]),
                Err(b) => node(p, work, word, &[W::Bad(b)]),
            }
        }
        Pat::Path(path) => {
            work.push(W::Path(path));
            Ok(())
        }
        Pat::TupleStruct { path, elems, .. } => node(p, work, "tuple-struct", &[W::Path(path), W::Pats(elems)]),
        Pat::Struct { path, fields, rest } => match rest {
            true => node(p, work, "struct", &[W::Path(path), W::Rows(fields), W::S("..")]),
            false => node(p, work, "struct", &[W::Path(path), W::Rows(fields)]),
        },
        Pat::Record { fields, rest } => match rest {
            true => node(p, work, "record_pat", &[W::Rows(fields), W::S("..")]),
            false => node(p, work, "record_pat", &[W::Rows(fields)]),
        },
        Pat::Or { alts } => node(p, work, "or", &[W::Pats(alts)]),
        Pat::Slice { elems, .. } => node(p, work, "slice", &[W::Pats(elems)]),
        Pat::Range { lo, hi, inclusive } => {
            p.open(if inclusive { "range=" } else { "range" })?;
            work.push(W::Close);
            match hi {
                Some(h) => work.push(W::Pat(h)),
                None => work.push(W::S("_")),
            }
            match lo {
                Some(l) => work.push(W::Pat(l)),
                None => work.push(W::S("_")),
            }
            Ok(())
        }
        Pat::MacroCall { row } => {
            work.push(W::Row(row));
            Ok(())
        }
    }
}

fn ty(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>, t: TypeId) -> fmt::Result {
    let x = match p.v.ty(t) {
        Ok(x) => x,
        Err(b) => return p.atom_fmt(format_args!("<bad {} {}>", b.column, b.tag)),
    };
    match x {
        TypeRef::Path(x) => {
            work.push(W::Path(x));
            Ok(())
        }
        TypeRef::Ref { mutbl, lifetime, inner } => {
            p.open(if mutbl == Mutbl::Mut { "&mut" } else { "&" })?;
            work.push(W::Close);
            work.push(W::Ty(inner));
            if let Some(l) = lifetime {
                work.push(W::Name(l));
            }
            Ok(())
        }
        TypeRef::Ptr { mutbl, inner } => {
            node(p, work, if mutbl == Mutbl::Mut { "*mut" } else { "*const" }, &[W::Ty(inner)])
        }
        TypeRef::Tuple(l) => node(p, work, "tuple", &[W::Types(l)]),
        TypeRef::Array { elem, len } => match p.v.const_range(len) {
            Ok(r) => node(p, work, "array", &[W::Ty(elem), W::Src(r)]),
            Err(b) => node(p, work, "array", &[W::Ty(elem), W::Bad(b)]),
        },
        TypeRef::Slice(inner) => node(p, work, "slice", &[W::Ty(inner)]),
        TypeRef::FnPtr { params, ret, unsafety, variadic, .. } => {
            p.open("fn")?;
            if unsafety {
                p.atom("unsafe")?;
            }
            if variadic {
                p.atom("...")?;
            }
            work.push(W::Close);
            if let Some(r) = ret {
                work.push(W::Close);
                work.push(W::Ty(r));
                work.push(W::Open("->"));
            }
            work.push(W::Rows(params));
            Ok(())
        }
        TypeRef::Dyn(l) => node(p, work, "dyn", &[W::Rows(l)]),
        TypeRef::Impl(l) => node(p, work, "impl", &[W::Rows(l)]),
        TypeRef::Never => p.atom("!"),
        TypeRef::Infer => p.atom("_"),
        TypeRef::Macro(row) => {
            work.push(W::Row(row));
            Ok(())
        }
        TypeRef::For { binder, inner } => node(p, work, "for", &[W::Rows(binder), W::Ty(inner)]),
        TypeRef::Pat { base, pat: TyPat::NotNull } => node(p, work, "pattern_type", &[W::Ty(base), W::S("!null")]),
        TypeRef::Pat { base, pat: TyPat::Of(pat) } => node(p, work, "pattern_type", &[W::Ty(base), W::Pat(pat)]),
        TypeRef::Record(fields) => node(p, work, "record_type", &[W::Rows(fields)]),
        TypeRef::Union(members) => node(p, work, "union_type", &[W::Types(members)]),
        TypeRef::Error => p.atom("(error)"),
    }
}

fn path(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>, x: PathIdx) -> fmt::Result {
    let path = match p.v.path(x) {
        Ok(path) => path,
        Err(b) => return p.atom_fmt(format_args!("<bad {} {}>", b.column, b.tag)),
    };
    if let Some(lang) = path.lang {
        return p.atom_fmt(format_args!("#{}", lang.name()));
    }
    if path.segments.len == 0 {
        return p.atom("<hygienic>");
    }
    p.open(if path.absolute { "path ::" } else { "path" })?;
    work.push(W::Close);
    work.push(W::Rows(path.segments));
    Ok(())
}

fn row(p: &mut Printer<'_, '_, '_>, work: &mut AVec<'_, W>, r: RowIdx) -> fmt::Result {
    let n = match p.v.row(r) {
        Ok(n) => n,
        Err(b) => return p.atom_fmt(format_args!("<bad {} {}>", b.column, b.tag)),
    };
    match n.tag {
        ROW_ARM => match Arm::decode(n) {
            Ok(a) => match a.guard {
                Some(g) => node(p, work, "arm", &[W::Pat(a.pat), W::Open("if"), W::Expr(g), W::Close, W::Expr(a.body)]),
                None => node(p, work, "arm", &[W::Pat(a.pat), W::Expr(a.body)]),
            },
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_FIELD_INIT => match FieldInit::decode(n) {
            Ok(f) => node(p, work, "init", &[W::Name(f.name), W::Expr(f.expr)]),
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_FIELD_PAT => match FieldPat::decode(n) {
            Ok(f) => node(p, work, "field", &[W::Name(f.name), W::Pat(f.pat)]),
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_METHOD => match MethodCall::decode(n) {
            Ok(m) => {
                if m.has_args {
                    node(p, work, "method", &[W::Expr(m.receiver), W::Name(m.name), W::Rows(m.generic_args), W::Exprs(m.args)])
                } else {
                    node(p, work, "method", &[W::Expr(m.receiver), W::Name(m.name), W::Exprs(m.args)])
                }
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_CLOSURE => match Closure::decode(n) {
            Ok(c) => {
                let head = match (c.kind, c.source) {
                    (CoroutineKind::None, _) => "closure",
                    (CoroutineKind::Async, CoroutineSource::Block) => "async-block",
                    (CoroutineKind::Async, CoroutineSource::Closure) => "async-closure",
                    (CoroutineKind::Gen, _) => "gen",
                    (CoroutineKind::AsyncGen, _) => "async-gen",
                };
                p.open(head)?;
                if c.capture_by_value {
                    p.atom("move")?;
                }
                work.push(W::Close);
                work.push(W::Expr(c.body));
                if let Some(t) = c.ret {
                    work.push(W::Close);
                    work.push(W::Ty(t));
                    work.push(W::Open("->"));
                }
                work.push(W::Rows(c.params));
                Ok(())
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_PARAM => match Param::decode(n) {
            Ok(x) => {
                p.open("param")?;
                match x.self_kind {
                    SelfKind::No => {}
                    SelfKind::Value => p.atom("self")?,
                    SelfKind::Ref(Mutbl::Not) => p.atom("&self")?,
                    SelfKind::Ref(Mutbl::Mut) => p.atom("&mut-self")?,
                    SelfKind::Typed => p.atom("self:")?,
                }
                if x.variadic {
                    p.atom("...")?;
                }
                work.push(W::Close);
                if let Some(t) = x.ty {
                    work.push(W::Ty(t));
                }
                if let Some(q) = x.pat {
                    work.push(W::Pat(q));
                }
                Ok(())
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_FORMAT => match p.v.format_args(n) {
            Ok(f) => {
                p.open(if f.newline { "format-args-nl" } else { "format-args" })?;
                let template = p.src(f.template);
                p.atom_fmt(format_args!("{template:?}"))?;
                work.push(W::Close);
                work.push(W::Rows(f.args));
                work.push(W::Rows(f.pieces));
                Ok(())
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_FMT_PIECE => match p.v.fmt_piece(n) {
            Ok(FmtPiece::Text(t)) => {
                let t = p.src(t);
                p.atom_fmt(format_args!("{t:?}"))
            }
            Ok(FmtPiece::Arg { arg, spec }) => {
                let spec = p.src(spec);
                p.atom_fmt(format_args!("{{{arg}:{spec}}}"))
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_FMT_ARG => match FmtArg::decode(n) {
            Ok(a) => {
                p.open(if a.captured { "captured" } else { "arg" })?;
                work.push(W::Close);
                work.push(W::Expr(a.expr));
                if let Some(name) = a.name {
                    work.push(W::Name(name));
                }
                Ok(())
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_MACRO => match p.v.macro_call(n) {
            Ok(m) => node(p, work, "macro", &[W::Path(m.path), W::Src(m.input)]),
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_LOCAL_ITEM | ROW_ASSOC => match p.v.local_item(n) {
            Ok(i) => {
                p.open("item")?;
                p.atom_fmt(format_args!("{:?}", syntax_name(i.syntax)))?;
                if let Some(name) = i.name {
                    work.push(W::Close);
                    work.push(W::Name(name));
                } else {
                    work.push(W::Close);
                }
                Ok(())
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_SEGMENT => match Segment::decode(n) {
            Ok(s) => {
                let simple = !s.has_args && s.ty.is_none();
                match (s.kind, s.name) {
                    (SegKind::Anchor, _) => {
                        p.open("<>")?;
                        work.push(W::Close);
                        if let Some(t) = s.trait_ {
                            work.push(W::Ty(t));
                        }
                        if let Some(t) = s.ty {
                            work.push(W::Ty(t));
                        }
                        Ok(())
                    }
                    (_, Some(name)) if simple => {
                        let text = p.name(name);
                        p.atom(text)
                    }
                    (SegKind::SelfMod, None) if simple => p.atom("self"),
                    (SegKind::Super, None) if simple => p.atom("super"),
                    (SegKind::Crate, None) if simple => p.atom("crate"),
                    (SegKind::SelfTy, None) if simple => p.atom("Self"),
                    (SegKind::DollarCrate, None) if simple => p.atom("$crate"),
                    (_, name) => {
                        p.open("seg")?;
                        if let Some(name) = name {
                            let text = p.name(name);
                            p.atom(text)?;
                        }
                        if s.parenthesized {
                            p.atom("()")?;
                        }
                        work.push(W::Close);
                        if let Some(t) = s.ty {
                            work.push(W::Ty(t));
                        }
                        work.push(W::Rows(s.args));
                        Ok(())
                    }
                }
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_ARG => match GenericArg::decode(n) {
            Ok(GenericArg::Type(t)) => {
                work.push(W::Ty(t));
                Ok(())
            }
            Ok(GenericArg::Lifetime(l)) => {
                let text = p.name(l);
                p.atom(text)
            }
            Ok(GenericArg::Const(c)) => match p.v.const_range(c) {
                Ok(r) => node(p, work, "const", &[W::Src(r)]),
                Err(b) => node(p, work, "const", &[W::Bad(b)]),
            },
            Ok(GenericArg::AssocType { name, ty, .. }) => node(p, work, "=", &[W::Name(name), W::Ty(ty)]),
            Ok(GenericArg::AssocConst { name, value, .. }) => match p.v.const_range(value) {
                Ok(r) => node(p, work, "=", &[W::Name(name), W::Src(r)]),
                Err(b) => node(p, work, "=", &[W::Name(name), W::Bad(b)]),
            },
            Ok(GenericArg::AssocBound { name, bounds, .. }) => node(p, work, ":", &[W::Name(name), W::Rows(bounds)]),
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_BOUND => match Bound::decode(n) {
            Ok(Bound::Trait { ty, binder, mods }) => {
                let head = match (mods.maybe, mods.negative, mods.konst, mods.maybe_const) {
                    (true, ..) => "?",
                    (_, true, ..) => "!",
                    (_, _, true, _) => "const",
                    (_, _, _, true) => "[const]",
                    _ => "bound",
                };
                if binder.len > 0 {
                    node(p, work, head, &[W::Open("for"), W::Rows(binder), W::Close, W::Ty(ty)])
                } else {
                    node(p, work, head, &[W::Ty(ty)])
                }
            }
            Ok(Bound::Lifetime(l)) => {
                let text = p.name(l);
                p.atom(text)
            }
            Ok(Bound::Use(l)) => match p.v.ids(l) {
                Ok(ids) => {
                    p.open("use")?;
                    work.push(W::Close);
                    push_ids(work, ids, |i| W::Name(NameIdx(i)));
                    Ok(())
                }
                Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
            },
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_GENERIC_PARAM => match GenericParam::decode(n) {
            Ok(GenericParam::Lifetime { name, bounds }) => node(p, work, "lifetime", &[W::Name(name), W::Rows(bounds)]),
            Ok(GenericParam::Type { name, bounds, default }) => match default {
                Some(d) => node(p, work, "type", &[W::Name(name), W::Rows(bounds), W::S("="), W::Ty(d)]),
                None => node(p, work, "type", &[W::Name(name), W::Rows(bounds)]),
            },
            Ok(GenericParam::Const { name, ty, default }) => match default.map(|d| p.v.const_range(d)) {
                Some(Ok(r)) => node(p, work, "const", &[W::Name(name), W::Ty(ty), W::S("="), W::Src(r)]),
                Some(Err(b)) => node(p, work, "const", &[W::Name(name), W::Ty(ty), W::Bad(b)]),
                None => node(p, work, "const", &[W::Name(name), W::Ty(ty)]),
            },
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_WHERE => match WherePred::decode(n) {
            Ok(WherePred::Type { binder, ty, bounds }) => {
                if binder.len > 0 {
                    node(p, work, "pred", &[W::Open("for"), W::Rows(binder), W::Close, W::Ty(ty), W::Rows(bounds)])
                } else {
                    node(p, work, "pred", &[W::Ty(ty), W::Rows(bounds)])
                }
            }
            Ok(WherePred::Lifetime { name, bounds }) => node(p, work, "pred", &[W::Name(name), W::Rows(bounds)]),
            Ok(WherePred::Const { cond }) => match p.v.const_range(cond) {
                Ok(r) => node(p, work, "const-pred", &[W::Src(r)]),
                Err(b) => node(p, work, "const-pred", &[W::Bad(b)]),
            },
            Ok(WherePred::ForFields { param, container, subject, bounds }) => node(
                p,
                work,
                "for-fields-pred",
                &[W::Name(param), W::Ty(container), W::Ty(subject), W::Rows(bounds)],
            ),
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_ATTR => match p.v.attr(n) {
            Ok(a) => {
                let path = p.src(a.path);
                let args = p.src(a.args);
                if args.is_empty() {
                    p.atom_fmt(format_args!("#[{path}]"))
                } else {
                    p.atom_fmt(format_args!("#[{path} {args}]"))
                }
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_FIELD => match Field::decode(n) {
            Ok(f) => {
                p.open("field")?;
                if f.vis != Vis::Private {
                    p.atom_fmt(format_args!("{:?}", f.vis))?;
                }
                work.push(W::Close);
                if let Some(d) = f.default {
                    match p.v.const_range(d) {
                        Ok(r) => work.push(W::Src(r)),
                        Err(b) => work.push(W::Bad(b)),
                    }
                    work.push(W::S("="));
                }
                work.push(W::Ty(f.ty));
                if let Some(name) = f.name {
                    work.push(W::Name(name));
                }
                if f.attrs.len > 0 {
                    work.push(W::Rows(f.attrs));
                }
                Ok(())
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        ROW_VARIANT => match Variant::decode(n) {
            Ok(v) => {
                p.open("variant")?;
                work.push(W::Close);
                if let Some(d) = v.discriminant {
                    match p.v.const_range(d) {
                        Ok(r) => work.push(W::Src(r)),
                        Err(b) => work.push(W::Bad(b)),
                    }
                    work.push(W::S("="));
                }
                work.push(W::Rows(v.fields));
                work.push(W::S(match v.shape {
                    Shape::Record => "{}",
                    Shape::Tuple => "()",
                    Shape::Unit => "unit",
                }));
                work.push(W::Name(v.name));
                Ok(())
            }
            Err(b) => p.atom_fmt(format_args!("<bad {}>", b.column)),
        },
        tag => p.atom_fmt(format_args!("<row tag {tag}>")),
    }
}
