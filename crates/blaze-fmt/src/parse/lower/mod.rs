//! rust-analyzer's syntax tree lowered to [`crate::ast`] with the spans rustc's parser
//! (`compiler/rustc_parse/src/parser/*.rs` at rust-lang/rust 12433c307) gives each node: rustfmt
//! recovers comments and blank lines from the text between spans, so a span that starts or ends
//! one token off changes its output. Each rule cites where rustc takes it.
//!
//! The tree is parsed from some text of a source-map file whose first byte is at `base`; token
//! streams (macro arguments, attribute arguments) come from rustc's lexer over the same text, so
//! their spans agree with the nodes'. A construct this cannot lower yet stops the whole file with
//! a reason rather than formatting it some other way.

mod expr;
mod item;
mod pat;
mod ty;

#[cfg(test)]
mod tests;

use std::cell::RefCell;
use std::collections::HashMap;

use syntax::view::{self as ra, AstNode, HasGenericArgs as _, HasName as _, HasTypeBounds as _};
use syntax::{NodeOrToken, SyntaxKind, T, TextRange};

use crate::ast::token::{self, Delimiter, Token, TokenKind};
use crate::ast::tokenstream::{DelimSpan, TokenStream, TokenTree};
use crate::ast::{self, AttrStyle, DUMMY_NODE_ID, ThinVec};
use crate::parse::lexer::{self, StripTokens};
use crate::span::edition::Edition;
use crate::span::{AttrId, BytePos, Ident, Span, Symbol, kw, sym};

/// A delimited token tree found by the position of its opening delimiter.
#[derive(Clone)]
pub(crate) struct Delimited {
    pub(crate) dspan: DelimSpan,
    pub(crate) delim: Delimiter,
    pub(crate) tokens: TokenStream,
}

/// Every delimited group of a token stream, by the position of its opening delimiter.
#[derive(Default)]
pub(crate) struct DelimIndex {
    by_open: HashMap<BytePos, Delimited>,
}

impl DelimIndex {
    pub(crate) fn new(stream: &TokenStream) -> DelimIndex {
        let mut index = DelimIndex::default();
        index.add(stream);
        index
    }

    fn add(&mut self, stream: &TokenStream) {
        for tt in stream.iter() {
            if let TokenTree::Delimited(dspan, _, delim, inner) = tt {
                self.by_open.insert(
                    dspan.open.lo(),
                    Delimited {
                        dspan: *dspan,
                        delim: *delim,
                        tokens: inner.clone(),
                    },
                );
                self.add(inner);
            }
        }
    }

    pub(crate) fn get(&self, open: BytePos) -> Option<&Delimited> {
        self.by_open.get(&open)
    }
}

/// Why a file could not be lowered: rustfmt would see it differently, so it is left alone.
#[derive(Debug, Clone)]
pub(crate) struct Unsupported {
    pub(crate) span: Span,
    pub(crate) message: String,
    /// rustc's parser rejects this too (rust-analyzer's accepts it), so it is a parse error
    /// rather than something blaze has yet to cover.
    pub(crate) rustc_rejects: bool,
}

pub(crate) struct Lower<'a> {
    base: BytePos,
    edition: Edition,
    delims: &'a DelimIndex,
    error: RefCell<Option<Unsupported>>,
}

impl<'a> Lower<'a> {
    pub(crate) fn new(base: BytePos, edition: Edition, delims: &'a DelimIndex) -> Lower<'a> {
        Lower {
            base,
            edition,
            delims,
            error: RefCell::new(None),
        }
    }

    pub(crate) fn finish(self) -> Result<(), Unsupported> {
        match self.error.into_inner() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn unsupported(&self, span: Span, message: impl Into<String>) {
        self.fail(span, message.into(), false);
    }

    /// What rust-analyzer's parser accepts and rustc's does not, with rustc's message.
    pub(crate) fn rustc_rejects(&self, span: Span, message: &str) {
        self.fail(span, message.to_owned(), true);
    }

    fn fail(&self, span: Span, message: String, rustc_rejects: bool) {
        let mut error = self.error.borrow_mut();
        if error.is_none() {
            *error = Some(Unsupported {
                span,
                message,
                rustc_rejects,
            });
        }
    }

    // ---- spans ----

    pub(crate) fn pos(&self, offset: syntax::TextSize) -> BytePos {
        self.base + BytePos(u32::from(offset))
    }

    pub(crate) fn sp(&self, range: TextRange) -> Span {
        Span::with_root_ctxt(self.pos(range.start()), self.pos(range.end()))
    }

    pub(crate) fn tsp(&self, token: ra::Token<'_>) -> Span {
        self.sp(token.text_range())
    }

    /// A node's span as rustc's `lo.to(self.prev_token.span)`: its first token after its own
    /// outer attributes and doc comments, to its last token.
    pub(crate) fn nsp(&self, node: ra::Node<'_>) -> Span {
        match (first_token_after_attrs(node), last_token(node)) {
            (Some(first), Some(last)) => self.tsp(first).to(self.tsp(last)),
            _ => self.sp(TextRange::empty(node.text_range().start())),
        }
    }

    /// A node's span including its outer attributes (`lo` taken before `parse_outer_attributes`).
    pub(crate) fn nsp_with_attrs(&self, node: ra::Node<'_>) -> Span {
        match (first_token(node), last_token(node)) {
            (Some(first), Some(last)) => self.tsp(first).to(self.tsp(last)),
            _ => self.sp(TextRange::empty(node.text_range().start())),
        }
    }

    // ---- tokens ----

    pub(crate) fn ident(&self, token: ra::Token<'_>) -> Ident {
        let text = token.text();
        let name = text.strip_prefix("r#").unwrap_or(text);
        Ident::new(Symbol::intern(name), self.tsp(token))
    }

    pub(crate) fn name(&self, name: ra::Name<'_>) -> Ident {
        match name.syntax().first_token() {
            Some(token) => self.ident(token),
            None => {
                let span = self.sp(name.syntax().text_range());
                self.unsupported(span, "a name without a token");
                Ident::new(sym::empty, span)
            }
        }
    }

    pub(crate) fn name_ref(&self, name: ra::NameRef<'_>) -> Ident {
        match name.syntax().first_token() {
            Some(token) => self.ident(token),
            None => {
                let span = self.sp(name.syntax().text_range());
                self.unsupported(span, "a name without a token");
                Ident::new(sym::empty, span)
            }
        }
    }

    pub(crate) fn lifetime(&self, lt: ra::Lifetime<'_>) -> ast::Lifetime {
        let token = lt.lifetime_ident_token();
        let ident = match token {
            Some(token) => {
                let text = token.text();
                let name = match text.strip_prefix("'r#") {
                    Some(rest) => format!("'{rest}"),
                    None => text.to_owned(),
                };
                Ident::new(Symbol::intern(&name), self.tsp(token))
            }
            None => {
                let span = self.sp(lt.syntax().text_range());
                self.unsupported(span, "a lifetime without a name");
                Ident::new(sym::empty, span)
            }
        };
        ast::Lifetime {
            id: DUMMY_NODE_ID,
            ident,
        }
    }

    /// A literal token as rustc's lexer cooks it.
    pub(crate) fn token_lit(&self, token: ra::Token<'_>) -> token::Lit {
        let text = token.text();
        match text {
            "true" => return token::Lit::new(token::Bool, kw::True, None),
            "false" => return token::Lit::new(token::Bool, kw::False, None),
            _ => {}
        }
        let span = self.tsp(token);
        match lexer::lex_token_trees(text, span.lo(), self.edition, StripTokens::Nothing) {
            Ok(stream) => match stream.iter().next() {
                Some(TokenTree::Token(
                    Token {
                        kind: TokenKind::Literal(lit),
                        ..
                    },
                    _,
                )) if stream.len() == 1 => *lit,
                _ => {
                    self.unsupported(span, "a literal rustc lexes as several tokens");
                    token::Lit::new(
                        token::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed()),
                        Symbol::intern(text),
                        None,
                    )
                }
            },
            Err(err) => {
                self.unsupported(err.span, err.message);
                token::Lit::new(
                    token::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed()),
                    Symbol::intern(text),
                    None,
                )
            }
        }
    }

    pub(crate) fn delimited(&self, open: ra::Token<'_>) -> Option<Delimited> {
        let found = self.delims.get(self.tsp(open).lo()).cloned();
        if found.is_none() {
            self.unsupported(self.tsp(open), "a delimiter rustc's lexer did not pair");
        }
        found
    }

    /// The delimited arguments of a macro call or attribute, from the token tree's first token.
    pub(crate) fn delim_args(&self, tt: ra::TokenTree<'_>) -> ast::DelimArgs {
        let open = tt.syntax().first_token();
        match open.and_then(|open| self.delimited(open)) {
            Some(d) => ast::DelimArgs {
                dspan: d.dspan,
                delim: d.delim,
                tokens: d.tokens,
            },
            None => ast::DelimArgs {
                dspan: DelimSpan::from_single(self.sp(tt.syntax().text_range())),
                delim: Delimiter::Parenthesis,
                tokens: TokenStream::default(),
            },
        }
    }

    // ---- attributes ----

    /// Outer attributes and doc comments written on `node`, in order.
    pub(crate) fn outer_attrs(&self, node: ra::Node<'_>) -> ast::AttrVec {
        let mut attrs = ThinVec::new();
        for child in node.children() {
            if let Some(any) = ra::AnyAttr::cast(child)
                && let Some(attr) = self.attr(any)
                && attr.style == AttrStyle::Outer
            {
                attrs.push(attr);
            }
        }
        attrs
    }

    /// Inner attributes (`#![..]`, `//!`) that are direct children of `node`.
    pub(crate) fn inner_attrs(&self, node: ra::Node<'_>) -> ast::AttrVec {
        let mut attrs = ThinVec::new();
        for child in node.children() {
            if let Some(any) = ra::AnyAttr::cast(child)
                && let Some(attr) = self.attr(any)
                && attr.style == AttrStyle::Inner
            {
                attrs.push(attr);
            }
        }
        attrs
    }

    pub(crate) fn attr(&self, any: ra::AnyAttr<'_>) -> Option<ast::Attribute> {
        match any {
            ra::AnyAttr::DocComment(doc) => {
                let token = doc.syntax().first_token()?;
                let text = token.text();
                let (kind, style, content) = if let Some(rest) = text.strip_prefix("///") {
                    (token::CommentKind::Line, AttrStyle::Outer, rest)
                } else if let Some(rest) = text.strip_prefix("//!") {
                    (token::CommentKind::Line, AttrStyle::Inner, rest)
                } else if let Some(rest) = text.strip_prefix("/**") {
                    (
                        token::CommentKind::Block,
                        AttrStyle::Outer,
                        rest.strip_suffix("*/")?,
                    )
                } else {
                    let rest = text.strip_prefix("/*!")?;
                    (
                        token::CommentKind::Block,
                        AttrStyle::Inner,
                        rest.strip_suffix("*/")?,
                    )
                };
                Some(ast::Attribute {
                    kind: ast::AttrKind::DocComment(kind, Symbol::intern(content)),
                    id: AttrId(0),
                    style,
                    span: self.tsp(token),
                })
            }
            ra::AnyAttr::Attr(attr) => self.normal_attr(attr),
        }
    }

    /// `#[..]` or `#![..]`, read from rustc's tokens inside the brackets (rustc's
    /// `parse_attribute` and `parse_attr_item`), with the expression of `#[k = expr]` lowered
    /// from rust-analyzer's tree.
    fn normal_attr(&self, attr: ra::Attr<'_>) -> Option<ast::Attribute> {
        let node = attr.syntax();
        let style = if attr.excl_token().is_some() {
            AttrStyle::Inner
        } else {
            AttrStyle::Outer
        };
        let span = self.sp(node.text_range());
        let Some(open) = node.child_token(T!['[']) else {
            self.unsupported(span, "an attribute without brackets");
            return None;
        };
        let group = self.delimited(open)?;
        let item = self.attr_item(&group.tokens, attr, span)?;
        Some(ast::Attribute {
            kind: ast::AttrKind::Normal(Box::new(ast::NormalAttr { item, tokens: None })),
            id: AttrId(0),
            style,
            span,
        })
    }

    fn attr_item(
        &self,
        tokens: &TokenStream,
        attr: ra::Attr<'_>,
        span: Span,
    ) -> Option<ast::AttrItem> {
        let tts: Vec<&TokenTree> = tokens.iter().collect();
        let mut i = 0;
        let lo = tts.first().map(|tt| tt.span())?;
        let mut unsafety = ast::Safety::Default;
        let mut inner: Vec<&TokenTree> = tts.clone();
        if let Some(TokenTree::Token(t, _)) = tts.first()
            && t.is_keyword(kw::Unsafe)
            && let Some(TokenTree::Delimited(_, _, Delimiter::Parenthesis, stream)) = tts.get(1)
        {
            unsafety = ast::Safety::Unsafe(t.span);
            inner = stream.iter().collect();
        }
        let path = self.token_path(&inner, &mut i, span)?;
        let args = match inner.get(i) {
            None => ast::AttrArgs::Empty,
            Some(TokenTree::Delimited(dspan, _, delim, stream)) => {
                ast::AttrArgs::Delimited(ast::DelimArgs {
                    dspan: *dspan,
                    delim: *delim,
                    tokens: stream.clone(),
                })
            }
            Some(TokenTree::Token(t, _)) if t.kind == token::Eq => {
                let eq_span = t.span;
                let expr = attr
                    .syntax()
                    .descendants()
                    .find_map(ra::KeyValueMeta::cast)
                    .and_then(|kv| kv.expr());
                let Some(expr) = expr else {
                    self.unsupported(span, "an attribute value rust-analyzer did not parse");
                    return None;
                };
                ast::AttrArgs::Eq {
                    eq_span,
                    expr: self.expr(expr),
                }
            }
            Some(tt) => {
                self.unsupported(tt.span(), "an attribute rustc would reject");
                return None;
            }
        };
        let hi = tts.last().map(|tt| tt.span())?;
        Some(ast::AttrItem {
            unsafety,
            path,
            args,
            span: lo.to(hi),
        })
    }

    /// A module-style path read from tokens, as attribute and macro paths are.
    fn token_path(&self, tts: &[&TokenTree], i: &mut usize, span: Span) -> Option<ast::Path> {
        let mut segments = ThinVec::new();
        let start = *i;
        if let Some(TokenTree::Token(t, _)) = tts.get(*i)
            && t.kind == token::PathSep
        {
            segments.push(ast::PathSegment::path_root(t.span.shrink_to_lo()));
            *i += 1;
        }
        while let Some(TokenTree::Token(t, _)) = tts.get(*i) {
            match t.ident() {
                Some((ident, _)) => {
                    segments.push(ast::PathSegment::from_ident(ident));
                    *i += 1;
                }
                None => break,
            }
            match tts.get(*i) {
                Some(TokenTree::Token(t, _)) if t.kind == token::PathSep => *i += 1,
                _ => break,
            }
        }
        if segments.is_empty() || *i == start {
            self.unsupported(span, "an attribute without a path");
            return None;
        }
        let lo = tts[start].span();
        let hi = tts[*i - 1].span();
        Some(ast::Path {
            span: lo.to(hi),
            segments,
        })
    }

    // ---- paths ----

    /// A path, flattened from rust-analyzer's qualifier chain (rustc's `parse_path` and
    /// `parse_qpath`).
    pub(crate) fn path(&self, path: ra::Path<'_>) -> (Option<Box<ast::QSelf>>, ast::Path) {
        let mut innermost = path;
        while let Some(q) = innermost.qualifier() {
            innermost = q;
        }
        // A qualifier is a child of the path it qualifies, so its parent is the next segment out.
        let chain = std::iter::successors(Some(innermost), |p| {
            if *p == path {
                None
            } else {
                p.syntax().parent().and_then(ra::Path::cast)
            }
        });
        let span = self.nsp(path.syntax());
        let mut segments = ThinVec::new();
        let mut qself = None;
        for (n, p) in chain.enumerate() {
            let Some(seg) = p.segment() else {
                self.unsupported(span, "a path segment rust-analyzer did not parse");
                continue;
            };
            if n == 0 && seg.coloncolon_token().is_some() {
                let root = seg.coloncolon_token().map(|t| self.tsp(t)).unwrap_or(span);
                segments.push(ast::PathSegment::path_root(root.shrink_to_lo()));
            }
            if let Some(anchor) = seg.type_anchor() {
                // `<T as Trait>::rest`: rustc keeps `Trait`'s segments in the path and counts
                // them in `QSelf::position`.
                // The self type is the first type child and may be a path type itself, so
                // `Trait` is the type after `as`, not the anchor's first path type.
                let mut types = anchor.syntax().children().filter_map(ra::Type::cast);
                let Some(ty) = types.next() else {
                    self.unsupported(span, "a qualified path without a type");
                    continue;
                };
                let ty = self.ty(ty);
                let trait_path = match types.next() {
                    Some(ra::Type::PathType(pt)) => pt.path(),
                    Some(other) => {
                        self.unsupported(
                            self.nsp(other.syntax()),
                            "a qualified path whose trait is not a path",
                        );
                        None
                    }
                    None => None,
                };
                let path_span = match trait_path {
                    Some(trait_path) => {
                        let (_, trait_path) = self.path(trait_path);
                        let path_span = trait_path.span;
                        segments.extend(trait_path.segments);
                        path_span
                    }
                    None => match anchor.r_angle_token() {
                        Some(gt) => self.tsp(gt),
                        None => span,
                    },
                };
                qself = Some(Box::new(ast::QSelf {
                    ty,
                    path_span,
                    position: segments.len(),
                }));
                continue;
            }
            segments.push(self.path_segment(seg));
        }
        (qself, ast::Path { span, segments })
    }

    pub(crate) fn path_segment(&self, seg: ra::PathSegment<'_>) -> ast::PathSegment {
        let ident = match seg.name_ref() {
            Some(name) => self.name_ref(name),
            None => {
                let span = self.nsp(seg.syntax());
                self.unsupported(span, "a path segment without a name");
                Ident::new(sym::empty, span)
            }
        };
        let args = if let Some(list) = seg.generic_arg_list() {
            Some(Box::new(self.angle_args(list)))
        } else if let Some(list) = seg.parenthesized_arg_list() {
            Some(Box::new(self.paren_args(ident, list, seg.ret_type())))
        } else if let Some(rts) = seg.return_type_syntax() {
            let span = self.nsp(rts.syntax());
            Some(Box::new(ast::GenericArgs::ParenthesizedElided(span)))
        } else {
            None
        };
        ast::PathSegment {
            ident,
            id: DUMMY_NODE_ID,
            args,
        }
    }

    /// `<..>` with its span from `<` to `>`, leaving out a turbofish's `::`.
    pub(crate) fn angle_args(&self, list: ra::GenericArgList<'_>) -> ast::GenericArgs {
        let lo = list.l_angle_token().map(|t| self.tsp(t));
        let hi = list.r_angle_token().map(|t| self.tsp(t));
        let span = match (lo, hi) {
            (Some(lo), Some(hi)) => lo.to(hi),
            _ => self.nsp(list.syntax()),
        };
        let args = list
            .generic_args()
            .map(|arg| self.angle_arg(arg))
            .collect();
        ast::GenericArgs::AngleBracketed(ast::AngleBracketedArgs { span, args })
    }

    fn angle_arg(&self, arg: ra::GenericArg<'_>) -> ast::AngleBracketedArg {
        match arg {
            ra::GenericArg::TypeArg(t) => match t.ty() {
                Some(ty) => ast::AngleBracketedArg::Arg(ast::GenericArg::Type(self.ty(ty))),
                None => self.missing_arg(t.syntax()),
            },
            ra::GenericArg::LifetimeArg(l) => match l.lifetime() {
                Some(lt) => {
                    ast::AngleBracketedArg::Arg(ast::GenericArg::Lifetime(self.lifetime(lt)))
                }
                None => self.missing_arg(l.syntax()),
            },
            ra::GenericArg::ConstArg(c) => match c.expr() {
                Some(e) => ast::AngleBracketedArg::Arg(ast::GenericArg::Const(self.anon_const(e))),
                None => self.missing_arg(c.syntax()),
            },
            ra::GenericArg::AssocTypeArg(a) => {
                ast::AngleBracketedArg::Constraint(self.assoc_constraint(a))
            }
        }
    }

    fn missing_arg(&self, node: ra::Node<'_>) -> ast::AngleBracketedArg {
        let span = self.nsp(node);
        self.unsupported(span, "a generic argument rust-analyzer did not parse");
        ast::AngleBracketedArg::Arg(ast::GenericArg::Type(self.ty_err(span)))
    }

    /// `Item = T`, `Item: Bound` (rustc's `parse_angle_arg`: from the name to the last token).
    fn assoc_constraint(&self, a: ra::AssocTypeArg<'_>) -> ast::AssocItemConstraint {
        let span = self.nsp(a.syntax());
        let ident = match a.name_ref() {
            Some(n) => self.name_ref(n),
            None => {
                self.unsupported(span, "an associated item constraint without a name");
                Ident::new(sym::empty, span)
            }
        };
        let gen_args = if let Some(list) = a.generic_arg_list() {
            Some(self.angle_args(list))
        } else if let Some(params) = a.param_list() {
            Some(self.paren_args_from_params(ident, params, a.ret_type()))
        } else {
            a.return_type_syntax()
                .map(|rts| ast::GenericArgs::ParenthesizedElided(self.nsp(rts.syntax())))
        };
        let kind = if let Some(bounds) = a.type_bound_list() {
            ast::AssocItemConstraintKind::Bound {
                bounds: self.bounds(bounds),
            }
        } else if let Some(ty) = a.ty() {
            ast::AssocItemConstraintKind::Equality {
                term: ast::Term::Ty(self.ty(ty)),
            }
        } else if let Some(c) = a.const_arg().and_then(|c| c.expr()) {
            ast::AssocItemConstraintKind::Equality {
                term: ast::Term::Const(self.anon_const(c)),
            }
        } else if a.colon_token().is_some() {
            ast::AssocItemConstraintKind::Bound {
                bounds: ThinVec::new(),
            }
        } else {
            self.unsupported(
                span,
                "an associated item constraint rust-analyzer did not parse",
            );
            ast::AssocItemConstraintKind::Bound {
                bounds: ThinVec::new(),
            }
        };
        ast::AssocItemConstraint {
            id: DUMMY_NODE_ID,
            ident,
            gen_args,
            kind,
            span,
        }
    }

    /// `Fn(A, B) -> C`: `span` from the name to the end of the output, `inputs_span` the
    /// parentheses (rustc's `parse_path_segment`).
    fn paren_args(
        &self,
        ident: Ident,
        list: ra::ParenthesizedArgList<'_>,
        ret: Option<ra::RetType<'_>>,
    ) -> ast::GenericArgs {
        let inputs_span = match (list.l_paren_token(), list.r_paren_token()) {
            (Some(l), Some(r)) => self.tsp(l).to(self.tsp(r)),
            _ => self.nsp(list.syntax()),
        };
        let inputs = list
            .type_args()
            .filter_map(|arg| arg.ty())
            .map(|ty| {
                let ty = self.ty(ty);
                let pat = Box::new(ast::Pat {
                    id: DUMMY_NODE_ID,
                    kind: ast::PatKind::Missing,
                    span: ty.span,
                });
                ast::Param {
                    attrs: ThinVec::new(),
                    span: ty.span,
                    ty,
                    pat,
                    id: DUMMY_NODE_ID,
                    is_placeholder: false,
                    has_const_keyword: false,
                }
            })
            .collect();
        let output = self.ret_ty(ret, inputs_span);
        let hi = match &output {
            ast::FnRetTy::Ty(ty) => ty.span,
            ast::FnRetTy::Default(_) => inputs_span,
        };
        ast::GenericArgs::Parenthesized(ast::ParenthesizedArgs {
            span: ident.span.to(hi),
            inputs,
            inputs_span,
            output,
        })
    }

    fn paren_args_from_params(
        &self,
        ident: Ident,
        params: ra::ParamList<'_>,
        ret: Option<ra::RetType<'_>>,
    ) -> ast::GenericArgs {
        let inputs_span = match (params.l_paren_token(), params.r_paren_token()) {
            (Some(l), Some(r)) => self.tsp(l).to(self.tsp(r)),
            _ => self.nsp(params.syntax()),
        };
        let inputs = params.params().map(|p| self.param(p)).collect();
        let output = self.ret_ty(ret, inputs_span);
        let hi = match &output {
            ast::FnRetTy::Ty(ty) => ty.span,
            ast::FnRetTy::Default(_) => inputs_span,
        };
        ast::GenericArgs::Parenthesized(ast::ParenthesizedArgs {
            span: ident.span.to(hi),
            inputs,
            inputs_span,
            output,
        })
    }

    /// `-> T`, or rustc's `FnRetTy::Default(self.prev_token.span.shrink_to_hi())`: empty, right
    /// after the parameters.
    pub(crate) fn ret_ty(&self, ret: Option<ra::RetType<'_>>, params_span: Span) -> ast::FnRetTy {
        match ret.and_then(|r| r.ty()) {
            Some(ty) => ast::FnRetTy::Ty(self.ty(ty)),
            None => {
                if let Some(ret) = ret {
                    self.unsupported(
                        self.nsp(ret.syntax()),
                        "a return type rust-analyzer did not parse",
                    );
                }
                ast::FnRetTy::Default(params_span.shrink_to_hi())
            }
        }
    }

    pub(crate) fn anon_const(&self, expr: ra::Expr<'_>) -> ast::AnonConst {
        ast::AnonConst {
            id: DUMMY_NODE_ID,
            value: self.expr(expr),
        }
    }

    // ---- visibility ----

    /// rustc's `parse_visibility`: `pub` alone, `pub(..)` through the parenthesis, or an empty
    /// span at the item's first token when there is none.
    pub(crate) fn visibility(
        &self,
        vis: Option<ra::Visibility<'_>>,
        owner: ra::Node<'_>,
    ) -> ast::Visibility {
        let Some(vis) = vis else {
            let at = first_token_after_attrs(owner)
                .map(|t| self.tsp(t).shrink_to_lo())
                .unwrap_or_else(|| self.sp(TextRange::empty(owner.text_range().start())));
            return ast::Visibility {
                kind: ast::VisibilityKind::Inherited,
                span: at,
            };
        };
        let span = self.nsp(vis.syntax());
        let kind = match vis.visibility_inner() {
            None => ast::VisibilityKind::Public,
            Some(inner) => match inner.path() {
                Some(path) => {
                    let shorthand = inner.in_token().is_none();
                    let (_, path) = self.path(path);
                    ast::VisibilityKind::Restricted {
                        path: Box::new(path),
                        id: DUMMY_NODE_ID,
                        shorthand,
                    }
                }
                None => {
                    self.unsupported(span, "a visibility rust-analyzer did not parse");
                    ast::VisibilityKind::Public
                }
            },
        };
        ast::Visibility { kind, span }
    }

    // ---- generics ----

    /// rustc's `parse_generics`: the span of `<..>`, or empty right after the name.
    pub(crate) fn generics(
        &self,
        list: Option<ra::GenericParamList<'_>>,
        where_clause: Option<ra::WhereClause<'_>>,
        after: Span,
        where_after: Span,
    ) -> ast::Generics {
        let (params, span) = match list {
            Some(list) => {
                let params = list
                    .generic_params()
                    .map(|p| self.generic_param(p))
                    .collect();
                (params, self.nsp(list.syntax()))
            }
            None => (ThinVec::new(), after.shrink_to_hi()),
        };
        ast::Generics {
            params,
            where_clause: self.where_clause(where_clause, where_after),
            span,
        }
    }

    /// rustc's `parse_where_clause`: `where` through the last predicate (or its trailing comma),
    /// or empty right after what precedes it.
    pub(crate) fn where_clause(
        &self,
        wc: Option<ra::WhereClause<'_>>,
        after: Span,
    ) -> ast::WhereClause {
        match wc {
            None => ast::WhereClause {
                has_where_token: false,
                predicates: ThinVec::new(),
                span: after.shrink_to_hi(),
            },
            Some(wc) => ast::WhereClause {
                has_where_token: true,
                predicates: wc.predicates().map(|p| self.where_pred(p)).collect(),
                span: self.nsp(wc.syntax()),
            },
        }
    }

    fn where_pred(&self, p: ra::WherePred<'_>) -> ast::WherePredicate {
        let span = self.nsp(p.syntax());
        let attrs = self.outer_attrs(p.syntax());
        let bounds = p
            .type_bound_list()
            .map(|b| self.bounds(b))
            .unwrap_or_default();
        let kind = if let Some(lt) = p.lifetime() {
            ast::WherePredicateKind::RegionPredicate(ast::WhereRegionPredicate {
                lifetime: self.lifetime(lt),
                bounds,
            })
        } else {
            let bounded_ty = match p.ty() {
                Some(ty) => self.ty(ty),
                None => {
                    self.unsupported(span, "a where predicate without a type");
                    self.ty_err(span)
                }
            };
            let bound_generic_params = p
                .for_binder()
                .and_then(|b| b.generic_param_list())
                .map(|l| l.generic_params().map(|g| self.generic_param(g)).collect())
                .unwrap_or_default();
            ast::WherePredicateKind::BoundPredicate(ast::WhereBoundPredicate {
                bound_generic_params,
                bounded_ty,
                bounds,
            })
        };
        ast::WherePredicate {
            attrs,
            kind,
            id: DUMMY_NODE_ID,
            span,
            is_placeholder: false,
        }
    }

    pub(crate) fn generic_param(&self, p: ra::GenericParam<'_>) -> ast::GenericParam {
        let attrs = self.outer_attrs(p.syntax());
        match p {
            ra::GenericParam::LifetimeParam(lp) => {
                let lifetime = match lp.lifetime() {
                    Some(lt) => self.lifetime(lt),
                    None => {
                        let span = self.nsp(lp.syntax());
                        self.unsupported(span, "a lifetime parameter without a name");
                        ast::Lifetime {
                            id: DUMMY_NODE_ID,
                            ident: Ident::new(sym::empty, span),
                        }
                    }
                };
                let colon_span = lp.colon_token().map(|t| self.tsp(t));
                let bounds = lp
                    .type_bound_list()
                    .map(|b| self.bounds(b))
                    .unwrap_or_default();
                ast::GenericParam {
                    id: DUMMY_NODE_ID,
                    ident: lifetime.ident,
                    attrs,
                    bounds,
                    is_placeholder: false,
                    kind: ast::GenericParamKind::Lifetime,
                    colon_span,
                }
            }
            ra::GenericParam::TypeParam(tp) => {
                let span = self.nsp(tp.syntax());
                let ident = match tp.name() {
                    Some(n) => self.name(n),
                    None => {
                        self.unsupported(span, "a type parameter without a name");
                        Ident::new(sym::empty, span)
                    }
                };
                let colon_span = tp.colon_token().map(|t| self.tsp(t));
                let bounds = tp
                    .type_bound_list()
                    .map(|b| self.bounds(b))
                    .unwrap_or_default();
                let default = tp.default_type().map(|t| self.ty(t));
                ast::GenericParam {
                    id: DUMMY_NODE_ID,
                    ident,
                    attrs,
                    bounds,
                    is_placeholder: false,
                    kind: ast::GenericParamKind::Type { default },
                    colon_span,
                }
            }
            ra::GenericParam::ConstParam(cp) => {
                let span_all = self.nsp(cp.syntax());
                let ident = match cp.name() {
                    Some(n) => self.name(n),
                    None => {
                        self.unsupported(span_all, "a const parameter without a name");
                        Ident::new(sym::empty, span_all)
                    }
                };
                let ty = match cp.ty() {
                    Some(t) => self.ty(t),
                    None => {
                        self.unsupported(span_all, "a const parameter without a type");
                        self.ty_err(span_all)
                    }
                };
                let default = cp
                    .default_val()
                    .and_then(|c| c.expr())
                    .map(|e| self.anon_const(e));
                // rustc's `parse_const_param`: `const` through the type, or the default.
                let span = span_all;
                ast::GenericParam {
                    id: DUMMY_NODE_ID,
                    ident,
                    attrs,
                    bounds: ThinVec::new(),
                    is_placeholder: false,
                    kind: ast::GenericParamKind::Const { ty, span, default },
                    colon_span: None,
                }
            }
        }
    }

    // ---- function parameters ----

    pub(crate) fn fn_decl(
        &self,
        params: Option<ra::ParamList<'_>>,
        ret: Option<ra::RetType<'_>>,
        owner: Span,
    ) -> ast::FnDecl {
        let Some(params) = params else {
            self.unsupported(
                owner,
                "a function without parameters rust-analyzer could parse",
            );
            return ast::FnDecl {
                inputs: ThinVec::new(),
                output: ast::FnRetTy::Default(owner),
            };
        };
        let mut inputs = ThinVec::new();
        if let Some(sp) = params.self_param() {
            inputs.push(self.self_param(sp));
        }
        for p in params.params() {
            inputs.push(self.param(p));
        }
        let params_span = self.nsp(params.syntax());
        let output = self.ret_ty(ret, params_span);
        ast::FnDecl { inputs, output }
    }

    /// rustc's `parse_param_general`: the span starts before the parameter's attributes.
    pub(crate) fn param(&self, p: ra::Param<'_>) -> ast::Param {
        let span = self.nsp_with_attrs(p.syntax());
        let attrs = self.outer_attrs(p.syntax());
        if p.dotdotdot_token().is_some() && p.ty().is_none() {
            let dots = p.dotdotdot_token().map(|t| self.tsp(t)).unwrap_or(span);
            let (pat, ty_span) = match p.pat() {
                Some(pat) => (self.pat(pat), dots),
                None => (
                    Box::new(ast::Pat {
                        id: DUMMY_NODE_ID,
                        kind: ast::PatKind::Missing,
                        span: dots,
                    }),
                    dots,
                ),
            };
            let ty = Box::new(ast::Ty {
                id: DUMMY_NODE_ID,
                kind: ast::TyKind::CVarArgs,
                span: ty_span,
            });
            return ast::Param {
                attrs,
                ty,
                pat,
                id: DUMMY_NODE_ID,
                span,
                is_placeholder: false,
                has_const_keyword: false,
            };
        }
        let ty = match p.ty() {
            Some(ty) => self.ty(ty),
            None => {
                self.unsupported(span, "a parameter without a type");
                self.ty_err(span)
            }
        };
        let pat = match p.pat() {
            Some(pat) => self.pat(pat),
            None => Box::new(ast::Pat {
                id: DUMMY_NODE_ID,
                kind: ast::PatKind::Missing,
                span: ty.span,
            }),
        };
        let has_const_keyword = p
            .syntax()
            .children_with_tokens()
            .any(|e| e.kind() == T![const]);
        ast::Param {
            attrs,
            ty,
            pat,
            id: DUMMY_NODE_ID,
            span,
            is_placeholder: false,
            has_const_keyword,
        }
    }

    /// rustc's `parse_self_param` and `Param::from_self`: the span runs from the first token
    /// after the attributes to `self`, leaving out a `: Type`.
    fn self_param(&self, sp: ra::SelfParam<'_>) -> ast::Param {
        let attrs = self.outer_attrs(sp.syntax());
        let all = self.nsp(sp.syntax());
        let self_tok = sp.name().and_then(|n| n.syntax().first_token());
        let eself_ident = match self_tok {
            Some(t) => Ident::new(kw::SelfLower, self.tsp(t)),
            None => {
                self.unsupported(all, "a self parameter without `self`");
                Ident::new(kw::SelfLower, all)
            }
        };
        let mutbl = if sp.mut_token().is_some() {
            ast::Mutability::Mut
        } else {
            ast::Mutability::Not
        };
        let kind = if sp.amp_token().is_some() {
            let lt = sp.lifetime().map(|l| self.lifetime(l));
            ast::SelfKind::Region(lt, mutbl)
        } else if let Some(ty) = sp.ty() {
            ast::SelfKind::Explicit(self.ty(ty), mutbl)
        } else {
            ast::SelfKind::Value(mutbl)
        };
        let lo = first_token_after_attrs(sp.syntax())
            .map(|t| self.tsp(t))
            .unwrap_or(all);
        let eself = ast::ExplicitSelf {
            node: kind,
            span: lo.to(eself_ident.span),
        };
        ast::Param::from_self(attrs, eself, eself_ident)
    }

    // ---- bounds ----

    pub(crate) fn bounds(&self, list: ra::TypeBoundList<'_>) -> ast::GenericBounds {
        list.bounds().map(|b| self.bound(b)).collect()
    }

    fn bound(&self, b: ra::TypeBound<'_>) -> ast::GenericBound {
        let span = self.nsp(b.syntax());
        if let Some(lt) = b.lifetime() {
            return ast::GenericBound::Outlives(self.lifetime(lt));
        }
        if let Some(args) = b.use_bound_generic_args() {
            let args = args
                .use_bound_generic_args()
                .map(|a| match a {
                    ra::UseBoundGenericArg::Lifetime(lt) => {
                        ast::PreciseCapturingArg::Lifetime(self.lifetime(lt))
                    }
                    ra::UseBoundGenericArg::NameRef(n) => {
                        let ident = self.name_ref(n);
                        ast::PreciseCapturingArg::Arg(ast::Path::from_ident(ident), DUMMY_NODE_ID)
                    }
                })
                .collect();
            return ast::GenericBound::Use(args, span);
        }
        let parens = if b.syntax().child_token(T!['(']).is_some() {
            ast::Parens::Yes
        } else {
            ast::Parens::No
        };
        let bound_generic_params = b
            .for_binder()
            .and_then(|f| f.generic_param_list())
            .map(|l| l.generic_params().map(|g| self.generic_param(g)).collect())
            .unwrap_or_default();
        let constness = if let Some(t) = b.tilde_token() {
            let c = b
                .const_token()
                .map(|c| self.tsp(c))
                .unwrap_or(self.tsp(t));
            ast::BoundConstness::Maybe(self.tsp(t).to(c))
        } else if let Some(l) = b.l_brack_token() {
            let r = b
                .r_brack_token()
                .map(|r| self.tsp(r))
                .unwrap_or(self.tsp(l));
            ast::BoundConstness::Maybe(self.tsp(l).to(r))
        } else if let Some(c) = b.const_token() {
            ast::BoundConstness::Always(self.tsp(c))
        } else {
            ast::BoundConstness::Never
        };
        let asyncness = match b.async_token() {
            Some(t) => ast::BoundAsyncness::Async(self.tsp(t)),
            None => ast::BoundAsyncness::Normal,
        };
        let polarity = if let Some(q) = b.question_mark_token() {
            ast::BoundPolarity::Maybe(self.tsp(q))
        } else {
            ast::BoundPolarity::Positive
        };
        let path = match b.ty() {
            Some(ra::Type::PathType(pt)) => match pt.path() {
                Some(p) => self.path(p).1,
                None => self.err_path(span),
            },
            Some(other) => {
                self.unsupported(self.nsp(other.syntax()), "a bound that is not a trait path");
                self.err_path(span)
            }
            None => self.err_path(span),
        };
        ast::GenericBound::Trait(ast::PolyTraitRef {
            bound_generic_params,
            modifiers: ast::TraitBoundModifiers {
                constness,
                asyncness,
                polarity,
            },
            trait_ref: ast::TraitRef {
                path,
                ref_id: DUMMY_NODE_ID,
            },
            span,
            parens,
        })
    }

    pub(crate) fn err_path(&self, span: Span) -> ast::Path {
        self.unsupported(span, "a path rust-analyzer did not parse");
        ast::Path {
            span,
            segments: ThinVec::new(),
        }
    }

    pub(crate) fn ty_err(&self, span: Span) -> Box<ast::Ty> {
        Box::new(ast::Ty {
            id: DUMMY_NODE_ID,
            kind: ast::TyKind::Err(ast::ErrorGuaranteed::unchecked_error_guaranteed()),
            span,
        })
    }

    // ---- macro calls ----

    pub(crate) fn mac_call(&self, mc: ra::MacroCall<'_>) -> Box<ast::MacCall> {
        let span = self.nsp(mc.syntax());
        let path = match mc.path() {
            Some(p) => self.path(p).1,
            None => self.err_path(span),
        };
        let args = match mc.token_tree() {
            Some(tt) => Box::new(self.delim_args(tt)),
            None => {
                self.unsupported(span, "a macro call without arguments");
                Box::new(ast::DelimArgs {
                    dspan: DelimSpan::from_single(span),
                    delim: Delimiter::Parenthesis,
                    tokens: TokenStream::default(),
                })
            }
        };
        Box::new(ast::MacCall { path, args })
    }
}

// ---- token helpers ----

pub(crate) fn is_trivia(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
}

fn is_attr_node(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT)
}

pub(crate) fn first_token(node: ra::Node<'_>) -> Option<ra::Token<'_>> {
    node.descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|t| !is_trivia(t.kind()))
}

pub(crate) fn last_token(node: ra::Node<'_>) -> Option<ra::Token<'_>> {
    let mut token = node.last_token()?;
    let start = node.text_range().start();
    while is_trivia(token.kind()) {
        token = token.prev_token()?;
        if token.text_range().start() < start {
            return None;
        }
    }
    Some(token)
}

/// The first token of `node` that is not trivia and not inside an attribute leading it, or
/// leading its first child: rust-analyzer may hang a statement's attributes on the innermost
/// expression of a method chain, where rustc's span leaves them out.
pub(crate) fn first_token_after_attrs(node: ra::Node<'_>) -> Option<ra::Token<'_>> {
    for child in node.children_with_tokens() {
        match child {
            NodeOrToken::Token(t) if is_trivia(t.kind()) => continue,
            NodeOrToken::Token(t) => return Some(t),
            NodeOrToken::Node(n) if is_attr_node(n.kind()) => continue,
            NodeOrToken::Node(n) => {
                if let Some(t) = first_token_after_attrs(n) {
                    return Some(t);
                }
            }
        }
    }
    None
}

pub(crate) fn next_real_token(token: ra::Token<'_>) -> Option<ra::Token<'_>> {
    let mut t = token.next_token()?;
    while is_trivia(t.kind()) {
        t = t.next_token()?;
    }
    Some(t)
}

pub(crate) fn prev_real_token(token: ra::Token<'_>) -> Option<ra::Token<'_>> {
    let mut t = token.prev_token()?;
    while is_trivia(t.kind()) {
        t = t.prev_token()?;
    }
    Some(t)
}
