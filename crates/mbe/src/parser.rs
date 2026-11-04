//! Parser recognizes special macro syntax, `$var` and `$(repeat)*`, in token
//! trees.

use std::sync::Arc;

use arrayvec::ArrayVec;
use blaze_hash::HashMap;
use intern::{Symbol, sym};
use span::{Edition, Span, SyntaxContext};
use std::alloc::Global;
use tt::{
    MAX_GLUED_PUNCT_LEN,
    iter::{TtElement, TtIter},
};

use crate::{MacroCallStyle, ParseError};

pub(crate) fn parse_rule_style(src: &mut TtIter<'_>) -> Result<MacroCallStyle, ParseError> {
    // Skip an optional `unsafe`. This is only actually allowed for `attr`
    // rules, but we'll let rustc worry about that.
    if let Some(TtElement::Leaf(tt::Leaf::Ident(ident))) = src.peek()
        && ident.sym == sym::unsafe_
    {
        src.next().expect("already peeked");
    }

    let kind = match src.peek() {
        Some(TtElement::Leaf(tt::Leaf::Ident(ident))) if ident.sym == sym::attr => {
            src.next().expect("already peeked");
            // FIXME: Add support for `attr(..)` rules with attribute arguments,
            // which would be inside these parens.
            src.expect_subtree().map_err(|_| ParseError::expected("expected `()`"))?;
            MacroCallStyle::Attr
        }
        Some(TtElement::Leaf(tt::Leaf::Ident(ident))) if ident.sym == sym::derive => {
            src.next().expect("already peeked");
            src.expect_subtree().map_err(|_| ParseError::expected("expected `()`"))?;
            MacroCallStyle::Derive
        }
        _ => MacroCallStyle::FnLike,
    };
    Ok(kind)
}

/// Consider
///
/// ```
/// macro_rules! a_macro {
///     ($x:expr, $y:expr) => ($y * $x)
/// }
/// ```
///
/// Stuff to the left of `=>` is a [`MetaTemplate`] pattern (which is matched
/// with input).
///
/// Stuff to the right is a [`MetaTemplate`] template which is used to produce
/// output.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MetaTemplate(pub(crate) Box<[Op]>);

impl MetaTemplate {
    pub(crate) fn parse_pattern(
        edition: impl Copy + Fn(SyntaxContext) -> Edition,
        pattern: TtIter<'_>,
    ) -> Result<Self, ParseError> {
        MetaTemplate::parse(edition, pattern, Mode::Pattern)
    }

    pub(crate) fn parse_template(
        edition: impl Copy + Fn(SyntaxContext) -> Edition,
        template: TtIter<'_>,
    ) -> Result<Self, ParseError> {
        MetaTemplate::parse(edition, template, Mode::Template)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Op> {
        self.0.iter()
    }

    fn parse(
        edition: impl Copy + Fn(SyntaxContext) -> Edition,
        mut src: TtIter<'_>,
        mode: Mode,
    ) -> Result<Self, ParseError> {
        // alloc: the template, once per rule, when the macro is defined.
        let mut res = Vec::new();
        while let Some(first) = src.peek() {
            let op = next_op(edition, first, &mut src, mode)?;
            res.push(op);
        }

        Ok(MetaTemplate(res.into_boxed_slice()))
    }
}

/// The number of a metavariable its rule's pattern declares, given by [`resolve_binders`]: the
/// index of its bindings, so that matching and transcribing never hash a name.
pub(crate) type Binder = u32;

/// A metavariable the pattern does not declare (or one not resolved yet).
pub(crate) const NO_BINDER: Binder = u32::MAX;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Op {
    /// `spec` is a pattern's fragment specifier as written (`pat`, `pat_param`, `expr_2021`),
    /// and `decl` the span of `$name:spec` (a template's `$name`): rustc's `MetaVarDecl`.
    Var {
        name: MetaVar,
        kind: Option<MetaVarKind>,
        id: Span,
        binder: Binder,
        spec: Option<Symbol>,
        decl: Span,
    },
    Ignore {
        name: MetaVar,
        id: Span,
        binder: Binder,
    },
    Index {
        depth: usize,
    },
    Len {
        depth: usize,
    },
    Count {
        name: MetaVar,
        depth: usize,
        binder: Binder,
    },
    Concat {
        elements: Box<[ConcatMetaVarExprElem]>,
        span: Span,
    },
    Repeat {
        tokens: MetaTemplate,
        kind: RepeatKind,
        separator: Option<Arc<Separator>>,
    },
    Subtree {
        tokens: MetaTemplate,
        delimiter: tt::Delimiter,
    },
    Literal(tt::Literal),
    Punct(Box<ArrayVec<tt::Punct, MAX_GLUED_PUNCT_LEN>>),
    Ident(tt::Ident),
}

/// The name of a metavariable. Two of one spelling are two variables when they were written by
/// different expansions, as the `$arg`s of a macro that another macro writes out an argument at a
/// time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MetaVar {
    sym: Symbol,
    ctx: SyntaxContext,
}

impl MetaVar {
    pub(crate) fn new(sym: Symbol, span: Span) -> MetaVar {
        MetaVar { sym, ctx: span.ctx }
    }

    pub(crate) fn sym(&self) -> &Symbol {
        &self.sym
    }
}

impl std::fmt::Display for MetaVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.sym.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConcatMetaVarExprElem {
    /// There is NO preceding dollar sign, which means that this identifier should be interpreted
    /// as a literal.
    Ident(tt::Ident),
    /// There is a preceding dollar sign, which means that this identifier should be expanded
    /// and interpreted as a variable.
    Var { ident: tt::Ident, binder: Binder },
    /// For example, a number or a string.
    Literal(tt::Literal),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum RepeatKind {
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExprKind {
    // Matches expressions using the post-edition 2024. Was written using
    // `expr` in edition 2024 or later.
    Expr,
    // Matches expressions using the pre-edition 2024 rules.
    // Either written using `expr` in edition 2021 or earlier or.was written using `expr_2021`.
    Expr2021,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum MetaVarKind {
    Path,
    Ty,
    Pat,
    PatParam,
    Stmt,
    Block,
    Meta,
    Item,
    Vis,
    Expr(ExprKind),
    Ident,
    Tt,
    Lifetime,
    Literal,
}

#[derive(Clone, Debug, Eq)]
pub(crate) enum Separator {
    Literal(tt::Literal),
    Ident(tt::Ident),
    Puncts(ArrayVec<tt::Punct, MAX_GLUED_PUNCT_LEN>),
    Lifetime(tt::Punct, tt::Ident),
}

// Note that when we compare a Separator, we just care about its textual value.
impl PartialEq for Separator {
    fn eq(&self, other: &Separator) -> bool {
        use Separator::*;

        match (self, other) {
            (Ident(a), Ident(b)) => a.sym == b.sym,
            (Literal(a), Literal(b)) => a.text_and_suffix == b.text_and_suffix,
            (Puncts(a), Puncts(b)) if a.len() == b.len() => {
                let a_iter = a.iter().map(|a| a.char);
                let b_iter = b.iter().map(|b| b.char);
                a_iter.eq(b_iter)
            }
            (Lifetime(_, a), Lifetime(_, b)) => a.sym == b.sym,
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Pattern,
    Template,
}

fn next_op(
    edition: impl Copy + Fn(SyntaxContext) -> Edition,
    first_peeked: TtElement<'_>,
    src: &mut TtIter<'_>,
    mode: Mode,
) -> Result<Op, ParseError> {
    let res = match first_peeked {
        TtElement::Leaf(tt::Leaf::Punct(p @ tt::Punct { char: '$', .. })) => {
            src.next().expect("first token already peeked");
            // Note that the '$' itself is a valid token inside macro_rules.
            let second = match src.next() {
                None => {
                    return Ok(Op::Punct({
                        let mut res = ArrayVec::new();
                        res.push(p);
                        // alloc: the template, once per rule, when the macro is defined.
                        Box::new(res)
                    }));
                }
                Some(it) => it,
            };
            match second {
                TtElement::Subtree(subtree, mut subtree_iter) => match subtree.delimiter.kind {
                    tt::DelimiterKind::Parenthesis => {
                        let (separator, kind) = parse_repeat(src)?;
                        let tokens = MetaTemplate::parse(edition, subtree_iter, mode)?;
                        Op::Repeat { tokens, separator: separator.map(Arc::new), kind }
                    }
                    tt::DelimiterKind::Brace => match mode {
                        Mode::Template => parse_metavar_expr(&mut subtree_iter).map_err(|()| {
                            ParseError::unexpected("invalid metavariable expression")
                        })?,
                        Mode::Pattern => {
                            return Err(ParseError::unexpected(
                                "`${}` metavariable expressions are not allowed in matchers",
                            ));
                        }
                    },
                    _ => {
                        return Err(ParseError::expected(
                            "expected `$()` repetition or `${}` expression",
                        ));
                    }
                },
                TtElement::Leaf(leaf) => match leaf {
                    tt::Leaf::Ident(ident) if ident.sym == sym::crate_ => {
                        // We simply produce identifier `$crate` here. And it will be resolved when lowering ast to Path.
                        Op::Ident(tt::Ident {
                            sym: sym::dollar_crate,
                            span: ident.span,
                            is_raw: tt::IdentIsRaw::No,
                        })
                    }
                    tt::Leaf::Ident(ident) => {
                        let (kind, spec) = eat_fragment_kind(edition, src, mode)?;
                        let name = MetaVar::new(ident.sym, ident.span);
                        let id = ident.span;
                        let decl = p.span.cover(spec.as_ref().map_or(id, |it| it.span));
                        let spec = spec.map(|it| it.sym);
                        Op::Var { name, kind, id, binder: NO_BINDER, spec, decl }
                    }
                    tt::Leaf::Literal(lit) if is_boolean_literal(&lit) => {
                        let (kind, spec) = eat_fragment_kind(edition, src, mode)?;
                        let name = MetaVar::new(lit.text_and_suffix, lit.span);
                        let id = lit.span;
                        let decl = p.span.cover(spec.as_ref().map_or(id, |it| it.span));
                        let spec = spec.map(|it| it.sym);
                        Op::Var { name, kind, id, binder: NO_BINDER, spec, decl }
                    }
                    tt::Leaf::Punct(punct @ tt::Punct { char: '$', .. }) => match mode {
                        Mode::Pattern => {
                            return Err(ParseError::unexpected(
                                "`$$` is not allowed on the pattern side",
                            ));
                        }
                        Mode::Template => Op::Punct({
                            let mut res = ArrayVec::new();
                            res.push(punct);
                            // alloc: the template, once per rule, when the macro is defined.
                            Box::new(res)
                        }),
                    },
                    tt::Leaf::Punct(_) | tt::Leaf::Literal(_) => {
                        return Err(ParseError::expected("expected ident"));
                    }
                },
            }
        }

        TtElement::Leaf(tt::Leaf::Literal(it)) => {
            src.next().expect("first token already peeked");
            // clone: the template, once per rule, when the macro is defined.
            Op::Literal(it.clone())
        }

        TtElement::Leaf(tt::Leaf::Ident(it)) => {
            src.next().expect("first token already peeked");
            // clone: the template, once per rule, when the macro is defined.
            Op::Ident(it.clone())
        }

        TtElement::Leaf(tt::Leaf::Punct(_)) => {
            // There's at least one punct so this shouldn't fail.
            let puncts = src.expect_glued_punct().unwrap();
            // alloc: the template, once per rule, when the macro is defined.
            Op::Punct(Box::new(puncts))
        }

        TtElement::Subtree(subtree, subtree_iter) => {
            src.next().expect("first token already peeked");
            let tokens = MetaTemplate::parse(edition, subtree_iter, mode)?;
            Op::Subtree { tokens, delimiter: subtree.delimiter }
        }
    };
    Ok(res)
}

fn eat_fragment_kind(
    edition: impl Copy + Fn(SyntaxContext) -> Edition,
    src: &mut TtIter<'_>,
    mode: Mode,
) -> Result<(Option<MetaVarKind>, Option<tt::Ident>), ParseError> {
    if let Mode::Pattern = mode {
        src.expect_char(':').map_err(|()| ParseError::unexpected("missing fragment specifier"))?;
        let ident = src
            .expect_ident()
            .map_err(|()| ParseError::unexpected("missing fragment specifier"))?;
        let kind = match ident.sym.as_str() {
            "path" => MetaVarKind::Path,
            "ty" => MetaVarKind::Ty,
            "pat" => {
                if edition(ident.span.ctx).at_least_2021() {
                    MetaVarKind::Pat
                } else {
                    MetaVarKind::PatParam
                }
            }
            "pat_param" => MetaVarKind::PatParam,
            "stmt" => MetaVarKind::Stmt,
            "block" => MetaVarKind::Block,
            "meta" => MetaVarKind::Meta,
            "item" => MetaVarKind::Item,
            "vis" => MetaVarKind::Vis,
            "expr" => {
                if edition(ident.span.ctx).at_least_2024() {
                    MetaVarKind::Expr(ExprKind::Expr)
                } else {
                    MetaVarKind::Expr(ExprKind::Expr2021)
                }
            }
            "expr_2021" => MetaVarKind::Expr(ExprKind::Expr2021),
            "ident" => MetaVarKind::Ident,
            "tt" => MetaVarKind::Tt,
            "lifetime" => MetaVarKind::Lifetime,
            "literal" => MetaVarKind::Literal,
            _ => return Ok((None, Some(ident))),
        };
        return Ok((Some(kind), Some(ident)));
    };
    Ok((None, None))
}

fn is_boolean_literal(lit: &tt::Literal) -> bool {
    lit.text_and_suffix == sym::true_ || lit.text_and_suffix == sym::false_
}

/// rustc's `parse_sep_and_kleene_op`: the separator, if any, and the operator after a `$(...)`.
/// The separator is any one token but a delimited group or an operator, punctuation glued into
/// one as rustc's lexer glues it ([`tt::glue`]): `$(a)::+` and `$(a)=>*` have the separators
/// `::` and `=>`, `$(a)+=*` the separator `+=`, while `$(a)++` is the operator `+` and then a
/// `+` token, and `$(a),?` is refused, as `?` takes no separator.
fn parse_repeat(src: &mut TtIter<'_>) -> Result<(Option<Separator>, RepeatKind), ParseError> {
    let separator = match kleene_or_separator(src)? {
        Ok(kind) => return Ok((None, kind)),
        Err(separator) => separator,
    };
    match kleene_or_separator(src)? {
        Ok(RepeatKind::ZeroOrOne) | Err(_) => Err(ParseError::InvalidRepeat),
        Ok(kind) => Ok((Some(separator), kind)),
    }
}

/// rustc's `parse_kleene_op`: the next token, as an operator (`Ok`) or as a separator (`Err`);
/// an error when it is a group or there is none.
fn kleene_or_separator(src: &mut TtIter<'_>) -> Result<Result<RepeatKind, Separator>, ParseError> {
    let separator = match src.peek() {
        Some(TtElement::Leaf(tt::Leaf::Ident(ident))) => Separator::Ident(ident),
        Some(TtElement::Leaf(tt::Leaf::Literal(lit))) => Separator::Literal(lit),
        Some(TtElement::Leaf(tt::Leaf::Punct(_))) => {
            let puncts = src.expect_glued_punct().map_err(|()| ParseError::InvalidRepeat)?;
            return Ok(match puncts.as_slice() {
                [tt::Punct { char: '*', .. }] => Ok(RepeatKind::ZeroOrMore),
                [tt::Punct { char: '+', .. }] => Ok(RepeatKind::OneOrMore),
                [tt::Punct { char: '?', .. }] => Ok(RepeatKind::ZeroOrOne),
                // A lifetime is one token to rustc.
                [quote @ tt::Punct { char: '\'', .. }] => {
                    let name =
                        src.expect_ident_or_underscore().map_err(|()| ParseError::InvalidRepeat)?;
                    Err(Separator::Lifetime(*quote, name))
                }
                _ => Err(Separator::Puncts(puncts)),
            });
        }
        Some(TtElement::Subtree(..)) | None => return Err(ParseError::InvalidRepeat),
    };
    src.next();
    Ok(Err(separator))
}

fn parse_metavar_expr(src: &mut TtIter<'_>) -> Result<Op, ()> {
    let func = src.expect_ident()?;
    let (args, mut args_iter) = src.expect_subtree()?;

    if args.delimiter.kind != tt::DelimiterKind::Parenthesis {
        return Err(());
    }

    let op = match &func.sym {
        s if sym::ignore == *s => {
            args_iter.expect_dollar()?;
            let ident = args_iter.expect_ident()?;
            Op::Ignore {
                name: MetaVar::new(ident.sym, ident.span),
                id: ident.span,
                binder: NO_BINDER,
            }
        }
        s if sym::index == *s => Op::Index { depth: parse_depth(&mut args_iter)? },
        s if sym::len == *s => Op::Len { depth: parse_depth(&mut args_iter)? },
        s if sym::count == *s => {
            args_iter.expect_dollar()?;
            let ident = args_iter.expect_ident()?;
            let depth =
                if try_eat_comma(&mut args_iter) { parse_depth(&mut args_iter)? } else { 0 };
            Op::Count { name: MetaVar::new(ident.sym, ident.span), depth, binder: NO_BINDER }
        }
        s if sym::concat == *s => {
            // alloc: the template, once per rule, when the macro is defined.
            let mut elements = Vec::new();
            while let Some(next) = args_iter.peek() {
                let element = if let TtElement::Leaf(tt::Leaf::Literal(lit)) = next {
                    args_iter.next().expect("already peeked");
                    // clone: the template, once per rule, when the macro is defined.
                    ConcatMetaVarExprElem::Literal(lit.clone())
                } else {
                    let is_var = try_eat_dollar(&mut args_iter);
                    // clone: the template, once per rule, when the macro is defined.
                    let ident = args_iter.expect_ident_or_underscore()?.clone();

                    if is_var {
                        ConcatMetaVarExprElem::Var { ident, binder: NO_BINDER }
                    } else {
                        ConcatMetaVarExprElem::Ident(ident)
                    }
                };
                elements.push(element);
                if !args_iter.is_empty() {
                    args_iter.expect_comma()?;
                }
            }
            if elements.len() < 2 {
                return Err(());
            }
            Op::Concat { elements: elements.into_boxed_slice(), span: func.span }
        }
        _ => return Err(()),
    };

    if args_iter.next().is_some() {
        return Err(());
    }

    Ok(op)
}

fn parse_depth(src: &mut TtIter<'_>) -> Result<usize, ()> {
    if src.is_empty() {
        Ok(0)
    } else if let tt::Leaf::Literal(lit) = src.expect_literal()?
        && let (text, suffix) = lit.text_and_suffix()
        && suffix.is_empty()
    {
        // Suffixes are not allowed.
        text.parse().map_err(|_| ())
    } else {
        Err(())
    }
}

fn try_eat_comma(src: &mut TtIter<'_>) -> bool {
    if let Some(TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: ',', .. }))) = src.peek() {
        let _ = src.next();
        return true;
    }
    false
}

fn try_eat_dollar(src: &mut TtIter<'_>) -> bool {
    if let Some(TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: '$', .. }))) = src.peek() {
        let _ = src.next();
        return true;
    }
    false
}

/// Numbers the metavariables `lhs` declares, one number per name in the order the names first
/// appear, and gives every metavariable of `lhs` and `rhs` its name's number ([`NO_BINDER`] for
/// one `lhs` declares no binding for). Returns how many numbers there are. Runs once per rule,
/// when the macro is defined.
pub(crate) fn resolve_binders(lhs: &mut MetaTemplate, rhs: &mut MetaTemplate) -> u32 {
    let mut binders = HashMap::new_in(Global);
    declare(lhs, &mut binders);
    resolve(rhs, &binders);
    return binders.len() as u32;

    fn declare(template: &mut MetaTemplate, binders: &mut HashMap<MetaVar, Binder, Global>) {
        for op in template.0.iter_mut() {
            match op {
                Op::Var { name, binder, .. } => {
                    let next = binders.len() as Binder;
                    *binder = *binders.entry(*name).or_insert(next);
                }
                Op::Repeat { tokens, .. } | Op::Subtree { tokens, .. } => declare(tokens, binders),
                _ => (),
            }
        }
    }

    fn resolve(template: &mut MetaTemplate, binders: &HashMap<MetaVar, Binder, Global>) {
        let lookup = |name: &MetaVar| binders.get(name).copied().unwrap_or(NO_BINDER);
        for op in template.0.iter_mut() {
            match op {
                Op::Var { name, binder, .. }
                | Op::Ignore { name, binder, .. }
                | Op::Count { name, binder, .. } => *binder = lookup(name),
                Op::Concat { elements, .. } => {
                    for element in elements.iter_mut() {
                        if let ConcatMetaVarExprElem::Var { ident, binder } = element {
                            *binder = lookup(&MetaVar::new(ident.sym, ident.span));
                        }
                    }
                }
                Op::Repeat { tokens, .. } | Op::Subtree { tokens, .. } => resolve(tokens, binders),
                _ => (),
            }
        }
    }
}
