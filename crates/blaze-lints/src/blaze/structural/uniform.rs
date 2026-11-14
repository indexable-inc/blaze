//! The one analysis the structural lints share: whether a body is uniform over a type's fields,
//! the same template written once per field.
//!
//! **The template.** A body is split into terms under one combinator:
//!
//! - statements, a term each (`self.a.hash(state); self.b.hash(state);`);
//! - one operator between operands, flattened through parentheses: `&&`, `||`, `+`, `*`, `^`,
//!   `|`, `&` (`self.a == other.a && self.b == other.b`);
//! - a method chain whose links that read a field share one name, the combinator: each such
//!   link's arguments are a term, a lone argument unwrapped, and a closure with no parameters
//!   unwrapped to its body (`.then(x)`, `.then_with(|| x)`, `.field("a", &self.a)`); what comes
//!   before the first combinator link is the head, a term itself when it reads a field
//!   (`self.a.cmp(&other.a)` in an `Ord` chain), and the links after it that read nothing are
//!   the frame (`.finish()`);
//! - a struct expression of the type (or of the owner type of a conversion), a term per field
//!   it gives, owned by the field's label; a tuple struct's constructor, a term per argument,
//!   owned by its position;
//! - a single-argument `Ok(..)` or `Some(..)` around any of these is looked through.
//!
//! A `let` that destructures a base (`let Self { a, b, cache: _ } = self;`) is not a term: it
//! binds each local to its field, and a `field: _` with `// left out: <why>` on its line marks
//! the field left out on purpose.
//!
//! A term's template is its tokens, trivia dropped, with each hole written `#`: the field name
//! of a field expression whose base is a base (`self`, a parameter of the type, through `&`,
//! `*` and parentheses), a local a destructuring bound to a field, and a string literal equal to
//! a field's name (written `"#"`). Type-directed spellings of one operation are one template:
//! `#.clone()` and `Clone::clone(&#)` are `#` (a `Copy` field is copied, another cloned, both
//! the derive's `Clone::clone(&self.a)`), and `Default::default()`, `T::default()`, `0`,
//! `0.0`, `false`, `None`, `""` and the empty constructors of std's collections are one
//! `DEFAULT`.
//!
//! **The verdict.** A term is clean when it reads one field and, if it has a label, the label
//! is that field (or names no field of the source, a rename); a labelled term that reads
//! nothing fills its own field. A labelled term reading another field that has its own label
//! is crossed; a term reading two fields is odd. The template of the most clean terms is the
//! body's (a tie is no template). Then:
//!
//! - [`Shape::Uniform`]: every field is covered once by the template or left out, at least
//!   `least` fields covered, nothing odd or crossed, and the base never handed on whole (a call
//!   taking `self` may read the rest);
//! - [`Shape::AlmostUniform`]: two or more fields covered, more than are odd or crossed, and
//!   some field missing, odd or crossed: the body is the template with a mistake in it;
//! - [`Shape::NotUniform`]: anything else.
//!
//! The order of the covered fields is kept: a derive writes `Hash`, `Debug` and `Ord` in
//! declaration order, and one written in another order is not the derive's.

use std::alloc::Global;

use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, ArithOp, BinaryOp, HasArgList as _, HasName as _, LogicOp},
};

/// What a field's place in a template is written as.
pub(super) const HOLE: &str = "#";

/// The template of a term that fills its field with the field type's default.
pub(super) const DEFAULT: &str = "DEFAULT";

/// What the analysis reads a body against.
pub(super) struct Scope<'a> {
    /// The fields read, in declaration order.
    pub(super) fields: &'a [&'a str],
    /// The labels a struct expression fills: the fields again for an impl of the type, the
    /// target's for a conversion.
    pub(super) owners: &'a [&'a str],
    /// The names whose fields are the holes: `self` and each parameter of the type.
    pub(super) bases: &'a [&'a str],
    /// The paths a struct expression of the owner type is written with: `Self`, its name.
    pub(super) makers: &'a [&'a str],
}

/// One term: the code of one field.
#[derive(Debug)]
pub(super) struct Term {
    /// The label it fills, an index into [`Scope::owners`].
    pub(super) owner: Option<usize>,
    /// The fields it reads, indices into [`Scope::fields`], each once, in the order read.
    pub(super) reads: Vec<usize, Global>,
    /// Its tokens, trivia dropped, each hole `#`, space-separated.
    pub(super) template: String,
    /// Where it is written.
    pub(super) range: TextRange,
}

/// How a body combines its terms.
#[derive(Debug)]
pub(super) enum Form<'t> {
    /// Statements, one term each.
    Statements,
    /// One operator between the terms.
    Operator(BinaryOp),
    /// A method chain whose links that read fields are all named `link`.
    Chain {
        /// What the first `link` call is made on.
        head: ast::Expr<'t>,
        link: &'t str,
        /// The names of the links after the first `link` that read nothing (`finish`).
        after: Vec<&'t str, Global>,
    },
    /// A struct expression, a term per field it gives.
    Record,
    /// A tuple struct's constructor, a term per argument.
    Tuple,
    /// One expression.
    One,
}

/// A body split into terms.
#[derive(Debug)]
pub(super) struct Body<'t> {
    pub(super) form: Form<'t>,
    pub(super) terms: Vec<Term, Global>,
    /// Fields a destructuring leaves out with `field: _` and `// left out: <why>`.
    pub(super) left_out: Vec<usize, Global>,
    /// Whether a base is handed on whole (`self.other_method()`, `f(self)`), or a struct
    /// expression ends in `..rest`: what the terms do not show may read the rest.
    pub(super) whole: bool,
}

/// The verdict of [`classify`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shape {
    Uniform,
    AlmostUniform,
    NotUniform,
}

/// What [`classify`] found.
#[derive(Debug)]
pub(super) struct Analysis {
    pub(super) shape: Shape,
    /// The body's template, empty for none.
    pub(super) template: String,
    /// The fields the template covers, in the order of the terms.
    pub(super) covered: Vec<usize, Global>,
    /// The fields no term reads, none left out, in declaration order.
    pub(super) missing: Vec<usize, Global>,
    /// The fields read by a term of another template, read twice, or read with another.
    pub(super) odd: Vec<usize, Global>,
    /// `(label, read)`: a term labelled with one field that reads another.
    pub(super) crossed: Vec<(usize, usize), Global>,
    /// The terms that read nothing and fill no field.
    pub(super) frame: usize,
}

impl Analysis {
    /// Whether the covered fields are every field, in declaration order.
    pub(super) fn in_order(&self, fields: usize) -> bool {
        self.covered.len() == fields && self.covered.iter().enumerate().all(|(at, &it)| at == it)
    }
}

/// Where a term's reads put it.
#[derive(Clone, Copy)]
enum Slot {
    Clean(usize),
    Crossed(usize, usize),
    Odd,
    Frame,
}

/// The verdict on `terms` over `fields` (labels `owners`), `left_out` excused, at least `least`
/// fields covered for [`Shape::Uniform`]. The pure core of the analysis: no syntax.
pub(super) fn classify(
    fields: &[&str],
    owners: &[&str],
    left_out: &[usize],
    terms: &[Term],
    least: usize,
    whole: bool,
) -> Analysis {
    let mut slots: Vec<Slot, Global> = Vec::new_in(Global);
    for term in terms {
        let own = term
            .owner
            .and_then(|it| owners.get(it))
            .map(|name| fields.iter().position(|field| field == name));
        let slot = match (own, term.reads.as_slice()) {
            (None, []) | (Some(None), []) => Slot::Frame,
            (None, [one]) | (Some(None), [one]) => Slot::Clean(*one),
            (Some(Some(at)), []) => Slot::Clean(at),
            (Some(Some(at)), [one]) if *one == at => Slot::Clean(at),
            (Some(Some(at)), [one]) => Slot::Crossed(at, *one),
            _ => Slot::Odd,
        };
        slots.push(slot);
    }
    let mut counts: Vec<(&str, usize), Global> = Vec::new_in(Global);
    for (term, slot) in terms.iter().zip(&slots) {
        if let Slot::Clean(_) = slot {
            match counts.iter_mut().find(|(it, _)| *it == term.template) {
                Some((_, count)) => *count += 1,
                None => counts.push((&term.template, 1)),
            }
        }
    }
    let best = counts.iter().map(|(_, count)| *count).max().unwrap_or(0);
    let tied = counts.iter().filter(|(_, count)| *count == best).count() > 1;
    let template = match counts.iter().find(|(_, count)| *count == best) {
        Some((it, _)) if !tied => *it,
        _ => "",
    };
    let mut seen: Vec<bool, Global> = Vec::new_in(Global);
    seen.resize(fields.len(), false);
    let mut covered: Vec<usize, Global> = Vec::new_in(Global);
    let mut odd: Vec<usize, Global> = Vec::new_in(Global);
    let mut crossed: Vec<(usize, usize), Global> = Vec::new_in(Global);
    let mut frame = 0;
    for (term, slot) in terms.iter().zip(&slots) {
        match *slot {
            Slot::Clean(at) if !template.is_empty() && term.template == template => {
                if seen[at] {
                    odd.push(at);
                } else {
                    seen[at] = true;
                    covered.push(at);
                }
            }
            Slot::Clean(at) => odd.push(at),
            Slot::Crossed(label, read) => crossed.push((label, read)),
            Slot::Odd => {
                odd.extend(term.reads.iter().copied());
                let own = term.owner.and_then(|it| owners.get(it));
                if let Some(at) = own.and_then(|name| fields.iter().position(|it| it == name)) {
                    odd.push(at);
                }
            }
            Slot::Frame => frame += 1,
        }
    }
    let mut kept: Vec<usize, Global> = Vec::new_in(Global);
    for it in odd {
        if !kept.contains(&it) {
            kept.push(it);
        }
    }
    let odd = kept;
    let touched =
        |at: usize| crossed.iter().any(|&(label, read)| label == at || read == at) || odd.contains(&at);
    let mut missing: Vec<usize, Global> = Vec::new_in(Global);
    for at in 0..fields.len() {
        if !seen[at] && !left_out.contains(&at) && !touched(at) {
            missing.push(at);
        }
    }
    let clean = missing.is_empty() && odd.is_empty() && crossed.is_empty();
    let shape = if whole || template.is_empty() {
        Shape::NotUniform
    } else if clean && covered.len() >= least.max(1) {
        Shape::Uniform
    } else if !clean && covered.len() >= 2 && covered.len() > odd.len() + crossed.len() {
        Shape::AlmostUniform
    } else {
        Shape::NotUniform
    };
    Analysis { shape, template: template.to_owned(), covered, missing, odd, crossed, frame }
}

/// The analysis of `body` in `scope`, with `least` fields covered for [`Shape::Uniform`].
pub(super) fn analyse(scope: &Scope<'_>, body: &Body<'_>, least: usize) -> Analysis {
    classify(scope.fields, scope.owners, &body.left_out, &body.terms, least, body.whole)
}

/// `block` split into terms in `scope`; `excused` says whether a `field: _` at a range has a
/// `// left out: <why>`. `None` for a body with an item in it.
pub(super) fn split<'t>(
    scope: &Scope<'_>,
    block: ast::BlockExpr<'t>,
    excused: &dyn Fn(TextRange) -> bool,
) -> Option<Body<'t>> {
    let list = block.stmt_list()?;
    let mut locals: Vec<(&'t str, usize), Global> = Vec::new_in(Global);
    let mut left_out: Vec<usize, Global> = Vec::new_in(Global);
    let mut whole = false;
    let mut parts: Vec<ast::Node<'t>, Global> = Vec::new_in(Global);
    for stmt in list.statements() {
        match stmt {
            ast::Stmt::Item(_) => return None,
            ast::Stmt::LetStmt(it) => {
                if !destructure(scope, it, &mut locals, &mut left_out, &mut whole, excused) {
                    parts.push(it.syntax());
                }
            }
            ast::Stmt::ExprStmt(it) => {
                if let Some(expr) = it.expr() {
                    parts.push(expr.syntax());
                }
            }
        }
    }
    if let Some(tail) = list.tail_expr() {
        parts.push(tail.syntax());
    }
    let mut cut = Cut { scope, locals: &locals, whole: &mut whole };
    let (form, terms) = match parts.as_slice() {
        [] => (Form::Statements, Vec::new_in(Global)),
        [one] => match ast::Expr::cast(*one) {
            Some(expr) => cut.expr(expr),
            None => (Form::One, one_term(cut.term(None, *one))),
        },
        _ => {
            let mut terms: Vec<Term, Global> = Vec::new_in(Global);
            for part in &parts {
                terms.push(cut.term(None, *part));
            }
            (Form::Statements, terms)
        }
    };
    Some(Body { form, terms, left_out, whole })
}

fn one_term(term: Term) -> Vec<Term, Global> {
    let mut terms: Vec<Term, Global> = Vec::new_in(Global);
    terms.push(term);
    terms
}

/// Reads a `let` that destructures a base into `locals` and `left_out`; `false` for any other
/// `let`, which is a term.
fn destructure<'t>(
    scope: &Scope<'_>,
    stmt: ast::LetStmt<'t>,
    locals: &mut Vec<(&'t str, usize), Global>,
    left_out: &mut Vec<usize, Global>,
    whole: &mut bool,
    excused: &dyn Fn(TextRange) -> bool,
) -> bool {
    let Some(init) = stmt.initializer() else { return false };
    if !is_base(scope, init) {
        return false;
    }
    let mut bind = |field: &str, pat: Option<ast::Pat<'t>>, range: TextRange| {
        let Some(at) = scope.fields.iter().position(|it| *it == field) else { return };
        match pat {
            Some(ast::Pat::IdentPat(it)) if it.pat().is_none() => {
                if let Some(name) = it.name() {
                    locals.push((name.text(), at));
                }
            }
            Some(ast::Pat::WildcardPat(_)) if excused(range) => left_out.push(at),
            Some(ast::Pat::WildcardPat(_)) => {}
            _ => *whole = true,
        }
    };
    match stmt.pat() {
        Some(ast::Pat::RecordPat(pat)) => {
            let Some(list) = pat.record_pat_field_list() else { return false };
            for field in list.fields() {
                let name = match field.name_ref() {
                    Some(it) => it.text(),
                    None => match field.pat() {
                        Some(ast::Pat::IdentPat(it)) => match it.name() {
                            Some(name) => name.text(),
                            None => continue,
                        },
                        _ => continue,
                    },
                };
                bind(name, field.pat(), field.syntax().text_range());
            }
            true
        }
        Some(ast::Pat::TupleStructPat(pat)) => {
            for (at, field) in pat.fields().enumerate() {
                let name = at.to_string();
                bind(&name, Some(field), field.syntax().text_range());
            }
            true
        }
        _ => false,
    }
}

/// Whether `expr` is a base, through `&`, `*` and parentheses.
fn is_base(scope: &Scope<'_>, expr: ast::Expr<'_>) -> bool {
    match peel(expr) {
        ast::Expr::PathExpr(it) => it.path().and_then(single).is_some_and(|it| scope.bases.contains(&it)),
        _ => false,
    }
}

/// `expr` without the `&`, `*` and parentheses around it.
fn peel(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    let mut at = expr;
    loop {
        let inner = match at {
            ast::Expr::ParenExpr(it) => it.expr(),
            ast::Expr::RefExpr(it) => it.expr(),
            ast::Expr::PrefixExpr(it) if it.op_kind() == Some(ast::UnaryOp::Deref) => it.expr(),
            _ => None,
        };
        match inner {
            Some(it) => at = it,
            None => return at,
        }
    }
}

/// The one name `path` is (`self`, `other`, `Self`, `Ok`), with no qualifier or arguments.
pub(super) fn single(path: ast::Path<'_>) -> Option<&str> {
    if path.qualifier().is_some() {
        return None;
    }
    let segment = path.segment()?;
    let text = segment.syntax().text();
    text.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'_').then_some(text)
}

/// The last segment's name of `path` (`Point` of `crate::geo::Point`).
pub(super) fn last(path: ast::Path<'_>) -> Option<&str> {
    let segment = path.segment()?;
    match segment.name_ref() {
        Some(it) => Some(it.text()),
        None => Some(segment.syntax().text()),
    }
}

/// The splitting of one expression, and the terms it makes.
struct Cut<'s, 'l, 't> {
    scope: &'s Scope<'s>,
    locals: &'l [(&'t str, usize)],
    whole: &'l mut bool,
}

impl<'t> Cut<'_, '_, 't> {
    fn expr(&mut self, expr: ast::Expr<'t>) -> (Form<'t>, Vec<Term, Global>) {
        let expr = expr.peel_parens();
        let mut terms: Vec<Term, Global> = Vec::new_in(Global);
        match expr {
            ast::Expr::CallExpr(call) => {
                let callee = match call.expr() {
                    Some(ast::Expr::PathExpr(it)) => it.path(),
                    _ => None,
                };
                let list = call.arg_list();
                let name = callee.and_then(last);
                if let (Some("Ok" | "Some"), Some(list)) = (name, list) {
                    let mut args = list.args();
                    if let (Some(arg), None) = (args.next(), args.next()) {
                        return self.expr(arg);
                    }
                }
                if let (Some(name), Some(list)) = (name, list)
                    && self.scope.makers.contains(&name)
                {
                    for (at, arg) in list.args().enumerate() {
                        let label = at.to_string();
                        let owner = self.scope.owners.iter().position(|it| *it == label);
                        terms.push(self.term(owner, arg.syntax()));
                    }
                    return (Form::Tuple, terms);
                }
                terms.push(self.term(None, expr.syntax()));
                (Form::One, terms)
            }
            ast::Expr::BinExpr(bin) => match bin.op_kind() {
                Some(op) if splits(op) => {
                    let mut operands: Vec<ast::Expr<'t>, Global> = Vec::new_in(Global);
                    flatten(expr, op, &mut operands);
                    for it in operands {
                        terms.push(self.term(None, it.syntax()));
                    }
                    (Form::Operator(op), terms)
                }
                _ => {
                    terms.push(self.term(None, expr.syntax()));
                    (Form::One, terms)
                }
            },
            ast::Expr::RecordExpr(record) => {
                let name = record.path().and_then(last);
                if !name.is_some_and(|it| self.scope.makers.contains(&it)) {
                    terms.push(self.term(None, expr.syntax()));
                    return (Form::One, terms);
                }
                let Some(list) = record.record_expr_field_list() else {
                    return (Form::Record, terms);
                };
                if list.spread().is_some() || list.dotdot_token().is_some() {
                    *self.whole = true;
                }
                for field in list.fields() {
                    let Some(value) = field.expr() else { continue };
                    let label = match field.name_ref() {
                        Some(it) => Some(it.text()),
                        None => match value {
                            ast::Expr::PathExpr(it) => it.path().and_then(single),
                            _ => None,
                        },
                    };
                    let owner = label
                        .and_then(|label| self.scope.owners.iter().position(|it| *it == label));
                    terms.push(self.term(owner, value.syntax()));
                }
                (Form::Record, terms)
            }
            ast::Expr::MethodCallExpr(_) => self.chain(expr),
            _ => {
                terms.push(self.term(None, expr.syntax()));
                (Form::One, terms)
            }
        }
    }

    /// A method chain split at its combinator, or one term when it has none.
    fn chain(&mut self, expr: ast::Expr<'t>) -> (Form<'t>, Vec<Term, Global>) {
        let mut links: Vec<ast::MethodCallExpr<'t>, Global> = Vec::new_in(Global);
        let mut at = expr;
        while let ast::Expr::MethodCallExpr(call) = at {
            links.push(call);
            match call.receiver() {
                Some(it) => at = it,
                None => break,
            }
        }
        links.reverse();
        let root = at;
        // Whether each link's arguments read a field, measured without marking `whole`.
        let mut reads: Vec<bool, Global> = Vec::new_in(Global);
        for link in &links {
            let read = link.arg_list().is_some_and(|it| !self.quiet(it.syntax()).reads.is_empty());
            reads.push(read);
        }
        let mut names: Vec<(&'t str, usize), Global> = Vec::new_in(Global);
        for (link, read) in links.iter().zip(&reads) {
            let Some(name) = link.name_ref().map(|it| it.text()) else { continue };
            if !read {
                continue;
            }
            match names.iter_mut().find(|(it, _)| *it == name) {
                Some((_, count)) => *count += 1,
                None => names.push((name, 1)),
            }
        }
        let best = names.iter().map(|(_, count)| *count).max().unwrap_or(0);
        let combinator = names.iter().find(|(_, count)| *count == best).map(|(it, _)| *it);
        let first = combinator.and_then(|name| {
            links.iter().position(|it| it.name_ref().is_some_and(|it| it.text() == name))
        });
        let mut terms: Vec<Term, Global> = Vec::new_in(Global);
        let (Some(link), Some(first)) = (combinator, first) else {
            terms.push(self.term(None, expr.syntax()));
            return (Form::One, terms);
        };
        let head = match first {
            0 => root,
            at => ast::Expr::MethodCallExpr(links[at - 1]),
        };
        let head_reads = !self.quiet(head.syntax()).reads.is_empty();
        if best < 2 && head_reads {
            terms.push(self.term(None, expr.syntax()));
            return (Form::One, terms);
        }
        if head_reads {
            terms.push(self.term(None, head.syntax()));
        } else {
            // Only to see a base handed on whole in the head.
            let _ = self.term(None, head.syntax());
        }
        let mut after: Vec<&'t str, Global> = Vec::new_in(Global);
        for (call, read) in links.iter().zip(&reads).skip(first) {
            let name = call.name_ref().map(|it| it.text());
            let Some(list) = call.arg_list() else { continue };
            if name == Some(link) {
                let mut args = list.args();
                let node = match (args.next(), args.next()) {
                    (Some(ast::Expr::ClosureExpr(it)), None)
                        if it.param_list().is_some_and(|it| it.params().next().is_none()) =>
                    {
                        it.body().map_or(list.syntax(), |it| it.syntax())
                    }
                    (Some(arg), None) => arg.syntax(),
                    _ => list.syntax(),
                };
                terms.push(self.term(None, node));
            } else if *read {
                terms.push(self.term(None, list.syntax()));
            } else {
                after.push(name.unwrap_or_default());
            }
        }
        (Form::Chain { head, link, after }, terms)
    }

    /// [`Cut::term`] that leaves `whole` alone: a measure of what a node reads.
    fn quiet(&self, node: ast::Node<'t>) -> Term {
        let mut whole = false;
        term_of(self.scope, self.locals, None, node, &mut whole)
    }

    fn term(&mut self, owner: Option<usize>, node: ast::Node<'t>) -> Term {
        term_of(self.scope, self.locals, owner, node, self.whole)
    }
}

/// The operators a body's terms are split at.
fn splits(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::LogicOp(LogicOp::And | LogicOp::Or)
            | BinaryOp::ArithOp(
                ArithOp::Add | ArithOp::Mul | ArithOp::BitXor | ArithOp::BitOr | ArithOp::BitAnd
            )
    )
}

/// The operands of a chain of `op`, through parentheses, left to right.
fn flatten<'t>(expr: ast::Expr<'t>, op: BinaryOp, out: &mut Vec<ast::Expr<'t>, Global>) {
    match expr.peel_parens() {
        ast::Expr::BinExpr(bin) if bin.op_kind() == Some(op) => {
            if let Some(lhs) = bin.lhs() {
                flatten(lhs, op, out);
            }
            if let Some(rhs) = bin.rhs() {
                flatten(rhs, op, out);
            }
        }
        other => out.push(other),
    }
}

/// The term `node` is: its reads and its template.
fn term_of<'t>(
    scope: &Scope<'_>,
    locals: &[(&'t str, usize)],
    owner: Option<usize>,
    node: ast::Node<'t>,
    whole: &mut bool,
) -> Term {
    let mut holes: Vec<(TextRange, usize), Global> = Vec::new_in(Global);
    for it in node.descendants() {
        if let Some(field) = ast::FieldExpr::cast(it) {
            let (Some(base), Some(name)) = (field.expr(), field.name_ref()) else { continue };
            if !is_base(scope, base) {
                continue;
            }
            if let Some(at) = scope.fields.iter().position(|it| *it == name.text()) {
                holes.push((name.syntax().text_range(), at));
            }
        } else if let Some(path) = ast::PathExpr::cast(it) {
            let Some(name) = path.path().and_then(single) else { continue };
            if let Some(&(_, at)) = locals.iter().find(|(local, _)| *local == name) {
                holes.push((it.text_range(), at));
            } else if scope.bases.contains(&name) && !under_field(it) {
                *whole = true;
            }
        }
    }
    let mut template = String::new();
    let mut reads: Vec<usize, Global> = Vec::new_in(Global);
    for element in node.descendants_with_tokens() {
        let NodeOrToken::Token(token) = element else { continue };
        if token.kind().is_trivia() {
            continue;
        }
        let range = token.text_range();
        let hole = holes.iter().find(|(hole, _)| hole.contains_range(range)).map(|(_, at)| *at);
        let named = match token.kind() {
            SyntaxKind::STRING => string_field(scope.fields, token.text()),
            _ => None,
        };
        let piece = if let Some(at) = hole {
            if !reads.contains(&at) {
                reads.push(at);
            }
            HOLE
        } else if let Some(at) = named {
            if !reads.contains(&at) {
                reads.push(at);
            }
            "\"#\""
        } else {
            token.text()
        };
        if !template.is_empty() {
            template.push(' ');
        }
        template.push_str(piece);
    }
    Term { owner, reads, template: normalize(scope.bases, template), range: node.text_range() }
}

/// Whether the path expression `node` is the base of a field expression, through `&`, `*` and
/// parentheses: `self` in `self.a` or `(*self).a`.
fn under_field(node: ast::Node<'_>) -> bool {
    let mut at = node;
    while let Some(parent) = at.parent() {
        match parent.kind() {
            SyntaxKind::PAREN_EXPR | SyntaxKind::REF_EXPR | SyntaxKind::PREFIX_EXPR => at = parent,
            SyntaxKind::FIELD_EXPR => {
                return ast::FieldExpr::cast(parent)
                    .and_then(|it| it.expr())
                    .is_some_and(|it| it.syntax() == at);
            }
            _ => return false,
        }
    }
    false
}

/// The field a string literal names, quotes and all (`"a"`).
fn string_field(fields: &[&str], text: &str) -> Option<usize> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    fields.iter().position(|it| *it == inner)
}

/// The spellings of one operation, made one template.
fn normalize(bases: &[&str], template: String) -> String {
    let mut text = template.replace("# . clone ( )", HOLE).replace("Clone :: clone ( & # )", HOLE);
    for base in bases {
        let from = format!("Clone :: clone ( & {base} . # )");
        if text.contains(&from) {
            text = text.replace(&from, &format!("{base} . #"));
        }
    }
    if is_default(&text) { DEFAULT.to_owned() } else { text }
}

/// Whether a template is a default value: `Default::default()`, `T::default()`, a zero, an
/// empty constructor of std's collections.
fn is_default(text: &str) -> bool {
    const CONSTANTS: &[&str] = &["0", "0.0", "false", "None", "\"\"", "Default :: default ( )"];
    const EMPTY: &[&str] = &[
        "String", "Vec", "VecDeque", "BTreeMap", "BTreeSet", "HashMap", "HashSet", "PathBuf",
        "OsString", "BinaryHeap",
    ];
    if CONSTANTS.contains(&text) {
        return true;
    }
    let Some((ty, call)) = text.split_once(" :: ") else { return false };
    let plain = ty.bytes().all(|it| it.is_ascii_alphanumeric() || it == b'_');
    plain && (call == "default ( )" || (call == "new ( )" && EMPTY.contains(&ty)))
}

/// The names a function's body reads fields of: `self` when it takes one, and each parameter
/// whose type is one of `makers` (through references).
pub(super) fn bases_of<'t>(f: ast::Fn<'t>, makers: &[&str]) -> Vec<&'t str, Global> {
    let mut out: Vec<&'t str, Global> = Vec::new_in(Global);
    let Some(list) = f.param_list() else { return out };
    if list.self_param().is_some() {
        out.push("self");
    }
    for param in list.params() {
        let (Some(ast::Pat::IdentPat(pat)), Some(ty)) = (param.pat(), param.ty()) else { continue };
        let Some(name) = pat.name() else { continue };
        if type_name(ty).is_some_and(|it| makers.contains(&it)) {
            out.push(name.text());
        }
    }
    out
}

/// The name a written type is of, through references: `Point` of `&mut geo::Point`.
pub(super) fn type_name(ty: ast::Type<'_>) -> Option<&str> {
    match ty {
        ast::Type::RefType(it) => type_name(it.ty()?),
        ast::Type::ParenType(it) => type_name(it.ty()?),
        ast::Type::PathType(it) => last(it.path()?),
        _ => None,
    }
}

/// The names of a function's parameters other than `self`, in order.
pub(super) fn param_names<'t>(f: ast::Fn<'t>) -> Vec<&'t str, Global> {
    let mut out: Vec<&'t str, Global> = Vec::new_in(Global);
    let Some(list) = f.param_list() else { return out };
    for param in list.params() {
        if let Some(ast::Pat::IdentPat(pat)) = param.pat()
            && let Some(name) = pat.name()
        {
            out.push(name.text());
        }
    }
    out
}

/// The template a body's text normalizes to with every space between tokens: what a check of
/// a derive's exact form compares with.
pub(super) fn tokens(node: ast::Node<'_>) -> String {
    let mut out = String::new();
    for element in node.descendants_with_tokens() {
        let NodeOrToken::Token(token) = element else { continue };
        if token.kind().is_trivia() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token.text());
    }
    out
}
