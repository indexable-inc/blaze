//! The part of rustc's `Parser` (`compiler/rustc_parse/src/parser/mod.rs` at rust-lang/rust
//! 12433c307, MIT or Apache-2.0) rustfmt drives over a macro's arguments: a cursor over rustc's
//! tokens with `token`, `prev_token`, `bump`, `eat` and `look_ahead`, and `parse_expr`,
//! `parse_ty`, `parse_pat_no_top_alt` and `parse_item`, which parse the fragment starting at the
//! current token with rust-analyzer's parser and lower it with rustc's spans. A fragment
//! rust-analyzer parses with an error, or whose end falls inside one of rustc's tokens (half of
//! a `>>`), is an error, as a fragment rustc cannot parse is.

use std::rc::Rc;

use parser::{PrefixEntryPoint, Step, TopEntryPoint};
use syntax::view::{self as ra, AstNode};
use syntax::{LexedStr, Parse, SyntaxTreeBuilder};

use crate::ast::token::{NonterminalKind, NtExprKind, Token, TokenKind};
use crate::ast::tokenstream::{TokenStream, TokenTree};
use crate::ast::{self, ThinVec};
use crate::parse::lower::{DelimIndex, Lower};
use crate::parse::session::ParseSess;
use crate::span::{BytePos, Ident, Span, kw};

/// What `eat`, `check` and `expect` look for: rustc's `ExpTokenPair` and `ExpKeywordPair`.
#[derive(Clone, Copy)]
pub(crate) enum Exp {
    Tok(TokenKind),
    Kw(crate::span::Symbol),
}

/// rustc's `exp!`: the expected token or keyword of that name.
macro_rules! exp {
    (If) => {
        $crate::parse::stream::Exp::Kw($crate::span::kw::If)
    };
    (Else) => {
        $crate::parse::stream::Exp::Kw($crate::span::kw::Else)
    };
    (Static) => {
        $crate::parse::stream::Exp::Kw($crate::span::kw::Static)
    };
    (Ref) => {
        $crate::parse::stream::Exp::Kw($crate::span::kw::Ref)
    };
    (Unsafe) => {
        $crate::parse::stream::Exp::Kw($crate::span::kw::Unsafe)
    };
    (Underscore) => {
        $crate::parse::stream::Exp::Kw($crate::span::kw::Underscore)
    };
    ($tok:ident) => {
        $crate::parse::stream::Exp::Tok($crate::ast::token::TokenKind::$tok)
    };
}
pub(crate) use exp;

/// A macro argument that did not parse as what rustfmt tried; rustfmt cancels it and tries the
/// next kind of argument.
#[derive(Debug)]
pub(crate) struct PErr {
    pub(crate) span: Span,
    pub(crate) message: &'static str,
}

impl PErr {
    pub(crate) fn cancel(self) {
        tracing::trace!(span = ?self.span, "macro argument: {}", self.message);
    }
}

pub(crate) type PResult<T> = Result<T, PErr>;

/// rustc's `ForceCollect`, `AllowConstBlockItems`, `FollowedByType` and `InnerAttrPolicy`: the
/// arguments rustfmt passes, which change nothing here.
#[derive(Clone, Copy)]
pub(crate) enum ForceCollect {
    No,
}
#[derive(Clone, Copy)]
pub(crate) enum AllowConstBlockItems {
    Yes,
    DoesNotMatter,
}
#[derive(Clone, Copy)]
pub(crate) enum FollowedByType {
    No,
}
#[derive(Clone, Copy)]
pub(crate) enum InnerAttrPolicy {
    Permitted,
}

/// The error count rustfmt checks after a fragment parse; this parser reports failure through
/// `Err`, so there is never one to see.
pub(crate) struct Dcx;

impl Dcx {
    pub(crate) fn has_errors(&self) -> Option<()> {
        None
    }

    pub(crate) fn reset_err_count(&self) {}
}

#[derive(Clone)]
pub(crate) struct Parser<'a> {
    pub(crate) psess: &'a ParseSess,
    tokens: Rc<[Token]>,
    delims: Rc<DelimIndex>,
    /// The index of `token` in `tokens`; `tokens.len()` at the end.
    pos: usize,
    pub(crate) token: Token,
    pub(crate) prev_token: Token,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(psess: &'a ParseSess, stream: TokenStream) -> Parser<'a> {
        let mut tokens = Vec::new();
        flatten(&stream, &mut tokens);
        let delims = Rc::new(DelimIndex::new(&stream));
        let dummy = Token::new(TokenKind::Eof, Span::default());
        let mut parser = Parser {
            psess,
            tokens: tokens.into(),
            delims,
            pos: 0,
            token: dummy,
            prev_token: dummy,
        };
        parser.token = parser.token_at(0, dummy.span);
        parser
    }

    /// The token at `i`, or rustc's end-of-stream token, which takes the span of the token
    /// before it.
    fn token_at(&self, i: usize, fallback: Span) -> Token {
        match self.tokens.get(i) {
            Some(t) => *t,
            None => Token::new(
                TokenKind::Eof,
                self.tokens.last().map_or(fallback, |t| t.span),
            ),
        }
    }

    pub(crate) fn dcx(&self) -> Dcx {
        Dcx
    }

    pub(crate) fn bump(&mut self) {
        self.prev_token = self.token;
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        self.token = self.token_at(self.pos, self.prev_token.span);
    }

    pub(crate) fn look_ahead<R>(&self, dist: usize, looker: impl FnOnce(&Token) -> R) -> R {
        looker(&self.token_at(self.pos + dist, self.token.span))
    }

    pub(crate) fn check(&self, exp: Exp) -> bool {
        match exp {
            Exp::Tok(kind) => self.token.kind == kind,
            Exp::Kw(kw) => self.token.is_keyword(kw),
        }
    }

    pub(crate) fn eat(&mut self, exp: Exp) -> bool {
        let found = self.check(exp);
        if found {
            self.bump();
        }
        found
    }

    pub(crate) fn eat_keyword(&mut self, exp: Exp) -> bool {
        self.eat(exp)
    }

    pub(crate) fn expect(&mut self, exp: Exp) -> PResult<()> {
        if self.eat(exp) {
            Ok(())
        } else {
            Err(self.err("expected a different token"))
        }
    }

    fn err(&self, message: &'static str) -> PErr {
        PErr {
            span: self.token.span,
            message,
        }
    }

    pub(crate) fn nonterminal_may_begin_with(kind: NonterminalKind, token: &Token) -> bool {
        match kind {
            NonterminalKind::Expr(NtExprKind::Expr2021 { .. }) => {
                token.can_begin_expr() && !token.is_keyword(kw::Let) && !token.is_keyword(kw::Const)
            }
            NonterminalKind::Expr(NtExprKind::Expr) => {
                (token.can_begin_expr() || token.is_keyword(kw::Underscore))
                    && !token.is_keyword(kw::Let)
            }
            NonterminalKind::Ty => token.can_begin_type(),
            NonterminalKind::Pat(pat_kind) => token.can_begin_pattern(pat_kind),
            NonterminalKind::Item | NonterminalKind::Stmt | NonterminalKind::TT => !matches!(
                token.kind,
                TokenKind::CloseParen | TokenKind::CloseBrace | TokenKind::CloseBracket
            ),
            _ => false,
        }
    }

    // ---- fragments ----

    pub(crate) fn parse_expr(&mut self) -> PResult<Box<ast::Expr>> {
        self.fragment(
            PrefixEntryPoint::Expr,
            TopEntryPoint::Expr,
            |lower, root| ra::Expr::cast(root).map(|e| lower.expr(e)),
        )
    }

    pub(crate) fn parse_ty(&mut self) -> PResult<Box<ast::Ty>> {
        self.fragment(PrefixEntryPoint::Ty, TopEntryPoint::Type, |lower, root| {
            ra::Type::cast(root).map(|t| lower.ty(t))
        })
    }

    pub(crate) fn parse_pat_no_top_alt(
        &mut self,
        _expected: Option<()>,
        _loc: Option<()>,
    ) -> PResult<ast::Pat> {
        self.fragment(
            PrefixEntryPoint::Pat,
            TopEntryPoint::Pattern,
            |lower, root| ra::Pat::cast(root).map(|p| lower.pat_val(p)),
        )
    }

    pub(crate) fn parse_item(
        &mut self,
        _: ForceCollect,
        _: AllowConstBlockItems,
    ) -> PResult<Option<Box<ast::Item>>> {
        if self.token.kind == TokenKind::Eof {
            return Ok(None);
        }
        self.fragment(
            PrefixEntryPoint::Item,
            TopEntryPoint::MacroItems,
            |lower, root| {
                let mut items = root.children().filter_map(ra::Item::cast);
                match (items.next(), items.next()) {
                    (Some(item), None) => Some(lower.item(item)),
                    _ => None,
                }
            },
        )
    }

    /// Parses the fragment starting at the current token with `prefix`, reparses exactly its
    /// text with `top` and lowers that tree; the cursor then moves past the fragment.
    fn fragment<T>(
        &mut self,
        prefix: PrefixEntryPoint,
        top: TopEntryPoint,
        lower: impl FnOnce(&Lower<'_>, ra::Node<'_>) -> Option<T>,
    ) -> PResult<T> {
        if self.token.kind == TokenKind::Eof {
            return Err(self.err("expected a fragment, found the end"));
        }
        let lo = self.token.span.lo();
        let end = self.tokens.last().map_or(lo, |t| t.span.hi());
        let span = Span::with_root_ctxt(lo, end);
        let text = self
            .psess
            .source_map()
            .span_to_snippet(span)
            .map_err(|_| self.err("a macro argument outside its file"))?;
        let edition = self.psess.edition();
        let ra_edition = edition.to_ra();

        let lexed = LexedStr::new(ra_edition, &text);
        let input = lexed.to_input(ra_edition);
        let output = prefix.parse(&input);
        let mut consumed = 0usize;
        for step in output.iter() {
            match step {
                Step::Token { n_input_tokens, .. } => consumed += usize::from(n_input_tokens),
                Step::FloatSplit { .. } => consumed += 1,
                Step::Error { .. } => {
                    return Err(self.err("a fragment rust-analyzer could not parse"));
                }
                Step::Enter { .. } | Step::Exit => {}
            }
        }
        if consumed == 0 {
            return Err(self.err("an empty fragment"));
        }
        let Some(last) = (0..lexed.len())
            .filter(|&i| !lexed.kind(i).is_trivia())
            .nth(consumed - 1)
        else {
            return Err(self.err("a fragment past the end"));
        };
        if lexed.errors().any(|(i, _)| i <= last) {
            return Err(self.err("a fragment that does not lex"));
        }
        let frag_end = lexed.text_range(last).end;
        let end_pos = lo + BytePos(frag_end as u32);
        let Some(k) = self.tokens[self.pos..]
            .iter()
            .position(|t| t.span.hi() == end_pos)
        else {
            return Err(self.err("a fragment ending inside a token"));
        };
        let k = self.pos + k;

        let parse = parse_ra(&text[..frag_end], ra_edition, top)
            .map_err(|_| self.err("a fragment rust-analyzer could not reparse"))?;
        let root = parse.root();
        if root.kind() == syntax::SyntaxKind::ERROR {
            return Err(self.err("a fragment rust-analyzer could not reparse"));
        }
        let lowering = Lower::new(lo, edition, &self.delims);
        let value = lower(&lowering, root);
        if let Err(unsupported) = lowering.finish() {
            let _ = unsupported;
            return Err(self.err("a fragment blaze fmt cannot lower yet"));
        }
        let value = value.ok_or_else(|| self.err("a fragment of another kind"))?;

        self.prev_token = self.tokens[k];
        self.pos = k + 1;
        self.token = self.token_at(self.pos, self.prev_token.span);
        Ok(value)
    }

    // ---- by hand ----

    pub(crate) fn parse_ident(&mut self) -> PResult<Ident> {
        match self.token.ident() {
            Some((ident, _)) if !self.token.is_reserved_ident() => {
                self.bump();
                Ok(ident)
            }
            _ => Err(self.err("expected an identifier")),
        }
    }

    /// rustc's `parse_visibility`.
    pub(crate) fn parse_visibility(&mut self, _: FollowedByType) -> PResult<ast::Visibility> {
        if !self.token.is_keyword(kw::Pub) {
            return Ok(ast::Visibility {
                kind: ast::VisibilityKind::Inherited,
                span: self.token.span.shrink_to_lo(),
            });
        }
        self.bump();
        let lo = self.prev_token.span;
        if self.token.kind == TokenKind::OpenParen {
            let in_path = self.look_ahead(1, |t| t.is_keyword(kw::In));
            let short = self.look_ahead(2, |t| t.kind == TokenKind::CloseParen)
                && self.look_ahead(1, |t| {
                    t.is_keyword(kw::Crate)
                        || t.is_keyword(kw::Super)
                        || t.is_keyword(kw::SelfLower)
                });
            if in_path || short {
                self.bump();
                if in_path {
                    self.bump();
                }
                let path = self.parse_mod_path()?;
                self.expect(exp!(CloseParen))?;
                let kind = ast::VisibilityKind::Restricted {
                    path: Box::new(path),
                    id: ast::DUMMY_NODE_ID,
                    shorthand: !in_path,
                };
                return Ok(ast::Visibility {
                    kind,
                    span: lo.to(self.prev_token.span),
                });
            }
        }
        Ok(ast::Visibility {
            kind: ast::VisibilityKind::Public,
            span: lo,
        })
    }

    /// A path of plain segments, as in attributes and `pub(in ..)`.
    fn parse_mod_path(&mut self) -> PResult<ast::Path> {
        let lo = self.token.span;
        let mut segments = ThinVec::new();
        if self.eat(exp!(PathSep)) {
            segments.push(ast::PathSegment::path_root(lo.shrink_to_lo()));
        }
        loop {
            match self.token.ident() {
                Some((ident, _)) => {
                    self.bump();
                    segments.push(ast::PathSegment::from_ident(ident));
                }
                None => return Err(self.err("expected a path segment")),
            }
            if !self.eat(exp!(PathSep)) {
                break;
            }
        }
        Ok(ast::Path {
            span: lo.to(self.prev_token.span),
            segments,
        })
    }

    /// Steps over `#[..]` or `#![..]`; rustfmt only needs to get past it.
    pub(crate) fn parse_attribute(&mut self, _: InnerAttrPolicy) -> PResult<()> {
        if !self.eat(exp!(Pound)) {
            return Err(self.err("expected `#`"));
        }
        let _ = self.eat(exp!(Bang));
        if self.token.kind != TokenKind::OpenBracket {
            return Err(self.err("expected `[`"));
        }
        self.skip_delimited();
        Ok(())
    }

    /// Steps over a `cfg` predicate: a path and its arguments, delimited or `= expr`.
    pub(crate) fn parse_attr_item(&mut self, _: ForceCollect) -> PResult<()> {
        let _ = self.eat(exp!(Unsafe));
        self.parse_mod_path()?;
        if matches!(
            self.token.kind,
            TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace
        ) {
            self.skip_delimited();
        } else if self.eat(exp!(Eq)) {
            self.parse_expr()?;
        }
        Ok(())
    }

    /// The current token is an opening delimiter: moves past its closing one.
    fn skip_delimited(&mut self) {
        let mut depth = 0usize;
        loop {
            match self.token.kind {
                TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace => depth += 1,
                TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace => {
                    depth = depth.saturating_sub(1)
                }
                TokenKind::Eof => return,
                _ => {}
            }
            self.bump();
            if depth == 0 {
                return;
            }
        }
    }

    /// rustc's `parse_meta_item_inner`: a literal, or a path with `= lit` or `(..)` after it.
    pub(crate) fn parse_meta_item_inner(&mut self) -> PResult<ast::MetaItemInner> {
        if let TokenKind::Literal(lit) = self.token.kind {
            let span = self.token.span;
            self.bump();
            return ast::MetaItemLit::from_token_lit(lit, span)
                .map(ast::MetaItemInner::Lit)
                .map_err(|_| self.err("a literal that does not cook"));
        }
        let lo = self.token.span;
        let path = self.parse_mod_path()?;
        let kind = if self.eat(exp!(Eq)) {
            match self.token.kind {
                TokenKind::Literal(lit) => {
                    let span = self.token.span;
                    self.bump();
                    ast::MetaItemKind::NameValue(
                        ast::MetaItemLit::from_token_lit(lit, span)
                            .map_err(|_| self.err("a literal that does not cook"))?,
                    )
                }
                _ => return Err(self.err("expected a literal")),
            }
        } else if self.eat(exp!(OpenParen)) {
            let mut items = ThinVec::new();
            while self.token.kind != TokenKind::CloseParen {
                items.push(self.parse_meta_item_inner()?);
                if !self.eat(exp!(Comma)) {
                    break;
                }
            }
            self.expect(exp!(CloseParen))?;
            ast::MetaItemKind::List(items)
        } else {
            ast::MetaItemKind::Word
        };
        Ok(ast::MetaItemInner::MetaItem(ast::MetaItem {
            unsafety: ast::Safety::Default,
            path,
            kind,
            span: lo.to(self.prev_token.span),
        }))
    }
}

/// The stream's tokens in order, each delimited group as its opening token, contents and
/// closing token.
fn flatten(stream: &TokenStream, out: &mut Vec<Token>) {
    for tt in stream.iter() {
        match tt {
            TokenTree::Token(t, _) => out.push(*t),
            TokenTree::Delimited(dspan, _, delim, inner) => {
                out.push(Token::new(delim.as_open_token_kind(), dspan.open));
                flatten(inner, out);
                out.push(Token::new(delim.as_close_token_kind(), dspan.close));
            }
        }
    }
}

/// rust-analyzer's tree of `text` parsed from `entry`, to be walked through [`Parse::root`], or
/// the first error its lexer or parser reports, with its offset in `text`. Its validation pass is
/// not run: what that flags (`self` inside a path, inner attributes on a block in expression
/// position) rustc's parser accepts.
pub(crate) fn parse_ra(
    text: &str,
    edition: syntax::Edition,
    entry: TopEntryPoint,
) -> Result<Parse<ra::Untyped>, (usize, String)> {
    let lexed = LexedStr::new(edition, text);
    if let Some((i, msg)) = lexed.errors().next() {
        return Err((lexed.text_range(i).start, msg.to_owned())); // alloc: the one error, reported
    }
    let output = entry.parse(&lexed.to_input(edition));
    let mut builder = SyntaxTreeBuilder::default();
    let mut error = None;
    lexed.intersperse_trivia(&output, &mut |step| match step {
        parser::StrStep::Token { kind, text } => builder.token(kind, text),
        parser::StrStep::Enter { kind } => builder.start_node(kind),
        parser::StrStep::Exit => builder.finish_node(),
        parser::StrStep::Error { msg, pos } => {
            error.get_or_insert_with(|| (pos, msg.to_owned())); // alloc: the first error, reported
        }
    });
    match error {
        Some(error) => Err(error),
        None => Ok(builder.finish_view()),
    }
}
