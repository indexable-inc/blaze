//! Recursive-descent + precedence-climbing parser for `blaze-sh`.
//!
//! Pure, sans-IO, `#![no_std]`, `#![forbid(unsafe_code)]`. Every allocation goes into the
//! caller's [`Alloc`], and every loop and recursion is explicitly bounded.

use blaze_own::{ARc, AString, AVec, Alloc, Box, Rc, Share};

use crate::ast::{
    AssignOp, AssignTarget, BinaryOp, Block, CallArg, CallExpr, Expr, ExprKind, InterpPart,
    MatchArm, ParamDef, ParamKind, PathKey, PathStep, Pattern, Stmt, TypeAnn, UnaryOp,
};
use crate::error::{ErrorKind, ShellError};
use crate::lex::{Keyword, LexInterpPart, TokKind, Token, tokenize};
use crate::span::Span;

/// Maximum recursion depth for nested expressions, blocks, calls, and patterns.
pub const MAX_PARSE_DEPTH: usize = 128;

const MAX_STMTS: usize = 65_536;
const MAX_PIPELINE_STAGES: usize = 1_024;
const MAX_CALL_ARGS: usize = 4_096;
const MAX_CONTAINER_ITEMS: usize = 65_536;
const MAX_PATH_STEPS: usize = 256;
const MAX_MATCH_ARMS: usize = 4_096;
const MAX_PARAMS: usize = 256;

/// Lex and parse a complete `blaze-sh` source string into a [`Block`].
pub fn parse<'a>(src: &str, alloc: Alloc<'a>) -> Result<Block<'a>, ShellError<'a>> {
    let tokens = tokenize(src, alloc)?;
    let mut parser = Parser::new(src, tokens.as_slice(), alloc, 0);
    parser.parse_block(false)
}

#[derive(Clone, Copy)]
struct ParseFlags {
    /// Inside `where <predicate>`: bare identifiers parse as `ExprKind::BarePath`.
    bare_path_mode: bool,
    /// Inside `if`/`while`/`for`/`match` headers: stop command arguments before `{`.
    stop_at_lbrace: bool,
    /// Inside a `match` arm body: stop command arguments before `,`.
    stop_at_comma: bool,
    /// Inside a list literal `[a b c]` (outside `where`): bare identifiers parse as strings.
    list_bare_words: bool,
    /// Inside a comma-less list `[1 -2 3]`: space + `-` touching a number starts the next element.
    in_list: bool,
}

impl ParseFlags {
    const DEFAULT: ParseFlags = ParseFlags {
        bare_path_mode: false,
        stop_at_lbrace: false,
        stop_at_comma: false,
        list_bare_words: false,
        in_list: false,
    };
}

struct Parser<'s, 't, 'a> {
    src: &'s str,
    tokens: &'t [Token<'a>],
    pos: usize,
    alloc: Alloc<'a>,
    depth: usize,
    flags: ParseFlags,
}

impl<'s, 't, 'a> Parser<'s, 't, 'a> {
    fn new(src: &'s str, tokens: &'t [Token<'a>], alloc: Alloc<'a>, depth: usize) -> Self {
        Parser {
            src,
            tokens,
            pos: 0,
            alloc,
            depth,
            flags: ParseFlags::DEFAULT,
        }
    }

    #[inline]
    fn nested<T>(
        &mut self,
        span: Span,
        f: impl FnOnce(&mut Self) -> Result<T, ShellError<'a>>,
    ) -> Result<T, ShellError<'a>> {
        if self.depth >= MAX_PARSE_DEPTH {
            return Err(ShellError::new(
                ErrorKind::Budget,
                span,
                "maximum parse nesting depth exceeded",
                self.alloc,
            ));
        }
        self.depth += 1;
        let res = f(self);
        self.depth -= 1;
        res
    }

    #[inline]
    fn with_flags<T>(
        &mut self,
        flags: ParseFlags,
        f: impl FnOnce(&mut Self) -> Result<T, ShellError<'a>>,
    ) -> Result<T, ShellError<'a>> {
        let prev = self.flags;
        self.flags = flags;
        let res = f(self);
        self.flags = prev;
        res
    }

    #[inline]
    fn peek(&self) -> Option<&'t Token<'a>> {
        match self.tokens.get(self.pos) {
            Some(tok) if !matches!(tok.kind, TokKind::Eof) => Some(tok),
            _ => None,
        }
    }

    #[inline]
    fn peek_at(&self, offset: usize) -> Option<&'t Token<'a>> {
        match self.tokens.get(self.pos + offset) {
            Some(tok) if !matches!(tok.kind, TokKind::Eof) => Some(tok),
            _ => None,
        }
    }

    #[inline]
    fn is_eof(&self) -> bool {
        self.peek().is_none()
    }

    #[inline]
    fn cur_span(&self) -> Span {
        if let Some(tok) = self.tokens.get(self.pos) {
            tok.span
        } else {
            Span::from_usize(self.src.len(), self.src.len())
        }
    }

    #[inline]
    fn prev_span(&self) -> Span {
        if self.pos > 0
            && let Some(tok) = self.tokens.get(self.pos - 1)
        {
            tok.span
        } else {
            Span::ZERO
        }
    }

    #[inline]
    fn bump(&mut self) -> Option<&'t Token<'a>> {
        let tok = self.peek()?;
        self.pos += 1;
        Some(tok)
    }

    #[inline]
    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Token { kind: TokKind::Newline, .. })) {
            self.pos += 1;
        }
    }

    #[inline]
    fn skip_stmt_seps(&mut self) {
        while matches!(
            self.peek(),
            Some(Token { kind: TokKind::Newline | TokKind::Semi, .. })
        ) {
            self.pos += 1;
        }
    }

    #[inline]
    fn peek_is_kw(&self, kw: Keyword) -> bool {
        matches!(self.peek(), Some(Token { kind: TokKind::Kw(k), .. }) if *k == kw)
    }

    #[inline]
    fn eat_kw(&mut self, kw: Keyword) -> Option<Span> {
        if self.peek_is_kw(kw) {
            let span = self.tokens[self.pos].span;
            self.pos += 1;
            Some(span)
        } else {
            None
        }
    }

    fn err(&self, span: Span, msg: &str) -> ShellError<'a> {
        ShellError::new(ErrorKind::Parse, span, msg, self.alloc)
    }

    fn err_budget(&self, span: Span, msg: &str) -> ShellError<'a> {
        ShellError::new(ErrorKind::Budget, span, msg, self.alloc)
    }

    fn expect_lbrace(&mut self, what: &str) -> Result<Span, ShellError<'a>> {
        if matches!(self.peek(), Some(Token { kind: TokKind::LBrace, .. })) {
            let span = self.tokens[self.pos].span;
            self.pos += 1;
            Ok(span)
        } else {
            Err(self.err(self.cur_span(), what))
        }
    }

    fn expect_rbrace(&mut self) -> Result<Span, ShellError<'a>> {
        if matches!(self.peek(), Some(Token { kind: TokKind::RBrace, .. })) {
            let span = self.tokens[self.pos].span;
            self.pos += 1;
            Ok(span)
        } else {
            Err(self.err(self.cur_span(), "expected `}`"))
        }
    }

    fn expect_rparen(&mut self) -> Result<Span, ShellError<'a>> {
        if matches!(self.peek(), Some(Token { kind: TokKind::RParen, .. })) {
            let span = self.tokens[self.pos].span;
            self.pos += 1;
            Ok(span)
        } else {
            Err(self.err(self.cur_span(), "expected `)`"))
        }
    }

    fn expect_rbracket(&mut self) -> Result<Span, ShellError<'a>> {
        if matches!(self.peek(), Some(Token { kind: TokKind::RBracket, .. })) {
            let span = self.tokens[self.pos].span;
            self.pos += 1;
            Ok(span)
        } else {
            Err(self.err(self.cur_span(), "expected `]`"))
        }
    }

    fn token_word_arc(&self, tok: &Token<'a>) -> Option<ARc<'a, str>> {
        match &tok.kind {
            TokKind::Ident(s) => Some(s.share()),
            TokKind::Kw(kw) => Some(Rc::from_str_in(kw.as_str(), self.alloc)),
            _ => None,
        }
    }

    /// Parse a sequence of statements until `}` (if `stop_at_rbrace`) or EOF.
    fn parse_block(&mut self, stop_at_rbrace: bool) -> Result<Block<'a>, ShellError<'a>> {
        let start = self.cur_span();
        let mut stmts = AVec::new_in(self.alloc);
        let mut end = start;

        loop {
            self.skip_stmt_seps();
            if self.is_eof() {
                break;
            }
            if stop_at_rbrace && matches!(self.peek(), Some(Token { kind: TokKind::RBrace, .. })) {
                break;
            }
            if stmts.len() >= MAX_STMTS {
                return Err(self.err_budget(self.cur_span(), "statement count limit exceeded"));
            }
            let stmt = self.parse_stmt()?;
            end = stmt_span(&stmt);
            stmts.push(stmt);

            // Require separator or block end between statements.
            match self.peek() {
                None => break,
                Some(Token { kind: TokKind::Newline | TokKind::Semi, .. }) => {
                    self.skip_stmt_seps();
                }
                Some(Token { kind: TokKind::RBrace, .. }) if stop_at_rbrace => break,
                Some(tok) => {
                    return Err(self.err(
                        tok.span,
                        "unexpected token after statement (expected newline, `;`, or `}`)",
                    ));
                }
            }
        }

        let span = if stmts.is_empty() {
            start
        } else {
            stmt_span(&stmts[0]).merge(end)
        };
        Ok(Block { stmts, span })
    }

    fn parse_braced_block(&mut self, what: &str) -> Result<Block<'a>, ShellError<'a>> {
        let open = self.expect_lbrace(what)?;
        let mut block = self.nested(open, |p| p.with_flags(ParseFlags::DEFAULT, |p| p.parse_block(true)))?;
        let close = self.expect_rbrace()?;
        block.span = open.merge(close);
        Ok(block)
    }

    fn parse_stmt(&mut self) -> Result<Stmt<'a>, ShellError<'a>> {
        let span = self.cur_span();
        self.nested(span, |p| {
            let Some(tok) = p.peek() else {
                return Err(p.err(span, "expected statement"));
            };
            match &tok.kind {
                TokKind::Kw(Keyword::Let) => p.parse_let(false),
                TokKind::Kw(Keyword::Mut) => p.parse_let(true),
                TokKind::Kw(Keyword::Def) => p.parse_def(),
                TokKind::Kw(Keyword::Alias) => p.parse_alias(),
                TokKind::Kw(Keyword::For) => p.parse_for(),
                TokKind::Kw(Keyword::While) => p.parse_while(),
                TokKind::Kw(Keyword::Loop) => p.parse_loop(),
                TokKind::Kw(Keyword::Break) => {
                    let s = p.bump().unwrap().span;
                    Ok(Stmt::Break(s))
                }
                TokKind::Kw(Keyword::Continue) => {
                    let s = p.bump().unwrap().span;
                    Ok(Stmt::Continue(s))
                }
                TokKind::Kw(Keyword::Return) => p.parse_return(),
                TokKind::Var(_) => {
                    if let Some(assign) = p.try_parse_assign()? {
                        Ok(assign)
                    } else {
                        let expr = p.parse_pipeline()?;
                        Ok(Stmt::Expr(expr))
                    }
                }
                _ => {
                    let expr = p.parse_pipeline()?;
                    Ok(Stmt::Expr(expr))
                }
            }
        })
    }

    fn parse_let(&mut self, mut mutable: bool) -> Result<Stmt<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `let` or `mut`
        if !mutable && self.eat_kw(Keyword::Mut).is_some() {
            mutable = true;
        }

        let name = self.parse_binding_name("expected variable name")?;
        let ty = if matches!(self.peek(), Some(Token { kind: TokKind::Colon, .. })) {
            self.pos += 1;
            Some(self.parse_type_ann()?)
        } else {
            None
        };

        if !matches!(self.peek(), Some(Token { kind: TokKind::Eq, .. })) {
            return Err(self.err(self.cur_span(), "expected `=` in variable binding"));
        }
        self.pos += 1;

        let value = self.with_flags(ParseFlags::DEFAULT, |p| p.parse_pipeline())?;
        let span = start.merge(value.span);
        Ok(Stmt::Let { name, mutable, ty, value, span })
    }

    fn parse_binding_name(&mut self, msg: &str) -> Result<ARc<'a, str>, ShellError<'a>> {
        let Some(tok) = self.peek() else {
            return Err(self.err(self.cur_span(), msg));
        };
        let name = match &tok.kind {
            TokKind::Ident(s) | TokKind::Var(s) => s.share(),
            _ => return Err(self.err(tok.span, msg)),
        };
        self.pos += 1;
        Ok(name)
    }

    fn parse_type_ann(&mut self) -> Result<TypeAnn, ShellError<'a>> {
        let Some(tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected type name after `:`"));
        };
        let span = tok.span;
        let word = token_word(tok)
            .ok_or_else(|| self.err(span, "expected type name after `:`"))?;
        let ty = TypeAnn::from_name(word)
            .ok_or_else(|| self.err(span, "unknown type annotation"))?;
        self.pos += 1;
        Ok(ty)
    }

    /// Speculatively parse `$var[.path...] (=|+=|-=|*=|/=|++=) pipeline`.
    fn try_parse_assign(&mut self) -> Result<Option<Stmt<'a>>, ShellError<'a>> {
        let saved = self.pos;
        let Some(Token { kind: TokKind::Var(var), span: var_span }) = self.peek() else {
            return Ok(None);
        };
        let var = var.share();
        let var_span = *var_span;
        self.pos += 1;

        let mut path = AVec::new_in(self.alloc);
        let mut target_end = var_span;

        while let Some(tok) = self.peek() {
            let optional = match tok.kind {
                TokKind::Dot => false,
                TokKind::QuestionDot => true,
                _ => break,
            };
            if path.len() >= MAX_PATH_STEPS {
                return Err(self.err_budget(tok.span, "cell path exceeds maximum step count"));
            }
            let dot_span = tok.span;
            self.pos += 1;
            let Some(step) = self.try_parse_path_step(dot_span, optional) else {
                self.pos = saved;
                return Ok(None);
            };
            target_end = step.span;
            path.push(step);
        }

        let op = match self.peek().map(|t| &t.kind) {
            Some(TokKind::Eq) => AssignOp::Set,
            Some(TokKind::PlusEq) => AssignOp::AddSet,
            Some(TokKind::MinusEq) => AssignOp::SubSet,
            Some(TokKind::StarEq) => AssignOp::MulSet,
            Some(TokKind::SlashEq) => AssignOp::DivSet,
            Some(TokKind::PlusPlusEq) => AssignOp::AppendSet,
            _ => {
                self.pos = saved;
                return Ok(None);
            }
        };
        self.pos += 1; // consume assignment operator

        let value = self.with_flags(ParseFlags::DEFAULT, |p| p.parse_pipeline())?;
        let target_span = var_span.merge(target_end);
        let span = var_span.merge(value.span);
        Ok(Some(Stmt::Assign {
            target: AssignTarget { var, path, span: target_span },
            op,
            value,
            span,
        }))
    }

    fn parse_def(&mut self) -> Result<Stmt<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `def`
        let Some(name_tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected command name after `def`"));
        };
        let name = match &name_tok.kind {
            TokKind::Ident(s) | TokKind::Str(s) => s.share(),
            TokKind::Kw(kw) => Rc::from_str_in(kw.as_str(), self.alloc),
            _ => return Err(self.err(name_tok.span, "expected command name after `def`")),
        };
        self.pos += 1;

        let mut params = AVec::new_in(self.alloc);
        if matches!(self.peek(), Some(Token { kind: TokKind::LBracket, .. })) {
            self.pos += 1; // `[`
            loop {
                self.skip_newlines();
                if matches!(self.peek(), Some(Token { kind: TokKind::RBracket, .. })) {
                    self.pos += 1;
                    break;
                }
                if self.is_eof() {
                    return Err(self.err(self.cur_span(), "unterminated parameter list `[...]` in `def`"));
                }
                if params.len() >= MAX_PARAMS {
                    return Err(self.err_budget(self.cur_span(), "parameter count limit exceeded"));
                }
                let param = self.parse_param_def()?;
                params.push(param);
                self.skip_newlines();
                if matches!(self.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                    self.pos += 1;
                }
            }
        }

        self.skip_newlines();
        let body = self.parse_braced_block("expected `{` to start `def` body")?;
        let span = start.merge(body.span);
        Ok(Stmt::Def {
            name,
            params,
            body: Rc::new_in(body, self.alloc),
            span,
        })
    }

    fn parse_param_def(&mut self) -> Result<ParamDef<'a>, ShellError<'a>> {
        let start = self.cur_span();

        // Rest parameter: `...rest[: ty]`
        if matches!(self.peek(), Some(Token { kind: TokKind::DotDotDot, .. })) {
            self.pos += 1;
            let name = self.parse_binding_name("expected parameter name after `...`")?;
            let ty = if matches!(self.peek(), Some(Token { kind: TokKind::Colon, .. })) {
                self.pos += 1;
                Some(self.parse_type_ann()?)
            } else {
                None
            };
            let span = start.merge(self.prev_span());
            return Ok(ParamDef {
                name,
                short: None,
                kind: ParamKind::Rest,
                ty,
                default: None,
                span,
            });
        }

        // Flag / named option parameter: `--flag(-f)` or `--opt(-o): int = 5`
        if matches!(self.peek(), Some(Token { kind: TokKind::Minus, .. })) {
            let m1 = self.bump().unwrap().span;
            let is_long = matches!(self.peek(), Some(Token { kind: TokKind::Minus, span, .. }) if span.start == m1.end);
            if !is_long {
                return Err(self.err(m1, "expected `--name` for flag/named parameter in `def`"));
            }
            let m2 = self.bump().unwrap().span;
            let Some(name_tok) = self.peek() else {
                return Err(self.err(self.cur_span(), "expected flag name after `--`"));
            };
            if name_tok.span.start != m2.end {
                return Err(self.err(name_tok.span, "expected flag name immediately after `--`"));
            }
            let name = self
                .token_word_arc(name_tok)
                .ok_or_else(|| self.err(name_tok.span, "expected flag name after `--`"))?;
            self.pos += 1;

            // Optional short alias `(-f)`
            let mut short = None;
            if matches!(self.peek(), Some(Token { kind: TokKind::LParen, .. })) {
                self.pos += 1;
                if !matches!(self.peek(), Some(Token { kind: TokKind::Minus, .. })) {
                    return Err(self.err(self.cur_span(), "expected `-x` inside short flag `(-x)`"));
                }
                self.pos += 1;
                let Some(short_tok) = self.peek() else {
                    return Err(self.err(self.cur_span(), "expected single character short flag"));
                };
                let short_span = short_tok.span;
                let word = token_word(short_tok)
                    .ok_or_else(|| self.err(short_span, "expected single character short flag"))?;
                let mut chars = word.chars();
                let ch = chars
                    .next()
                    .ok_or_else(|| self.err(short_span, "expected single character short flag"))?;
                if chars.next().is_some() {
                    return Err(self.err(short_span, "short flag alias must be a single character"));
                }
                self.pos += 1;
                self.expect_rparen()?;
                short = Some(ch);
            }

            let ty = if matches!(self.peek(), Some(Token { kind: TokKind::Colon, .. })) {
                self.pos += 1;
                Some(self.parse_type_ann()?)
            } else {
                None
            };

            let default = if matches!(self.peek(), Some(Token { kind: TokKind::Eq, .. })) {
                self.pos += 1;
                Some(self.with_flags(ParseFlags::DEFAULT, |p| p.parse_expr())?)
            } else {
                None
            };

            let kind = if ty.is_some() || default.is_some() {
                ParamKind::Named
            } else {
                ParamKind::Flag
            };
            let span = start.merge(self.prev_span());
            return Ok(ParamDef { name, short, kind, ty, default, span });
        }

        // Positional parameter: `a`, `a?`, `a: int`, `a: int = 1`
        let name = self.parse_binding_name("expected parameter name in `def`")?;
        let mut kind = ParamKind::Positional;
        if matches!(self.peek(), Some(Token { kind: TokKind::Question, .. })) {
            self.pos += 1;
            kind = ParamKind::OptionalPositional;
        }
        let ty = if matches!(self.peek(), Some(Token { kind: TokKind::Colon, .. })) {
            self.pos += 1;
            Some(self.parse_type_ann()?)
        } else {
            None
        };
        let default = if matches!(self.peek(), Some(Token { kind: TokKind::Eq, .. })) {
            self.pos += 1;
            kind = ParamKind::OptionalPositional;
            Some(self.with_flags(ParseFlags::DEFAULT, |p| p.parse_expr())?)
        } else {
            None
        };
        let span = start.merge(self.prev_span());
        Ok(ParamDef { name, short: None, kind, ty, default, span })
    }

    fn parse_alias(&mut self) -> Result<Stmt<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `alias`
        let Some(name_tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected alias name after `alias`"));
        };
        let name = self
            .token_word_arc(name_tok)
            .ok_or_else(|| self.err(name_tok.span, "expected alias name after `alias`"))?;
        self.pos += 1;

        if !matches!(self.peek(), Some(Token { kind: TokKind::Eq, .. })) {
            return Err(self.err(self.cur_span(), "expected `=` after alias name"));
        }
        self.pos += 1;

        // Capture rest of line (up to Newline or Eof) directly from source text.
        let mut first_span: Option<Span> = None;
        let mut last_span = self.prev_span();
        while let Some(tok) = self.peek() {
            if matches!(tok.kind, TokKind::Newline) {
                break;
            }
            if first_span.is_none() {
                first_span = Some(tok.span);
            }
            last_span = tok.span;
            self.pos += 1;
        }
        let Some(first) = first_span else {
            return Err(self.err(self.cur_span(), "expected expansion text after `=` in `alias`"));
        };
        let exp_span = first.merge(last_span);
        let expansion = Rc::from_str_in(exp_span.slice(self.src).trim(), self.alloc);
        let span = start.merge(exp_span);
        Ok(Stmt::Alias { name, expansion, span })
    }

    fn parse_for(&mut self) -> Result<Stmt<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `for`
        let var = self.parse_binding_name("expected loop variable name after `for`")?;
        if self.eat_kw(Keyword::In).is_none() {
            return Err(self.err(self.cur_span(), "expected `in` after `for` loop variable"));
        }
        let iter = self.with_flags(
            ParseFlags { stop_at_lbrace: true, ..ParseFlags::DEFAULT },
            |p| p.parse_stage(),
        )?;
        let body = self.parse_braced_block("expected `{` to start `for` loop body")?;
        let span = start.merge(body.span);
        Ok(Stmt::For { var, iter, body, span })
    }

    fn parse_while(&mut self) -> Result<Stmt<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `while`
        let cond = self.with_flags(
            ParseFlags { stop_at_lbrace: true, ..ParseFlags::DEFAULT },
            |p| p.parse_stage(),
        )?;
        let body = self.parse_braced_block("expected `{` to start `while` loop body")?;
        let span = start.merge(body.span);
        Ok(Stmt::While { cond, body, span })
    }

    fn parse_loop(&mut self) -> Result<Stmt<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `loop`
        let body = self.parse_braced_block("expected `{` to start `loop` body")?;
        let span = start.merge(body.span);
        Ok(Stmt::Loop { body, span })
    }

    fn parse_return(&mut self) -> Result<Stmt<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `return`
        let has_val = !matches!(
            self.peek().map(|t| &t.kind),
            None
                | Some(
                    TokKind::Newline
                        | TokKind::Semi
                        | TokKind::RBrace
                        | TokKind::RParen
                        | TokKind::RBracket
                        | TokKind::Pipe
                )
        );
        if has_val {
            let val = self.parse_pipeline()?;
            let span = start.merge(val.span);
            Ok(Stmt::Return { value: Some(val), span })
        } else {
            Ok(Stmt::Return { value: None, span: start })
        }
    }

    /// Parse a pipeline `stage1 | stage2 | ...` (allowing newlines after `|`).
    fn parse_pipeline(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let first = self.parse_stage()?;
        if !matches!(self.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
            return Ok(first);
        }

        let start = first.span;
        let mut end = start;
        let mut stages = AVec::new_in(self.alloc);
        stages.push(first);

        while matches!(self.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
            self.pos += 1; // consume `|`
            self.skip_newlines();
            if stages.len() >= MAX_PIPELINE_STAGES {
                return Err(self.err_budget(self.cur_span(), "pipeline stage limit exceeded"));
            }
            let stage = self.parse_stage()?;
            end = stage.span;
            stages.push(stage);
        }

        Ok(Expr {
            kind: ExprKind::Pipeline(stages),
            span: start.merge(end),
        })
    }

    /// Parse a single pipeline stage: either an expression (control flow, literal, var,
    /// container, unary, binary) or a command call (`ls -la`, `^git status`, `./tool`).
    fn parse_stage(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let span = self.cur_span();
        self.nested(span, |p| {
            let Some(tok) = p.peek() else {
                return Err(p.err(span, "expected expression or command"));
            };

            if p.flags.bare_path_mode {
                return p.parse_expr();
            }

            match &tok.kind {
                // Forced external command `^cmd ...`
                TokKind::Caret => p.parse_call(true),

                // Control-flow & literal/expression starters -> full expression parser
                TokKind::RustCell(_)
                | TokKind::Kw(
                    Keyword::If
                        | Keyword::Match
                        | Keyword::Try
                        | Keyword::True
                        | Keyword::False
                        | Keyword::Null
                        | Keyword::Not,
                )
                | TokKind::Int(_)
                | TokKind::Float(_)
                | TokKind::Size(_)
                | TokKind::Duration(_)
                | TokKind::Str(_)
                | TokKind::Interp(_)
                | TokKind::Var(_)
                | TokKind::Dollar
                | TokKind::LParen
                | TokKind::LBracket
                | TokKind::LBrace
                | TokKind::Minus
                | TokKind::DotDotLt => p.parse_expr(),

                // `../foo` is a command path; `..10` or `..` is a prefix range expression.
                TokKind::DotDot => {
                    let touches_slash = matches!(
                        p.peek_at(1),
                        Some(Token { kind: TokKind::Slash, span: s, .. }) if s.start == tok.span.end
                    );
                    if touches_slash {
                        p.parse_call(false)
                    } else {
                        p.parse_expr()
                    }
                }

                // `./foo` is a command path.
                TokKind::Dot => {
                    let touches_slash = matches!(
                        p.peek_at(1),
                        Some(Token { kind: TokKind::Slash, span: s, .. }) if s.start == tok.span.end
                    );
                    if touches_slash {
                        p.parse_call(false)
                    } else {
                        Err(p.err(tok.span, "unexpected `.` at start of expression"))
                    }
                }

                // `/bin/ls` or `~/bin/tool`
                TokKind::Slash | TokKind::Tilde => p.parse_call(false),

                // Bare word (`nothing` standalone vs command call)
                TokKind::Ident(s) => {
                    if s.as_ref() == "nothing" && p.next_is_expr_continuation_or_end() {
                        p.parse_expr()
                    } else {
                        p.parse_call(false)
                    }
                }

                _ => Err(p.err(tok.span, "unexpected token at start of expression")),
            }
        })
    }

    fn next_is_expr_continuation_or_end(&self) -> bool {
        matches!(
            self.peek_at(1).map(|t| &t.kind),
            None | Some(
                TokKind::Newline
                    | TokKind::Semi
                    | TokKind::Pipe
                    | TokKind::RParen
                    | TokKind::RBracket
                    | TokKind::RBrace
                    | TokKind::Comma
                    | TokKind::EqEq
                    | TokKind::BangEq
                    | TokKind::Lt
                    | TokKind::LtEq
                    | TokKind::Gt
                    | TokKind::GtEq
                    | TokKind::Kw(
                        Keyword::And | Keyword::Or | Keyword::Xor | Keyword::In | Keyword::NotIn
                    )
            )
        )
    }

    /// Parse a command call (`cmd arg1 --flag=val ...`) with special Nushell ergonomics for `where`.
    fn parse_call(&mut self, forced_external: bool) -> Result<Expr<'a>, ShellError<'a>> {
        let start = self.cur_span();
        if forced_external {
            self.pos += 1; // consume `^`
        }

        let (head, head_span) = self.parse_bare_word_token("expected command name")?;

        // Special Nushell `where` ergonomics:
        // If head == "where" (not forced_external) and the first arg is NOT a closure `{|...| ...}`,
        // parse the rest of the pipeline stage as a single predicate expression where bare
        // identifiers become `ExprKind::BarePath`.
        if !forced_external
            && head.as_ref() == "where"
            && !self.is_stage_end()
            && !self.is_closure_start()
        {
            let mut args = AVec::new_in(self.alloc);
            let pred = self.with_flags(
                ParseFlags {
                    bare_path_mode: true,
                    stop_at_lbrace: self.flags.stop_at_lbrace,
                    stop_at_comma: self.flags.stop_at_comma,
                    list_bare_words: false,
                    in_list: false,
                },
                |p| p.parse_expr(),
            )?;
            let span = start.merge(pred.span);
            args.push(CallArg::Positional(pred));
            return Ok(Expr {
                kind: ExprKind::Call(CallExpr { head, head_span, forced_external, args }),
                span,
            });
        }

        let mut args = AVec::new_in(self.alloc);
        let mut end = head_span;

        while !self.is_stage_end() {
            if args.len() >= MAX_CALL_ARGS {
                return Err(self.err_budget(self.cur_span(), "command argument limit exceeded"));
            }
            let arg = self.parse_call_arg()?;
            end = call_arg_span(&arg);
            args.push(arg);
        }

        Ok(Expr {
            kind: ExprKind::Call(CallExpr { head, head_span, forced_external, args }),
            span: start.merge(end),
        })
    }

    fn is_stage_end(&self) -> bool {
        matches!(
            self.peek().map(|t| &t.kind),
            None | Some(
                TokKind::Pipe
                    | TokKind::Semi
                    | TokKind::Newline
                    | TokKind::RParen
                    | TokKind::RBracket
                    | TokKind::RBrace
            )
        ) || (self.flags.stop_at_lbrace
            && matches!(self.peek(), Some(Token { kind: TokKind::LBrace, .. })))
            || (self.flags.stop_at_comma
                && matches!(self.peek(), Some(Token { kind: TokKind::Comma, .. })))
    }

    /// True if the upcoming tokens start a closure `{|...| ...}`.
    fn is_closure_start(&self) -> bool {
        if !matches!(self.peek(), Some(Token { kind: TokKind::LBrace, .. })) {
            return false;
        }
        let mut idx = 1;
        while matches!(self.peek_at(idx), Some(Token { kind: TokKind::Newline, .. })) {
            idx += 1;
        }
        matches!(self.peek_at(idx), Some(Token { kind: TokKind::Pipe, .. }))
    }

    fn parse_call_arg(&mut self) -> Result<CallArg<'a>, ShellError<'a>> {
        let Some(tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected command argument"));
        };

        // Flags (`--long`, `--long=val`, `-r`, `-la`, `-f=val`) vs negative numbers (`-5`, `-3.14`)
        if matches!(tok.kind, TokKind::Minus) {
            let m1 = tok.span;

            // `--...`
            if matches!(self.peek_at(1), Some(Token { kind: TokKind::Minus, span: m2, .. }) if m2.start == m1.end)
            {
                let m2_end = self.tokens[self.pos + 1].span.end;
                // `--flag` if touching an ident/keyword word
                if let Some(name_tok) = self.peek_at(2)
                    && name_tok.span.start == m2_end
                    && token_word(name_tok).is_some()
                {
                    let name = self.token_word_arc(name_tok).unwrap();
                    let name_span = name_tok.span;
                    self.pos += 3; // `-`, `-`, name
                    let (value, end) = self.parse_optional_flag_eq_value(name_span)?;
                    return Ok(CallArg::Flag {
                        name,
                        short: false,
                        value,
                        span: m1.merge(end),
                    });
                }
                // Standalone `--` separator
                self.pos += 2;
                let span = Span::new(m1.start, m2_end);
                return Ok(CallArg::Positional(Expr {
                    kind: ExprKind::String(Rc::from_str_in("--", self.alloc)),
                    span,
                }));
            }

            // `-5`, `-3.14`, `-10kb`, `-5sec`: negative numeric literal (not a flag!)
            if let Some(num_tok) = self.peek_at(1)
                && num_tok.span.start == m1.end
                && matches!(
                    num_tok.kind,
                    TokKind::Int(_) | TokKind::Float(_) | TokKind::Size(_) | TokKind::Duration(_)
                )
            {
                self.pos += 1; // consume `-`
                let neg_expr = self.parse_negated_literal(m1)?;
                return Ok(CallArg::Positional(neg_expr));
            }

            // `-r`, `-la`, `-f=val`: short flag
            if let Some(name_tok) = self.peek_at(1)
                && name_tok.span.start == m1.end
                && token_word(name_tok).is_some()
            {
                let name = self.token_word_arc(name_tok).unwrap();
                let name_span = name_tok.span;
                self.pos += 2; // `-`, name
                let (value, end) = self.parse_optional_flag_eq_value(name_span)?;
                return Ok(CallArg::Flag {
                    name,
                    short: true,
                    value,
                    span: m1.merge(end),
                });
            }

            // Standalone `-` (e.g. `cd -`, `cat -`)
            self.pos += 1;
            return Ok(CallArg::Positional(Expr {
                kind: ExprKind::String(Rc::from_str_in("-", self.alloc)),
                span: m1,
            }));
        }

        let expr = self.parse_arg_expr()?;
        Ok(CallArg::Positional(expr))
    }

    fn parse_optional_flag_eq_value(
        &mut self,
        name_span: Span,
    ) -> Result<(Option<Expr<'a>>, Span), ShellError<'a>> {
        if matches!(self.peek(), Some(Token { kind: TokKind::Eq, span: eq_span, .. }) if eq_span.start == name_span.end)
        {
            let eq_end = self.tokens[self.pos].span.end;
            self.pos += 1; // consume `=`
            if !matches!(self.peek(), Some(t) if t.span.start == eq_end) {
                return Err(self.err(
                    Span::new(name_span.start, eq_end),
                    "expected value immediately after `=` in flag",
                ));
            }
            let val = self.parse_arg_expr()?;
            let end = val.span;
            Ok((Some(val), end))
        } else {
            Ok((None, name_span))
        }
    }

    fn parse_negated_literal(&mut self, minus_span: Span) -> Result<Expr<'a>, ShellError<'a>> {
        let tok = self.bump().unwrap();
        let span = minus_span.merge(tok.span);
        match tok.kind {
            TokKind::Int(n) => {
                let neg = n.checked_neg().ok_or_else(|| {
                    ShellError::new(ErrorKind::Overflow, span, "integer negation overflow", self.alloc)
                })?;
                Ok(Expr { kind: ExprKind::Int(neg), span })
            }
            TokKind::Float(f) => Ok(Expr { kind: ExprKind::Float(-f), span }),
            TokKind::Size(n) => {
                let neg = n.checked_neg().ok_or_else(|| {
                    ShellError::new(ErrorKind::Overflow, span, "size negation overflow", self.alloc)
                })?;
                Ok(Expr { kind: ExprKind::Size(neg), span })
            }
            TokKind::Duration(n) => {
                let neg = n.checked_neg().ok_or_else(|| {
                    ShellError::new(
                        ErrorKind::Overflow,
                        span,
                        "duration negation overflow",
                        self.alloc,
                    )
                })?;
                Ok(Expr { kind: ExprKind::Duration(neg), span })
            }
            _ => Err(self.err(span, "expected numeric literal after `-`")),
        }
    }

    /// Parse a single positional command argument or `--flag=<value>` expression.
    fn parse_arg_expr(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let Some(tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected argument"));
        };

        match &tok.kind {
            // Parenthesized expression `(1 + 2)` or `(ls | length)` with optional postfix path
            TokKind::LParen => {
                let primary = self.parse_paren_expr()?;
                self.parse_postfix(primary)
            }
            // List / Table `[...]`
            TokKind::LBracket => {
                let primary = self.parse_bracket_expr()?;
                self.parse_postfix(primary)
            }
            // Closure `{|x| ...}`, Record `{a: 1}`, or Block `{ ... }`
            TokKind::LBrace => {
                let primary = self.parse_brace_expr()?;
                self.parse_postfix(primary)
            }
            // Quoted strings `"..."`, `$"..."`, `'...'`, `r#'...'#`
            TokKind::Str(_) | TokKind::Interp(_) => {
                let primary = self.parse_string_token()?;
                self.parse_postfix(primary)
            }
            // Variables `$x`, `$x.a.0?.b` and bare `$.a.b`
            TokKind::Var(_) | TokKind::Dollar => {
                let primary = self.parse_primary()?;
                self.parse_postfix(primary)
            }
            // Negative numeric literal (can appear after `--flag=-5`)
            TokKind::Minus
                if matches!(
                    self.peek_at(1),
                    Some(Token {
                        kind: TokKind::Int(_) | TokKind::Float(_) | TokKind::Size(_) | TokKind::Duration(_),
                        span: s,
                        ..
                    }) if s.start == tok.span.end
                ) =>
            {
                let m = self.bump().unwrap().span;
                self.parse_negated_literal(m)
            }
            // Numeric literals: watch out for ranges `1..10` and path/IP glues `127.0.0.1` / `2026-11-24`
            TokKind::Int(_) | TokKind::Float(_) | TokKind::Size(_) | TokKind::Duration(_) => {
                let tok_end = tok.span.end;
                if let Some(next) = self.peek_at(1)
                    && next.span.start == tok_end
                {
                    if matches!(next.kind, TokKind::DotDot | TokKind::DotDotLt) {
                        return self.parse_range();
                    }
                    if is_bare_word_glue_token(&next.kind) {
                        let (word, span) = self.parse_bare_word_token("expected argument")?;
                        return Ok(Expr { kind: ExprKind::String(word), span });
                    }
                }
                self.parse_primary()
            }
            // Prefix ranges `..10`, `..<10` vs bare path `..` / `../foo`
            TokKind::DotDot | TokKind::DotDotLt => {
                let tok_end = tok.span.end;
                let touches_slash = matches!(
                    self.peek_at(1),
                    Some(Token { kind: TokKind::Slash, span: s, .. }) if s.start == tok_end
                );
                let touches_num_or_var = matches!(
                    self.peek_at(1),
                    Some(Token {
                        kind: TokKind::Int(_) | TokKind::Var(_) | TokKind::LParen | TokKind::Minus,
                        span: s,
                        ..
                    }) if s.start == tok_end
                );
                if touches_num_or_var && !touches_slash {
                    self.parse_range()
                } else {
                    let (word, span) = self.parse_bare_word_token("expected argument")?;
                    Ok(Expr { kind: ExprKind::String(word), span })
                }
            }
            // `true`, `false`, `null`, `nothing` ONLY when standalone (not glued like `true.txt`)
            TokKind::Kw(Keyword::True | Keyword::False | Keyword::Null) => {
                let tok_end = tok.span.end;
                if self
                    .peek_at(1)
                    .is_some_and(|n| n.span.start == tok_end && is_bare_word_glue_token(&n.kind))
                {
                    let (word, span) = self.parse_bare_word_token("expected argument")?;
                    Ok(Expr { kind: ExprKind::String(word), span })
                } else {
                    self.parse_primary()
                }
            }
            TokKind::Ident(s) if s.as_ref() == "nothing" => {
                let tok_end = tok.span.end;
                if self
                    .peek_at(1)
                    .is_some_and(|n| n.span.start == tok_end && is_bare_word_glue_token(&n.kind))
                {
                    let (word, span) = self.parse_bare_word_token("expected argument")?;
                    Ok(Expr { kind: ExprKind::String(word), span })
                } else {
                    let span = self.bump().unwrap().span;
                    Ok(Expr { kind: ExprKind::Nothing, span })
                }
            }
            // Bare words, paths, globs, URLs (`Cargo.toml`, `./src/main.rs`, `*.rs`, `/tmp/foo`)
            _ => {
                let (word, span) = self.parse_bare_word_token("expected argument")?;
                Ok(Expr { kind: ExprKind::String(word), span })
            }
        }
    }

    /// Glue consecutive touching bare-word/path tokens (`Cargo.toml`, `./src/main.rs`, `*.rs`)
    /// into a single `ARc<str>` sliced directly from `self.src`.
    fn parse_bare_word_token(&mut self, msg: &str) -> Result<(ARc<'a, str>, Span), ShellError<'a>> {
        let Some(first) = self.peek() else {
            return Err(self.err(self.cur_span(), msg));
        };
        if !is_bare_word_token(&first.kind, self.flags.stop_at_comma) {
            return Err(self.err(first.span, msg));
        }
        let start = first.span.start;
        let mut end = first.span.end;
        self.pos += 1;

        while let Some(next) = self.peek() {
            if next.span.start != end {
                break;
            }
            if !is_bare_word_token(&next.kind, self.flags.stop_at_comma) {
                break;
            }
            end = next.span.end;
            self.pos += 1;
        }

        let span = Span::new(start, end);
        let text = Rc::from_str_in(span.slice(self.src), self.alloc);
        Ok((text, span))
    }

    // -------------------------------------------------------------------------
    // Expressions & Operator Precedence
    //
    // 1. or
    // 2. xor
    // 3. and
    // 4. == != < <= > >= =~ !~ in not-in starts-with ends-with
    // 5. ++
    // 6. .. ..<
    // 7. + -
    // 8. * / // mod
    // 9. ** (right-associative)
    // 10. unary -, not
    // 11. postfix .field, .0, ?.field, ?.0
    // -------------------------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let span = self.cur_span();
        self.nested(span, |p| p.parse_or())
    }

    fn parse_or(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let mut left = self.parse_xor()?;
        while self.eat_kw(Keyword::Or).is_some() {
            let right = self.parse_xor()?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Or,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_xor(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let mut left = self.parse_and()?;
        while self.eat_kw(Keyword::Xor).is_some() {
            let right = self.parse_and()?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Xor,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let mut left = self.parse_comparison()?;
        while self.eat_kw(Keyword::And).is_some() {
            let right = self.parse_comparison()?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::And,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let mut left = self.parse_append()?;
        loop {
            let op = match self.peek().map(|t| &t.kind) {
                Some(TokKind::EqEq) => BinaryOp::Eq,
                Some(TokKind::BangEq) => BinaryOp::Ne,
                Some(TokKind::Lt) => BinaryOp::Lt,
                Some(TokKind::LtEq) => BinaryOp::Le,
                Some(TokKind::Gt) => BinaryOp::Gt,
                Some(TokKind::GtEq) => BinaryOp::Ge,
                Some(TokKind::EqTilde) => BinaryOp::RegexMatch,
                Some(TokKind::BangTilde) => BinaryOp::RegexNotMatch,
                Some(TokKind::Kw(Keyword::In)) => BinaryOp::In,
                Some(TokKind::Kw(Keyword::NotIn)) => BinaryOp::NotIn,
                Some(TokKind::Kw(Keyword::StartsWith)) => BinaryOp::StartsWith,
                Some(TokKind::Kw(Keyword::EndsWith)) => BinaryOp::EndsWith,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_append()?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_append(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let mut left = self.parse_range()?;
        while matches!(self.peek(), Some(Token { kind: TokKind::PlusPlus, .. })) {
            self.pos += 1;
            let right = self.parse_range()?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Append,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_range(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        // Prefix ranges: `..10`, `..<10`, `..`
        if matches!(self.peek(), Some(Token { kind: TokKind::DotDot | TokKind::DotDotLt, .. })) {
            let op_tok = self.bump().unwrap();
            let inclusive = matches!(op_tok.kind, TokKind::DotDot);
            if self.can_start_additive() {
                let end_expr = self.parse_additive()?;
                let span = op_tok.span.merge(end_expr.span);
                return Ok(Expr {
                    kind: ExprKind::Range {
                        start: None,
                        step: None,
                        end: Some(Box::new_in(end_expr, self.alloc)),
                        inclusive,
                    },
                    span,
                });
            }
            if !inclusive {
                return Err(self.err(self.cur_span(), "expected end expression after `..<`"));
            }
            return Ok(Expr {
                kind: ExprKind::Range {
                    start: None,
                    step: None,
                    end: None,
                    inclusive: true,
                },
                span: op_tok.span,
            });
        }

        let start_expr = self.parse_additive()?;

        // Infix / postfix ranges: `a..b`, `a..<b`, `a..step..b`, `a..step..<b`, `a..`
        if !matches!(self.peek(), Some(Token { kind: TokKind::DotDot | TokKind::DotDotLt, .. })) {
            return Ok(start_expr);
        }

        let op1 = self.bump().unwrap();
        let op1_inclusive = matches!(op1.kind, TokKind::DotDot);

        if !op1_inclusive {
            let end_expr = self.parse_additive()?;
            let span = start_expr.span.merge(end_expr.span);
            return Ok(Expr {
                kind: ExprKind::Range {
                    start: Some(Box::new_in(start_expr, self.alloc)),
                    step: None,
                    end: Some(Box::new_in(end_expr, self.alloc)),
                    inclusive: false,
                },
                span,
            });
        }

        // `start..` with no end expression
        if !self.can_start_additive() {
            let span = start_expr.span.merge(op1.span);
            return Ok(Expr {
                kind: ExprKind::Range {
                    start: Some(Box::new_in(start_expr, self.alloc)),
                    step: None,
                    end: None,
                    inclusive: true,
                },
                span,
            });
        }

        let mid_expr = self.parse_additive()?;

        // Stepped range: `start..step..end`, `start..step..<end`, `start..step..`
        if matches!(self.peek(), Some(Token { kind: TokKind::DotDot | TokKind::DotDotLt, .. })) {
            let op2 = self.bump().unwrap();
            let inclusive = matches!(op2.kind, TokKind::DotDot);
            if inclusive && !self.can_start_additive() {
                let span = start_expr.span.merge(op2.span);
                return Ok(Expr {
                    kind: ExprKind::Range {
                        start: Some(Box::new_in(start_expr, self.alloc)),
                        step: Some(Box::new_in(mid_expr, self.alloc)),
                        end: None,
                        inclusive: true,
                    },
                    span,
                });
            }
            let end_expr = self.parse_additive()?;
            let span = start_expr.span.merge(end_expr.span);
            return Ok(Expr {
                kind: ExprKind::Range {
                    start: Some(Box::new_in(start_expr, self.alloc)),
                    step: Some(Box::new_in(mid_expr, self.alloc)),
                    end: Some(Box::new_in(end_expr, self.alloc)),
                    inclusive,
                },
                span,
            });
        }

        let span = start_expr.span.merge(mid_expr.span);
        Ok(Expr {
            kind: ExprKind::Range {
                start: Some(Box::new_in(start_expr, self.alloc)),
                step: None,
                end: Some(Box::new_in(mid_expr, self.alloc)),
                inclusive: true,
            },
            span,
        })
    }

    fn can_start_additive(&self) -> bool {
        matches!(
            self.peek().map(|t| &t.kind),
            Some(
                TokKind::Int(_)
                    | TokKind::Float(_)
                    | TokKind::Size(_)
                    | TokKind::Duration(_)
                    | TokKind::Str(_)
                    | TokKind::Interp(_)
                    | TokKind::Var(_)
                    | TokKind::Dollar
                    | TokKind::LParen
                    | TokKind::LBracket
                    | TokKind::LBrace
                    | TokKind::Minus
                    | TokKind::RustCell(_)
                    | TokKind::Kw(
                        Keyword::True
                            | Keyword::False
                            | Keyword::Null
                            | Keyword::Not
                            | Keyword::If
                            | Keyword::Match
                            | Keyword::Try
                    )
            )
        ) || matches!(
            self.peek().map(|t| &t.kind),
            Some(TokKind::Ident(s))
                if self.flags.bare_path_mode
                    || self.flags.list_bare_words
                    || s.as_ref() == "nothing"
        )
    }

    fn parse_additive(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Token { kind: TokKind::Plus, .. }) => BinaryOp::Add,
                Some(Token { kind: TokKind::Minus, span: mspan, .. }) => {
                    // Inside a comma-less list `[1 -2 3]`, whitespace before `-` followed
                    // immediately (no space) by a numeric literal starts the next list element.
                    if self.flags.in_list
                        && mspan.start > left.span.end
                        && let Some(next) = self.peek_at(1)
                        && next.span.start == mspan.end
                        && matches!(
                            next.kind,
                            TokKind::Int(_)
                                | TokKind::Float(_)
                                | TokKind::Size(_)
                                | TokKind::Duration(_)
                        )
                    {
                        break;
                    }
                    BinaryOp::Sub
                }
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_multiplicative()?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            };
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let mut left = self.parse_pow()?;
        loop {
            let op = match self.peek().map(|t| &t.kind) {
                Some(TokKind::Star) => BinaryOp::Mul,
                Some(TokKind::Slash) => BinaryOp::Div,
                Some(TokKind::SlashSlash) => BinaryOp::FloorDiv,
                Some(TokKind::Kw(Keyword::Mod)) => BinaryOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_pow()?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            };
        }
        Ok(left)
    }

    /// Right-associative exponentiation (`2 ** 3 ** 2` == `2 ** (3 ** 2)`).
    fn parse_pow(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let left = self.parse_unary()?;
        if matches!(self.peek(), Some(Token { kind: TokKind::StarStar, .. })) {
            let op_span = self.bump().unwrap().span;
            let right = self.nested(op_span, |p| p.parse_pow())?;
            let span = left.span.merge(right.span);
            return Ok(Expr {
                kind: ExprKind::Binary {
                    op: BinaryOp::Pow,
                    left: Box::new_in(left, self.alloc),
                    right: Box::new_in(right, self.alloc),
                },
                span,
            });
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        if matches!(self.peek(), Some(Token { kind: TokKind::Minus, .. })) {
            let start = self.bump().unwrap().span;
            let inner = self.nested(start, |p| p.parse_unary())?;
            let span = start.merge(inner.span);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new_in(inner, self.alloc),
                },
                span,
            });
        }
        if let Some(start) = self.eat_kw(Keyword::Not) {
            let inner = self.nested(start, |p| p.parse_unary())?;
            let span = start.merge(inner.span);
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new_in(inner, self.alloc),
                },
                span,
            });
        }
        let primary = self.parse_primary()?;
        self.parse_postfix(primary)
    }

    /// Postfix cell-path projection: `.field`, `.0`, `?.field`, `?.0`, and trailing `?`.
    fn parse_postfix(&mut self, mut base: Expr<'a>) -> Result<Expr<'a>, ShellError<'a>> {
        let mut steps = AVec::new_in(self.alloc);
        let mut end = base.span;

        while let Some(tok) = self.peek() {
            let optional = match tok.kind {
                TokKind::Dot => false,
                TokKind::QuestionDot => true,
                _ => break,
            };
            if steps.len() >= MAX_PATH_STEPS {
                return Err(self.err_budget(tok.span, "cell path exceeds maximum step count"));
            }
            let dot_span = tok.span;
            self.pos += 1;
            let Some(step) = self.try_parse_path_step(dot_span, optional) else {
                return Err(self.err(
                    self.cur_span(),
                    "expected field name or integer index in cell path",
                ));
            };
            end = step.span;
            steps.push(step);
        }

        if steps.is_empty() {
            return Ok(base);
        }

        let span = base.span.merge(end);
        // Merge directly into `BarePath` if the base is already a `BarePath` (e.g. `where a.b.0 > 5`).
        if let ExprKind::BarePath(ref mut existing) = base.kind {
            if existing.len() + steps.len() > MAX_PATH_STEPS {
                return Err(self.err_budget(span, "cell path exceeds maximum step count"));
            }
            for step in steps {
                existing.push(step);
            }
            base.span = span;
            return Ok(base);
        }

        Ok(Expr {
            kind: ExprKind::CellPath {
                base: Box::new_in(base, self.alloc),
                steps,
            },
            span,
        })
    }

    fn try_parse_path_step(&mut self, dot_span: Span, mut optional: bool) -> Option<PathStep<'a>> {
        let tok = self.peek()?;
        let (key, mut step_end) = match &tok.kind {
            TokKind::Int(n) if *n >= 0 => {
                let idx = usize::try_from(*n).ok()?;
                (PathKey::Index(idx), tok.span)
            }
            TokKind::Ident(s) | TokKind::Str(s) => (PathKey::Field(s.share()), tok.span),
            TokKind::Kw(kw) => (PathKey::Field(Rc::from_str_in(kw.as_str(), self.alloc)), tok.span),
            _ => return None,
        };
        self.pos += 1;

        // Also accept trailing `?` on a step (e.g. `$x.a.0?`).
        if matches!(self.peek(), Some(Token { kind: TokKind::Question, span: qspan, .. }) if qspan.start == step_end.end)
        {
            optional = true;
            step_end = self.tokens[self.pos].span;
            self.pos += 1;
        }

        Some(PathStep {
            key,
            optional,
            span: dot_span.merge(step_end),
        })
    }

    fn parse_primary(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let Some(tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected expression"));
        };
        let span = tok.span;

        match &tok.kind {
            TokKind::Kw(Keyword::Null) => {
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Nothing, span })
            }
            TokKind::Kw(Keyword::True) => {
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Bool(true), span })
            }
            TokKind::Kw(Keyword::False) => {
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Bool(false), span })
            }
            TokKind::Int(n) => {
                let n = *n;
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Int(n), span })
            }
            TokKind::Float(f) => {
                let f = *f;
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Float(f), span })
            }
            TokKind::Size(n) => {
                let n = *n;
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Size(n), span })
            }
            TokKind::Duration(n) => {
                let n = *n;
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Duration(n), span })
            }
            TokKind::Str(_) | TokKind::Interp(_) => self.parse_string_token(),
            TokKind::Var(name) => {
                let name = name.share();
                self.pos += 1;
                Ok(Expr { kind: ExprKind::Var(name), span })
            }
            // Bare cell-path literal `$.a.b` or `$?.a`
            TokKind::Dollar => {
                self.pos += 1;
                let mut steps = AVec::new_in(self.alloc);
                let mut end = span;
                while let Some(t) = self.peek() {
                    let optional = match t.kind {
                        TokKind::Dot => false,
                        TokKind::QuestionDot => true,
                        _ => break,
                    };
                    if steps.len() >= MAX_PATH_STEPS {
                        return Err(self.err_budget(t.span, "cell path exceeds maximum step count"));
                    }
                    let dspan = t.span;
                    self.pos += 1;
                    let Some(step) = self.try_parse_path_step(dspan, optional) else {
                        return Err(self.err(
                            self.cur_span(),
                            "expected field name or integer index after `$.`",
                        ));
                    };
                    end = step.span;
                    steps.push(step);
                }
                if steps.is_empty() {
                    return Err(self.err(span, "expected variable name or `.field` after `$`"));
                }
                Ok(Expr {
                    kind: ExprKind::BarePath(steps),
                    span: span.merge(end),
                })
            }
            TokKind::RustCell(code) => {
                let code = AString::from_str_in(code.as_str(), self.alloc);
                self.pos += 1;
                Ok(Expr { kind: ExprKind::RustCell { code }, span })
            }
            TokKind::LParen => self.parse_paren_expr(),
            TokKind::LBracket => self.parse_bracket_expr(),
            TokKind::LBrace => self.parse_brace_expr(),
            TokKind::Kw(Keyword::If) => self.parse_if(),
            TokKind::Kw(Keyword::Match) => self.parse_match(),
            TokKind::Kw(Keyword::Try) => self.parse_try(),
            TokKind::Ident(name) => {
                if name.as_ref() == "nothing" {
                    self.pos += 1;
                    Ok(Expr { kind: ExprKind::Nothing, span })
                } else if self.flags.bare_path_mode {
                    let name = name.share();
                    self.pos += 1;
                    let mut steps = AVec::new_in(self.alloc);
                    steps.push(PathStep {
                        key: PathKey::Field(name),
                        optional: false,
                        span,
                    });
                    Ok(Expr { kind: ExprKind::BarePath(steps), span })
                } else if self.flags.list_bare_words {
                    let name = name.share();
                    self.pos += 1;
                    Ok(Expr { kind: ExprKind::String(name), span })
                } else {
                    Err(self.err(span, "expected expression"))
                }
            }
            _ => Err(self.err(span, "expected expression")),
        }
    }

    fn parse_string_token(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let tok = self.bump().unwrap();
        let span = tok.span;
        match &tok.kind {
            TokKind::Str(s) => Ok(Expr { kind: ExprKind::String(s.share()), span }),
            TokKind::Interp(raw_parts) => {
                let mut parts = AVec::with_capacity_in(raw_parts.len(), self.alloc);
                for part in raw_parts.as_slice() {
                    match part {
                        LexInterpPart::Literal(s, _) => {
                            parts.push(InterpPart::Literal(s.share()));
                        }
                        LexInterpPart::Tokens(inner, ispan) => {
                            if inner.is_empty() {
                                return Err(self.err(
                                    *ispan,
                                    "empty interpolation `()` in double-quoted string; escape with `\\(`",
                                ));
                            }
                            let expr = self.nested(*ispan, |p| {
                                let mut sub = Parser::new(p.src, inner.as_slice(), p.alloc, p.depth);
                                sub.skip_newlines();
                                let e = sub.parse_pipeline()?;
                                sub.skip_newlines();
                                if let Some(extra) = sub.peek() {
                                    return Err(sub.err(
                                        extra.span,
                                        "unexpected token inside string interpolation",
                                    ));
                                }
                                Ok(e)
                            })?;
                            parts.push(InterpPart::Expr(expr));
                        }
                    }
                }
                Ok(Expr { kind: ExprKind::Interpolated(parts), span })
            }
            _ => Err(self.err(span, "expected string literal")),
        }
    }

    fn parse_paren_expr(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let open = self.bump().unwrap().span; // `(`
        let inner_flags = ParseFlags {
            bare_path_mode: self.flags.bare_path_mode,
            stop_at_lbrace: false,
            stop_at_comma: false,
            list_bare_words: false,
            in_list: false,
        };
        let mut expr = self.nested(open, |p| {
            p.with_flags(inner_flags, |p| {
                p.skip_newlines();
                let mut first = p.parse_stage()?;
                p.skip_newlines();
                // Allow multi-line pipelines inside `(...)` with `|` at start of next line too.
                if matches!(p.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
                    let pstart = first.span;
                    let mut pend = pstart;
                    let mut stages = AVec::new_in(p.alloc);
                    stages.push(first);
                    while matches!(p.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
                        p.pos += 1;
                        p.skip_newlines();
                        if stages.len() >= MAX_PIPELINE_STAGES {
                            return Err(p.err_budget(p.cur_span(), "pipeline stage limit exceeded"));
                        }
                        let st = p.parse_stage()?;
                        pend = st.span;
                        stages.push(st);
                        p.skip_newlines();
                    }
                    first = Expr {
                        kind: ExprKind::Pipeline(stages),
                        span: pstart.merge(pend),
                    };
                }
                Ok(first)
            })
        })?;
        self.skip_newlines();
        let close = self.expect_rparen()?;
        expr.span = open.merge(close);
        Ok(expr)
    }

    /// Parse `[1 2 3]` / `[1, 2, 3]` list OR `[[a b]; [1 2] [3 4]]` table literal.
    fn parse_bracket_expr(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let open = self.bump().unwrap().span; // `[`
        self.nested(open, |p| {
            p.skip_newlines();
            if matches!(p.peek(), Some(Token { kind: TokKind::RBracket, .. })) {
                let close = p.bump().unwrap().span;
                return Ok(Expr {
                    kind: ExprKind::List(AVec::new_in(p.alloc)),
                    span: open.merge(close),
                });
            }

            // Speculatively check for table literal: `[[col1 col2]; [v1 v2] ...]`
            if matches!(p.peek(), Some(Token { kind: TokKind::LBracket, .. }))
                && let Some(table) = p.try_parse_table_after_open(open)?
            {
                return Ok(table);
            }

            // Normal list literal: `[expr1 expr2 ...]` or `[expr1, expr2, ...]`
            let elem_flags = ParseFlags {
                bare_path_mode: p.flags.bare_path_mode,
                stop_at_lbrace: false,
                stop_at_comma: false,
                list_bare_words: !p.flags.bare_path_mode,
                in_list: true,
            };
            let mut items = AVec::new_in(p.alloc);
            p.with_flags(elem_flags, |p| {
                loop {
                    p.skip_newlines();
                    if matches!(p.peek(), Some(Token { kind: TokKind::RBracket, .. })) {
                        break;
                    }
                    if p.is_eof() {
                        return Err(p.err(open, "unterminated `[...]` list literal"));
                    }
                    if items.len() >= MAX_CONTAINER_ITEMS {
                        return Err(p.err_budget(p.cur_span(), "list literal item limit exceeded"));
                    }
                    let item = p.parse_expr()?;
                    items.push(item);
                    p.skip_newlines();
                    if matches!(p.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                        p.pos += 1;
                    }
                }
                Ok(())
            })?;
            let close = p.expect_rbracket()?;
            Ok(Expr {
                kind: ExprKind::List(items),
                span: open.merge(close),
            })
        })
    }

    fn try_parse_table_after_open(&mut self, open: Span) -> Result<Option<Expr<'a>>, ShellError<'a>> {
        let saved = self.pos;
        self.pos += 1; // inner `[`
        let mut headers = AVec::new_in(self.alloc);
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Some(Token { kind: TokKind::RBracket, .. })) {
                self.pos += 1;
                break;
            }
            let Some(tok) = self.peek() else {
                self.pos = saved;
                return Ok(None);
            };
            let col = match &tok.kind {
                TokKind::Ident(s) | TokKind::Str(s) => s.share(),
                TokKind::Kw(kw) => Rc::from_str_in(kw.as_str(), self.alloc),
                _ => {
                    self.pos = saved;
                    return Ok(None);
                }
            };
            if headers.len() >= MAX_CONTAINER_ITEMS {
                return Err(self.err_budget(tok.span, "table header column limit exceeded"));
            }
            headers.push(col);
            self.pos += 1;
            self.skip_newlines();
            if matches!(self.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                self.pos += 1;
            }
        }

        self.skip_newlines();
        if !matches!(self.peek(), Some(Token { kind: TokKind::Semi, .. })) {
            self.pos = saved;
            return Ok(None);
        }
        self.pos += 1; // consume `;`

        // Committed to table literal! Parse row lists `[e1 e2 ...]` until outer `]`.
        let elem_flags = ParseFlags {
            bare_path_mode: self.flags.bare_path_mode,
            stop_at_lbrace: false,
            stop_at_comma: false,
            list_bare_words: !self.flags.bare_path_mode,
            in_list: true,
        };
        let mut rows = AVec::new_in(self.alloc);
        self.with_flags(elem_flags, |p| {
            loop {
                p.skip_newlines();
                if matches!(p.peek(), Some(Token { kind: TokKind::RBracket, .. })) {
                    break;
                }
                if p.is_eof() {
                    return Err(p.err(open, "unterminated `[[...]; ...]` table literal"));
                }
                if rows.len() >= MAX_CONTAINER_ITEMS {
                    return Err(p.err_budget(p.cur_span(), "table row limit exceeded"));
                }
                if !matches!(p.peek(), Some(Token { kind: TokKind::LBracket, .. })) {
                    return Err(p.err(p.cur_span(), "expected `[` for table row"));
                }
                let row_open = p.bump().unwrap().span;
                let mut row = AVec::new_in(p.alloc);
                loop {
                    p.skip_newlines();
                    if matches!(p.peek(), Some(Token { kind: TokKind::RBracket, .. })) {
                        p.pos += 1;
                        break;
                    }
                    if p.is_eof() {
                        return Err(p.err(row_open, "unterminated table row `[...]`"));
                    }
                    if row.len() >= MAX_CONTAINER_ITEMS {
                        return Err(p.err_budget(p.cur_span(), "table row cell limit exceeded"));
                    }
                    row.push(p.parse_expr()?);
                    p.skip_newlines();
                    if matches!(p.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                        p.pos += 1;
                    }
                }
                rows.push(row);
                p.skip_newlines();
                if matches!(p.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                    p.pos += 1;
                }
            }
            Ok(())
        })?;

        let close = self.expect_rbracket()?;
        Ok(Some(Expr {
            kind: ExprKind::Table { headers, rows },
            span: open.merge(close),
        }))
    }

    /// Parse `{|x, y| ... }` closure, `{a: 1, b: 2}` record, or `{ ... }` block.
    fn parse_brace_expr(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let open = self.bump().unwrap().span; // `{`
        self.nested(open, |p| {
            p.skip_newlines();

            // Empty `{}` -> empty Record
            if matches!(p.peek(), Some(Token { kind: TokKind::RBrace, .. })) {
                let close = p.bump().unwrap().span;
                return Ok(Expr {
                    kind: ExprKind::Record(AVec::new_in(p.alloc)),
                    span: open.merge(close),
                });
            }

            // Closure: `{|x, y| body }` or `{|| body }`
            if matches!(p.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
                p.pos += 1; // opening `|`
                let mut params = AVec::new_in(p.alloc);
                loop {
                    if matches!(p.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
                        p.pos += 1; // closing `|`
                        break;
                    }
                    if p.is_eof() {
                        return Err(p.err(open, "unterminated closure parameter list `|...|`"));
                    }
                    if params.len() >= MAX_PARAMS {
                        return Err(p.err_budget(p.cur_span(), "closure parameter limit exceeded"));
                    }
                    let pname = p.parse_binding_name("expected closure parameter name")?;
                    // Optional `: TypeAnn` inside closure params `{|x: int| ...}`
                    if matches!(p.peek(), Some(Token { kind: TokKind::Colon, .. })) {
                        p.pos += 1;
                        let _ = p.parse_type_ann()?;
                    }
                    params.push(pname);
                    if matches!(p.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                        p.pos += 1;
                    }
                }
                let mut body = p.with_flags(ParseFlags::DEFAULT, |p| p.parse_block(true))?;
                let close = p.expect_rbrace()?;
                body.span = open.merge(close);
                return Ok(Expr {
                    kind: ExprKind::Closure {
                        params,
                        body: Rc::new_in(body, p.alloc),
                    },
                    span: open.merge(close),
                });
            }

            // Record: starts with `(ident | keyword | string) ':'`
            let is_record = matches!(
                (p.peek().map(|t| &t.kind), p.peek_at(1).map(|t| &t.kind)),
                (
                    Some(TokKind::Ident(_) | TokKind::Kw(_) | TokKind::Str(_)),
                    Some(TokKind::Colon),
                )
            );

            if is_record {
                let mut entries = AVec::new_in(p.alloc);
                p.with_flags(ParseFlags::DEFAULT, |p| {
                    loop {
                        p.skip_newlines();
                        if matches!(p.peek(), Some(Token { kind: TokKind::RBrace, .. })) {
                            break;
                        }
                        if p.is_eof() {
                            return Err(p.err(open, "unterminated `{...}` record literal"));
                        }
                        if entries.len() >= MAX_CONTAINER_ITEMS {
                            return Err(
                                p.err_budget(p.cur_span(), "record literal field limit exceeded")
                            );
                        }
                        let Some(key_tok) = p.peek() else {
                            return Err(p.err(p.cur_span(), "expected record key"));
                        };
                        let key = match &key_tok.kind {
                            TokKind::Ident(s) | TokKind::Str(s) => s.share(),
                            TokKind::Kw(kw) => Rc::from_str_in(kw.as_str(), p.alloc),
                            _ => return Err(p.err(key_tok.span, "expected record key")),
                        };
                        p.pos += 1;
                        if !matches!(p.peek(), Some(Token { kind: TokKind::Colon, .. })) {
                            return Err(p.err(p.cur_span(), "expected `:` after record key"));
                        }
                        p.pos += 1;
                        p.skip_newlines();
                        let val = p.parse_expr()?;
                        entries.push((key, val));
                        p.skip_newlines();
                        if matches!(p.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                            p.pos += 1;
                        }
                    }
                    Ok(())
                })?;
                let close = p.expect_rbrace()?;
                return Ok(Expr {
                    kind: ExprKind::Record(entries),
                    span: open.merge(close),
                });
            }

            // Otherwise: block expression `{ stmt1; stmt2 }`
            let mut block = p.with_flags(ParseFlags::DEFAULT, |p| p.parse_block(true))?;
            let close = p.expect_rbrace()?;
            block.span = open.merge(close);
            Ok(Expr {
                kind: ExprKind::Block(block),
                span: open.merge(close),
            })
        })
    }

    fn parse_if(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `if`
        self.nested(start, |p| {
            let cond = p.with_flags(
                ParseFlags { stop_at_lbrace: true, ..ParseFlags::DEFAULT },
                |p| p.parse_stage(),
            )?;
            let then_block = p.parse_braced_block("expected `{` after `if` condition")?;
            let mut end = then_block.span;

            // Peek past optional newlines to see if an `else` branch follows.
            let saved = p.pos;
            p.skip_newlines();
            let else_branch = if p.eat_kw(Keyword::Else).is_some() {
                p.skip_newlines();
                let eb = if p.peek_is_kw(Keyword::If) {
                    p.parse_if()?
                } else {
                    let blk = p.parse_braced_block("expected `{` or `if` after `else`")?;
                    let bspan = blk.span;
                    Expr { kind: ExprKind::Block(blk), span: bspan }
                };
                end = eb.span;
                Some(Box::new_in(eb, p.alloc))
            } else {
                p.pos = saved;
                None
            };

            Ok(Expr {
                kind: ExprKind::If {
                    cond: Box::new_in(cond, p.alloc),
                    then_block,
                    else_branch,
                },
                span: start.merge(end),
            })
        })
    }

    fn parse_match(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `match`
        self.nested(start, |p| {
            let scrutinee = p.with_flags(
                ParseFlags { stop_at_lbrace: true, ..ParseFlags::DEFAULT },
                |p| p.parse_stage(),
            )?;
            let open = p.expect_lbrace("expected `{` to start `match` arms")?;
            let mut arms = AVec::new_in(p.alloc);

            loop {
                p.skip_newlines();
                if matches!(p.peek(), Some(Token { kind: TokKind::RBrace, .. })) {
                    break;
                }
                if p.is_eof() {
                    return Err(p.err(open, "unterminated `match { ... }` expression"));
                }
                if arms.len() >= MAX_MATCH_ARMS {
                    return Err(p.err_budget(p.cur_span(), "match arm limit exceeded"));
                }
                let arm_start = p.cur_span();
                let pattern = p.parse_pattern()?;
                let guard = if p.eat_kw(Keyword::If).is_some() {
                    Some(p.with_flags(ParseFlags::DEFAULT, |p| p.parse_expr())?)
                } else {
                    None
                };
                if !matches!(p.peek(), Some(Token { kind: TokKind::FatArrow, .. })) {
                    return Err(p.err(p.cur_span(), "expected `=>` in `match` arm"));
                }
                p.pos += 1;
                p.skip_newlines();

                let body = p.with_flags(
                    ParseFlags { stop_at_comma: true, ..ParseFlags::DEFAULT },
                    |p| p.parse_pipeline(),
                )?;
                let arm_span = arm_start.merge(body.span);
                arms.push(MatchArm { pattern, guard, body, span: arm_span });

                p.skip_newlines();
                if matches!(p.peek(), Some(Token { kind: TokKind::Comma, .. })) {
                    p.pos += 1;
                }
            }

            let close = p.expect_rbrace()?;
            Ok(Expr {
                kind: ExprKind::Match {
                    scrutinee: Box::new_in(scrutinee, p.alloc),
                    arms,
                },
                span: start.merge(close),
            })
        })
    }

    fn parse_pattern(&mut self) -> Result<Pattern<'a>, ShellError<'a>> {
        let first = self.parse_pattern_atom()?;
        if !matches!(self.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
            return Ok(first);
        }
        let mut pats = AVec::new_in(self.alloc);
        pats.push(first);
        while matches!(self.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
            self.pos += 1;
            if pats.len() >= MAX_MATCH_ARMS {
                return Err(self.err_budget(self.cur_span(), "or-pattern branch limit exceeded"));
            }
            pats.push(self.parse_pattern_atom()?);
        }
        Ok(Pattern::Or(pats))
    }

    fn parse_pattern_atom(&mut self) -> Result<Pattern<'a>, ShellError<'a>> {
        let Some(tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected pattern in `match` arm"));
        };
        let span = tok.span;

        match &tok.kind {
            TokKind::Kw(Keyword::Null) => {
                self.pos += 1;
                Ok(Pattern::Literal(Expr { kind: ExprKind::Nothing, span }))
            }
            TokKind::Kw(Keyword::True) => {
                self.pos += 1;
                Ok(Pattern::Literal(Expr { kind: ExprKind::Bool(true), span }))
            }
            TokKind::Kw(Keyword::False) => {
                self.pos += 1;
                Ok(Pattern::Literal(Expr { kind: ExprKind::Bool(false), span }))
            }
            TokKind::Str(s) => {
                let s = s.share();
                self.pos += 1;
                Ok(Pattern::Literal(Expr { kind: ExprKind::String(s), span }))
            }
            TokKind::Int(n) => {
                let start_n = *n;
                self.pos += 1;
                self.finish_int_pattern(start_n, span)
            }
            TokKind::Minus => {
                let mspan = self.bump().unwrap().span;
                let neg = self.parse_negated_literal(mspan)?;
                if let ExprKind::Int(start_n) = neg.kind {
                    self.finish_int_pattern(start_n, neg.span)
                } else {
                    Ok(Pattern::Literal(neg))
                }
            }
            TokKind::Float(f) => {
                let f = *f;
                self.pos += 1;
                Ok(Pattern::Literal(Expr { kind: ExprKind::Float(f), span }))
            }
            TokKind::Size(n) => {
                let n = *n;
                self.pos += 1;
                Ok(Pattern::Literal(Expr { kind: ExprKind::Size(n), span }))
            }
            TokKind::Duration(n) => {
                let n = *n;
                self.pos += 1;
                Ok(Pattern::Literal(Expr { kind: ExprKind::Duration(n), span }))
            }
            TokKind::Var(s) => {
                let s = s.share();
                self.pos += 1;
                Ok(Pattern::Binding(s))
            }
            TokKind::Ident(s) => {
                let s = s.share();
                self.pos += 1;
                match s.as_ref() {
                    "_" => Ok(Pattern::Wildcard),
                    "nothing" => Ok(Pattern::Literal(Expr { kind: ExprKind::Nothing, span })),
                    _ => Ok(Pattern::Binding(s)),
                }
            }
            _ => Err(self.err(span, "expected pattern in `match` arm")),
        }
    }

    fn finish_int_pattern(&mut self, start: i64, start_span: Span) -> Result<Pattern<'a>, ShellError<'a>> {
        if matches!(self.peek(), Some(Token { kind: TokKind::DotDot | TokKind::DotDotLt, .. })) {
            let op = self.bump().unwrap();
            let inclusive = matches!(op.kind, TokKind::DotDot);
            let end = self.parse_signed_int_pattern()?;
            Ok(Pattern::Range { start, end, inclusive })
        } else {
            Ok(Pattern::Literal(Expr {
                kind: ExprKind::Int(start),
                span: start_span,
            }))
        }
    }

    fn parse_signed_int_pattern(&mut self) -> Result<i64, ShellError<'a>> {
        let Some(tok) = self.peek() else {
            return Err(self.err(self.cur_span(), "expected integer range bound in pattern"));
        };
        match tok.kind {
            TokKind::Int(n) => {
                self.pos += 1;
                Ok(n)
            }
            TokKind::Minus => {
                let m = self.bump().unwrap().span;
                let expr = self.parse_negated_literal(m)?;
                match expr.kind {
                    ExprKind::Int(n) => Ok(n),
                    _ => Err(self.err(expr.span, "expected integer range bound in pattern")),
                }
            }
            _ => Err(self.err(tok.span, "expected integer range bound in pattern")),
        }
    }

    fn parse_try(&mut self) -> Result<Expr<'a>, ShellError<'a>> {
        let start = self.bump().unwrap().span; // `try`
        self.nested(start, |p| {
            let try_block = p.parse_braced_block("expected `{` after `try`")?;
            let mut end = try_block.span;

            let saved = p.pos;
            p.skip_newlines();
            let (catch_var, catch_block) = if p.eat_kw(Keyword::Catch).is_some() {
                p.skip_newlines();
                let open = p.expect_lbrace("expected `{` after `catch`")?;
                p.skip_newlines();
                let mut var = None;
                if matches!(p.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
                    p.pos += 1; // opening `|`
                    if !matches!(p.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
                        let name = p.parse_binding_name("expected error variable name in `catch {|e| ...}`")?;
                        if matches!(p.peek(), Some(Token { kind: TokKind::Colon, .. })) {
                            p.pos += 1;
                            let _ = p.parse_type_ann()?;
                        }
                        var = Some(name);
                    }
                    if !matches!(p.peek(), Some(Token { kind: TokKind::Pipe, .. })) {
                        return Err(p.err(p.cur_span(), "expected `|` after `catch` binding"));
                    }
                    p.pos += 1; // closing `|`
                }
                let mut block = p.with_flags(ParseFlags::DEFAULT, |p| p.parse_block(true))?;
                let close = p.expect_rbrace()?;
                block.span = open.merge(close);
                end = block.span;
                (var, Some(block))
            } else {
                p.pos = saved;
                (None, None)
            };

            Ok(Expr {
                kind: ExprKind::Try {
                    try_block,
                    catch_var,
                    catch_block,
                },
                span: start.merge(end),
            })
        })
    }
}

/// Borrow a token as a word (identifier or keyword) for names, field keys, or flags.
fn token_word<'x>(tok: &'x Token<'_>) -> Option<&'x str> {
    match &tok.kind {
        TokKind::Ident(s) => Some(s.as_ref()),
        TokKind::Kw(kw) => Some(kw.as_str()),
        _ => None,
    }
}

/// Tokens that can be part of an unquoted bare-word command name or argument
/// (`Cargo.toml`, `./src/main.rs`, `*.rs`, `https://a.b/c?x=1`, `FOO=bar`).
fn is_bare_word_token(kind: &TokKind<'_>, stop_at_comma: bool) -> bool {
    if stop_at_comma && matches!(kind, TokKind::Comma) {
        return false;
    }
    !matches!(
        kind,
        TokKind::Pipe
            | TokKind::Semi
            | TokKind::Newline
            | TokKind::LParen
            | TokKind::RParen
            | TokKind::LBracket
            | TokKind::RBracket
            | TokKind::LBrace
            | TokKind::RBrace
            | TokKind::Str(_)
            | TokKind::Interp(_)
            | TokKind::RustCell(_)
            | TokKind::Eof
    )
}

/// Tokens that, when immediately touching a literal (e.g. `127.0.0.1`, `2026-11-24`, `true.txt`),
/// signal that the whole run is a bare path/word argument rather than a scalar literal.
fn is_bare_word_glue_token(kind: &TokKind<'_>) -> bool {
    matches!(
        kind,
        TokKind::Dot
            | TokKind::Slash
            | TokKind::Minus
            | TokKind::Ident(_)
            | TokKind::Kw(_)
            | TokKind::Star
            | TokKind::Colon
            | TokKind::At
            | TokKind::Tilde
            | TokKind::Eq
            | TokKind::Plus
            | TokKind::Question
            | TokKind::Other(_)
    )
}

fn stmt_span(stmt: &Stmt<'_>) -> Span {
    match stmt {
        Stmt::Let { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::Def { span, .. }
        | Stmt::Alias { span, .. }
        | Stmt::For { span, .. }
        | Stmt::While { span, .. }
        | Stmt::Loop { span, .. }
        | Stmt::Break(span)
        | Stmt::Continue(span)
        | Stmt::Return { span, .. } => *span,
        Stmt::Expr(e) => e.span,
    }
}

fn call_arg_span(arg: &CallArg<'_>) -> Span {
    match arg {
        CallArg::Positional(e) => e.span,
        CallArg::Flag { span, .. } => *span,
    }
}
