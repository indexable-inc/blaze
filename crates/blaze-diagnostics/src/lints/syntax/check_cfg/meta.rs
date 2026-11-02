//! rustc's meta items (`MetaItem`, `MetaItemInner`) as its parser reads them from a command
//! line's `--check-cfg` and `--cfg`, or from a `cfg_select!` call's tokens: `path`,
//! `path = lit` and `path(inner, ..)`, an inner one a meta item or an unsuffixed literal.
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/attr.rs` (`parse_meta_item`,
//! `parse_meta_item_kind`, `parse_meta_item_inner`, `parse_unsuffixed_meta_item_lit`),
//! `compiler/rustc_parse/src/parser/path.rs` (`PathStyle::Mod`), the reserved identifiers of
//! `compiler/rustc_span/src/symbol.rs` (`Symbol::is_reserved`, `is_path_segment_keyword`,
//! `can_be_raw`) and `compiler/rustc_ast_pretty/src/pprust/state.rs`
//! (`meta_list_item_to_string`) (MIT or Apache-2.0).

use std::borrow::Cow;

use syntax::{Edition, LexedStr, SyntaxKind, TextRange, TextSize};

/// A token of a meta item, trivia left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Tok<'a> {
    pub(super) kind: SyntaxKind,
    pub(super) text: &'a str,
    pub(super) range: TextRange,
}

/// The tokens of `text` lexed as rustc lexes a command line's spec in `edition`, or `None` when
/// the lexer refuses some of it (an unterminated literal, an unknown character), which rustc's
/// parser of a spec takes as no meta item.
pub(super) fn lex(text: &str, edition: Edition) -> Option<Vec<Tok<'_>>> {
    let lexed = LexedStr::new(edition, text);
    if lexed.errors().next().is_some() {
        return None;
    }
    let mut tokens = Vec::with_capacity(lexed.len());
    for i in 0..lexed.len() {
        let kind = lexed.kind(i);
        if kind.is_trivia() || kind == SyntaxKind::EOF {
            continue;
        }
        if kind == SyntaxKind::ERROR {
            return None;
        }
        let range = lexed.text_range(i);
        let token = &text[range.start..range.end];
        // An escape that is not one is an error of rustc's lexer.
        if kind == SyntaxKind::STRING && token.starts_with('"') && token.ends_with('"') && token.len() > 1 {
            unescaped(&token[1..token.len() - 1])?;
        }
        let range = TextRange::new(TextSize::new(range.start as u32), TextSize::new(range.end as u32));
        tokens.push(Tok { kind, text: token, range });
    }
    Some(tokens)
}

/// A name as written: without its `r#`, and whether it had one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ident<'a> {
    pub(super) name: &'a str,
    pub(super) raw: bool,
    pub(super) range: TextRange,
}

/// The kind of a literal: rustc's `token::LitKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LitKind {
    Bool(bool),
    Byte,
    Char,
    Integer,
    Float,
    Str,
    StrRaw,
    ByteStr,
    ByteStrRaw,
    CStr,
    CStrRaw,
}

impl LitKind {
    /// As rustc's `LitKind::article` and `descr` say it: "a string", "an integer".
    pub(super) fn described(self) -> (&'static str, &'static str) {
        match self {
            LitKind::Integer => ("an", "integer"),
            LitKind::Bool(_) => ("a", "boolean"),
            LitKind::Byte => ("a", "byte"),
            LitKind::Char => ("a", "char"),
            LitKind::Float => ("a", "float"),
            LitKind::Str | LitKind::StrRaw => ("a", "string"),
            LitKind::ByteStr | LitKind::ByteStrRaw => ("a", "byte string"),
            LitKind::CStr | LitKind::CStrRaw => ("a", "C string"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Lit<'a> {
    pub(super) kind: LitKind,
    /// As written, quotes included.
    pub(super) text: &'a str,
    pub(super) range: TextRange,
}

impl<'a> Lit<'a> {
    /// What a string literal stands for (rustc's `value_str`): `None` for any other literal,
    /// or one whose escapes are not valid.
    pub(super) fn value_str(&self) -> Option<Cow<'a, str>> {
        match self.kind {
            LitKind::StrRaw => {
                let hashes = self.text[1..].bytes().take_while(|&it| it == b'#').count();
                Some(Cow::Borrowed(&self.text[2 + hashes..self.text.len() - 1 - hashes]))
            }
            LitKind::Str => unescaped(&self.text[1..self.text.len() - 1]),
            _ => None,
        }
    }
}

/// The text of a cooked string literal between its quotes, unescaped.
fn unescaped(text: &str) -> Option<Cow<'_, str>> {
    if !text.contains(['\\', '\r']) {
        return Some(Cow::Borrowed(text));
    }
    let text = text.replace("\r\n", "\n");
    let mut out = String::with_capacity(text.len());
    let mut fatal = false;
    syntax::unescape::unescape_str(&text, |_, it| match it {
        Ok(c) => out.push(c),
        Err(e) => fatal |= e.is_fatal(),
    });
    (!fatal).then_some(Cow::Owned(out))
}

/// rustc's `MetaItem`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MetaItem<'a> {
    /// The path starts with `::`.
    pub(super) global: bool,
    pub(super) path: Vec<Ident<'a>>,
    pub(super) kind: Kind<'a>,
    pub(super) range: TextRange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind<'a> {
    Word,
    List(Vec<Inner<'a>>),
    NameValue(Lit<'a>),
}

/// rustc's `MetaItemInner`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Inner<'a> {
    Meta(MetaItem<'a>),
    Lit(Lit<'a>),
}

impl<'a> MetaItem<'a> {
    /// rustc's `ident`: the name of a path of one segment.
    pub(super) fn ident(&self) -> Option<Ident<'a>> {
        match (self.global, self.path.as_slice()) {
            (false, [it]) => Some(*it),
            _ => None,
        }
    }

    /// rustc's `has_name`.
    pub(super) fn has_name(&self, name: &str) -> bool {
        self.ident().is_some_and(|it| it.name == name)
    }

    /// rustc's `meta_item_list`.
    pub(super) fn list(&self) -> Option<&[Inner<'a>]> {
        match &self.kind {
            Kind::List(it) => Some(it),
            _ => None,
        }
    }

    pub(super) fn path_range(&self) -> TextRange {
        let start = self.range.start();
        let end = self.path.last().map_or(start, |it| it.range.end());
        TextRange::new(start, end)
    }
}

impl<'a> Inner<'a> {
    pub(super) fn meta(&self) -> Option<&MetaItem<'a>> {
        match self {
            Inner::Meta(it) => Some(it),
            Inner::Lit(_) => None,
        }
    }

    pub(super) fn lit(&self) -> Option<&Lit<'a>> {
        match self {
            Inner::Lit(it) => Some(it),
            Inner::Meta(_) => None,
        }
    }

    /// rustc's `is_word`: a meta item that is a path alone.
    pub(super) fn is_word(&self) -> bool {
        self.meta().is_some_and(|it| it.kind == Kind::Word)
    }

    pub(super) fn ident(&self) -> Option<Ident<'a>> {
        self.meta()?.ident()
    }

    pub(super) fn has_name(&self, name: &str) -> bool {
        self.meta().is_some_and(|it| it.has_name(name))
    }

    pub(super) fn list(&self) -> Option<&[Inner<'a>]> {
        self.meta()?.list()
    }

    /// rustc's `boolean_literal`.
    pub(super) fn boolean_literal(&self) -> Option<bool> {
        match self.lit()?.kind {
            LitKind::Bool(it) => Some(it),
            _ => None,
        }
    }

    pub(super) fn range(&self) -> TextRange {
        match self {
            Inner::Meta(it) => it.range,
            Inner::Lit(it) => it.range,
        }
    }

    /// rustc's `pprust::meta_list_item_to_string`.
    pub(super) fn to_text(&self) -> String {
        // alloc: the text of an invalid `--check-cfg` argument, for its error
        let mut out = String::new();
        print_inner(self, &mut out);
        out
    }
}

fn print_inner(inner: &Inner<'_>, out: &mut String) {
    match inner {
        Inner::Lit(lit) => out.push_str(lit.text),
        Inner::Meta(meta) => print_meta(meta, out),
    }
}

fn print_meta(meta: &MetaItem<'_>, out: &mut String) {
    for (i, segment) in meta.path.iter().enumerate() {
        if i > 0 || meta.global {
            out.push_str("::");
        }
        if segment.raw {
            out.push_str("r#");
        }
        out.push_str(segment.name);
    }
    match &meta.kind {
        Kind::Word => {}
        Kind::NameValue(lit) => {
            out.push_str(" = ");
            out.push_str(lit.text);
        }
        Kind::List(items) => {
            out.push('(');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                print_inner(item, out);
            }
            out.push(')');
        }
    }
}

/// rustc's parser of meta items over `tokens`.
pub(super) struct Parser<'t, 'a> {
    tokens: &'t [Tok<'a>],
    at: usize,
    edition: Edition,
}

impl<'t, 'a> Parser<'t, 'a> {
    pub(super) fn new(tokens: &'t [Tok<'a>], edition: Edition) -> Parser<'t, 'a> {
        Parser { tokens, at: 0, edition }
    }

    pub(super) fn at_end(&self) -> bool {
        self.at == self.tokens.len()
    }

    fn peek(&self) -> Option<Tok<'a>> {
        self.tokens.get(self.at).copied()
    }

    fn nth(&self, n: usize) -> Option<Tok<'a>> {
        self.tokens.get(self.at + n).copied()
    }

    /// Whether the next token is `kind` and is not the start of a longer one rustc's lexer
    /// would make of it with the one right after (`=` of `==` or `=>`, `:` of `::`).
    fn at_single(&self, kind: SyntaxKind) -> bool {
        let Some(tok) = self.peek() else { return false };
        if tok.kind != kind {
            return false;
        }
        let joined = |next: SyntaxKind| {
            self.nth(1).is_some_and(|it| it.kind == next && it.range.start() == tok.range.end())
        };
        match kind {
            SyntaxKind::EQ => !joined(SyntaxKind::EQ) && !joined(SyntaxKind::R_ANGLE),
            SyntaxKind::COLON => !joined(SyntaxKind::COLON),
            _ => true,
        }
    }

    fn eat_single(&mut self, kind: SyntaxKind) -> bool {
        let at = self.at_single(kind);
        self.at += usize::from(at);
        at
    }

    /// `::`, as one token or as two `:` with nothing between them.
    fn eat_path_sep(&mut self) -> bool {
        if self.peek().is_some_and(|it| it.kind == SyntaxKind::COLON2) {
            self.at += 1;
            return true;
        }
        let (Some(a), Some(b)) = (self.peek(), self.nth(1)) else { return false };
        let sep = a.kind == SyntaxKind::COLON && b.kind == SyntaxKind::COLON && a.range.end() == b.range.start();
        if sep {
            self.at += 2;
        }
        sep
    }

    /// `=>`, the arrow of a `cfg_select!` arm.
    pub(super) fn eat_fat_arrow(&mut self) -> bool {
        if self.peek().is_some_and(|it| it.kind == SyntaxKind::FAT_ARROW) {
            self.at += 1;
            return true;
        }
        let (Some(a), Some(b)) = (self.peek(), self.nth(1)) else { return false };
        let arrow = a.kind == SyntaxKind::EQ && b.kind == SyntaxKind::R_ANGLE && a.range.end() == b.range.start();
        if arrow {
            self.at += 2;
        }
        arrow
    }

    /// `_`.
    pub(super) fn eat_underscore(&mut self) -> bool {
        let at = self.peek().is_some_and(|it| it.kind == SyntaxKind::UNDERSCORE);
        self.at += usize::from(at);
        at
    }

    /// An attribute (`#[..]`, `#![..]`), skipped.
    pub(super) fn eat_attr(&mut self) -> bool {
        if !self.peek().is_some_and(|it| it.kind == SyntaxKind::POUND) {
            return false;
        }
        let bang = usize::from(self.nth(1).is_some_and(|it| it.kind == SyntaxKind::BANG));
        if !self.nth(1 + bang).is_some_and(|it| it.kind == SyntaxKind::L_BRACK) {
            return false;
        }
        self.at += 1 + bang;
        self.skip_delimited()
    }

    /// A delimited token tree (`{ .. }`, `( .. )`, `[ .. ]`), skipped.
    pub(super) fn skip_delimited(&mut self) -> bool {
        let open = |kind| matches!(kind, SyntaxKind::L_CURLY | SyntaxKind::L_PAREN | SyntaxKind::L_BRACK);
        let close = |kind| matches!(kind, SyntaxKind::R_CURLY | SyntaxKind::R_PAREN | SyntaxKind::R_BRACK);
        if !self.peek().is_some_and(|it| open(it.kind)) {
            return false;
        }
        let mut depth = 0usize;
        while let Some(tok) = self.peek() {
            self.at += 1;
            if open(tok.kind) {
                depth += 1;
            } else if close(tok.kind) {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
        }
        false
    }

    /// The identifier of a path segment (`PathStyle::Mod`): a name, raw or not reserved, or
    /// one of the keywords a path may have (`self`, `super`, `crate`, `Self`).
    fn segment(&mut self) -> Option<Ident<'a>> {
        let tok = self.peek()?;
        let ident = ident_of(tok)?;
        if !ident.raw && is_reserved(ident.name, self.edition) && !is_path_segment_keyword(ident.name) {
            return None;
        }
        self.at += 1;
        Some(ident)
    }

    /// rustc's `parse_meta_item`.
    pub(super) fn meta_item(&mut self) -> Option<MetaItem<'a>> {
        let start = self.peek()?.range.start();
        let global = self.eat_path_sep();
        // alloc: of a command-line argument (setup) or a `cfg_select!` arm, never a tree's attribute
        let mut path = vec![self.segment()?];
        while self.eat_path_sep() {
            path.push(self.segment()?);
        }
        let kind = if self.eat_single(SyntaxKind::EQ) {
            Kind::NameValue(self.unsuffixed_lit()?)
        } else if self.at_single(SyntaxKind::L_PAREN) {
            Kind::List(self.paren_list()?)
        } else {
            Kind::Word
        };
        let end = self.tokens[self.at - 1].range.end();
        Some(MetaItem { global, path, kind, range: TextRange::new(start, end) })
    }

    /// rustc's `parse_paren_comma_seq` of inner meta items.
    fn paren_list(&mut self) -> Option<Vec<Inner<'a>>> {
        if !self.eat_single(SyntaxKind::L_PAREN) {
            return None;
        }
        // alloc: as `meta_item`'s path
        let mut items = Vec::new();
        loop {
            if self.eat_single(SyntaxKind::R_PAREN) {
                return Some(items);
            }
            items.push(self.inner()?);
            if !self.eat_single(SyntaxKind::COMMA) {
                return self.eat_single(SyntaxKind::R_PAREN).then_some(items);
            }
        }
    }

    /// rustc's `parse_meta_item_inner`.
    pub(super) fn inner(&mut self) -> Option<Inner<'a>> {
        if let Some(lit) = self.lit() {
            self.at += 1;
            return lit_unsuffixed(lit).then_some(Inner::Lit(lit));
        }
        self.meta_item().map(Inner::Meta)
    }

    /// rustc's `parse_unsuffixed_meta_item_lit`. A suffixed literal is an error of rustc's
    /// parser, which makes the whole spec no meta item.
    fn unsuffixed_lit(&mut self) -> Option<Lit<'a>> {
        let lit = self.lit()?;
        self.at += 1;
        lit_unsuffixed(lit).then_some(lit)
    }

    fn lit(&self) -> Option<Lit<'a>> {
        let tok = self.peek()?;
        let kind = match tok.kind {
            SyntaxKind::TRUE_KW if !tok.text.starts_with("r#") => LitKind::Bool(true),
            SyntaxKind::FALSE_KW if !tok.text.starts_with("r#") => LitKind::Bool(false),
            SyntaxKind::INT_NUMBER => LitKind::Integer,
            SyntaxKind::FLOAT_NUMBER => LitKind::Float,
            SyntaxKind::CHAR => LitKind::Char,
            SyntaxKind::BYTE => LitKind::Byte,
            SyntaxKind::STRING if tok.text.starts_with('r') => LitKind::StrRaw,
            SyntaxKind::STRING => LitKind::Str,
            SyntaxKind::BYTE_STRING if tok.text.starts_with("br") => LitKind::ByteStrRaw,
            SyntaxKind::BYTE_STRING => LitKind::ByteStr,
            SyntaxKind::C_STRING if tok.text.starts_with("cr") => LitKind::CStrRaw,
            SyntaxKind::C_STRING => LitKind::CStr,
            _ => return None,
        };
        Some(Lit { kind, text: tok.text, range: tok.range })
    }
}

/// The name a token is, if it is one (rustc's `Token::ident`): an identifier or a keyword,
/// raw or not.
pub(super) fn ident_of(tok: Tok<'_>) -> Option<Ident<'_>> {
    if matches!(tok.kind, SyntaxKind::TRUE_KW | SyntaxKind::FALSE_KW) && !tok.text.starts_with("r#") {
        return None;
    }
    let (name, raw) = match tok.text.strip_prefix("r#") {
        Some(name) => (name, true),
        None => (tok.text, false),
    };
    let mut chars = name.chars();
    let first = chars.next()?;
    let starts = first == '_' || unicode_ident::is_xid_start(first);
    if !starts || !chars.all(unicode_ident::is_xid_continue) || name == "_" {
        return None;
    }
    Some(Ident { name, raw, range: tok.range })
}

/// Whether a literal has no suffix: `1`, not `1u8`; `"a"`, not `"a"x`.
fn lit_unsuffixed(lit: Lit<'_>) -> bool {
    let text = lit.text;
    match lit.kind {
        LitKind::Bool(_) => true,
        LitKind::Str | LitKind::ByteStr | LitKind::CStr => text.ends_with('"'),
        LitKind::StrRaw | LitKind::ByteStrRaw | LitKind::CStrRaw => text.ends_with(['"', '#']),
        LitKind::Char | LitKind::Byte => text.ends_with('\''),
        LitKind::Integer => {
            let (digits, hex) = match text.get(..2) {
                Some("0x") => (&text[2..], true),
                Some("0o" | "0b") => (&text[2..], false),
                _ => (text, false),
            };
            digits.bytes().all(|it| it == b'_' || it.is_ascii_digit() || (hex && it.is_ascii_hexdigit()))
        }
        LitKind::Float => {
            let mut bytes = text.bytes().peekable();
            let mut exponent = false;
            while let Some(it) = bytes.next() {
                match it {
                    b'0'..=b'9' | b'_' | b'.' => {}
                    b'e' | b'E' if !exponent => {
                        exponent = true;
                        if bytes.peek().is_some_and(|it| matches!(it, b'+' | b'-')) {
                            bytes.next();
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
    }
}

/// rustc's `Symbol::is_path_segment_keyword` of a name as written.
pub(super) fn is_path_segment_keyword(name: &str) -> bool {
    matches!(name, "super" | "self" | "Self" | "crate")
}

/// rustc's `Symbol::is_reserved` in `edition`.
pub(super) fn is_reserved(name: &str, edition: Edition) -> bool {
    const ALWAYS: &[&str] = &[
        "_", "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for", "if",
        "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
        "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "abstract",
        "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield",
    ];
    ALWAYS.contains(&name)
        || (matches!(name, "async" | "await" | "dyn" | "try") && edition >= Edition::Edition2018)
        || (name == "gen" && edition >= Edition::Edition2024)
}

/// A name as rustc's `Ident` shows it in `edition`: with `r#` when it needs one
/// (`is_raw_guess`).
pub(super) fn shown(name: &str, edition: Edition) -> Cow<'_, str> {
    let can_be_raw = !name.is_empty() && name != "_" && !is_path_segment_keyword(name);
    match can_be_raw && is_reserved(name, edition) {
        // alloc: a diagnostic's text, a reserved name shown raw
        true => Cow::Owned(format!("r#{name}")),
        false => Cow::Borrowed(name),
    }
}
