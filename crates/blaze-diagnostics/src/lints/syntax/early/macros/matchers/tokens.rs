//! A macro definition's tokens as rustc's lexer gives them: token trees of the syntax tree's
//! tokens, the comments that are not doc comments dropped, punctuation written with nothing
//! between glued as rustc glues it (`=>`, `::`, `||`, `*=`), each token with the edition and
//! context of its span.
//!
//! Follows rustc's `compiler/rustc_parse/src/lexer/tokentrees.rs` (`bump`: a token is glued to
//! the next unless whitespace or a comment is between), `compiler/rustc_parse/src/lexer/mod.rs`
//! (`next_token_from_cursor`: what is a token) and `compiler/rustc_ast/src/token.rs`
//! (`Token::glue`) (MIT or Apache-2.0).

use span::Edition;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Delim {
    Paren,
    Brace,
    Bracket,
}

/// What rustc's macro machinery tells literals apart by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lit {
    /// `"..."` with no suffix: rustc's `LitKind::Str`.
    Str,
    /// An integer with no suffix that fits a `usize`.
    Int,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Punct {
    Eq,
    Lt,
    Le,
    EqEq,
    Ne,
    Ge,
    Gt,
    AndAnd,
    OrOr,
    Bang,
    Tilde,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    And,
    Or,
    Shl,
    Shr,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    CaretEq,
    AndEq,
    OrEq,
    ShlEq,
    ShrEq,
    At,
    Dot,
    DotDot,
    DotDotDot,
    DotDotEq,
    Comma,
    Semi,
    Colon,
    PathSep,
    RArrow,
    LArrow,
    FatArrow,
    Pound,
    Dollar,
    Question,
}

impl Punct {
    fn of(c: char) -> Option<Punct> {
        Some(match c {
            '=' => Punct::Eq,
            '<' => Punct::Lt,
            '>' => Punct::Gt,
            '!' => Punct::Bang,
            '~' => Punct::Tilde,
            '+' => Punct::Plus,
            '-' => Punct::Minus,
            '*' => Punct::Star,
            '/' => Punct::Slash,
            '%' => Punct::Percent,
            '^' => Punct::Caret,
            '&' => Punct::And,
            '|' => Punct::Or,
            '@' => Punct::At,
            '.' => Punct::Dot,
            ',' => Punct::Comma,
            ';' => Punct::Semi,
            ':' => Punct::Colon,
            '#' => Punct::Pound,
            '$' => Punct::Dollar,
            '?' => Punct::Question,
            _ => return None,
        })
    }

    /// rustc's `Token::glue`: what `self` written right before `next` lexes as.
    fn glue(self, next: Punct) -> Option<Punct> {
        use Punct::*;
        Some(match (self, next) {
            (Eq, Eq) => EqEq,
            (Eq, Gt) => FatArrow,
            (Lt, Eq) => Le,
            (Lt, Lt) => Shl,
            (Lt, Le) => ShlEq,
            (Lt, Minus) => LArrow,
            (Gt, Eq) => Ge,
            (Gt, Gt) => Shr,
            (Gt, Ge) => ShrEq,
            (Bang, Eq) => Ne,
            (Plus, Eq) => PlusEq,
            (Minus, Eq) => MinusEq,
            (Minus, Gt) => RArrow,
            (Star, Eq) => StarEq,
            (Slash, Eq) => SlashEq,
            (Percent, Eq) => PercentEq,
            (Caret, Eq) => CaretEq,
            (And, Eq) => AndEq,
            (And, And) => AndAnd,
            (Or, Eq) => OrEq,
            (Or, Or) => OrOr,
            (Shl, Eq) => ShlEq,
            (Shr, Eq) => ShrEq,
            (Dot, Dot) => DotDot,
            (Dot, DotDot) => DotDotDot,
            (DotDot, Dot) => DotDotDot,
            (DotDot, Eq) => DotDotEq,
            (Colon, Colon) => PathSep,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// An identifier or a keyword, `_`, `true` and `false` included (rustc's `Ident`); `raw`
    /// when written `r#`.
    Ident { raw: bool },
    Lifetime,
    Literal(Lit),
    DocComment,
    Punct(Punct),
    /// A group's delimiters, as the follow-set check makes tokens of them.
    Open(Delim),
    Close(Delim),
    /// What has no token of rustc's here (an error).
    Other,
}

/// A token, where it is written and the edition and context of its span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Token {
    pub(super) kind: Kind,
    pub(super) range: TextRange,
    /// rustc's `span.edition()`.
    pub(super) edition: Edition,
    /// Whether its span has the root context: not of a macro's making.
    pub(super) root: bool,
}

const _: () = assert!(size_of::<Token>() <= 16);

impl Token {
    pub(super) fn is(&self, punct: Punct) -> bool {
        self.kind == Kind::Punct(punct)
    }
}

/// A run of an arena: the children of a group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Ids {
    pub(super) start: u32,
    pub(super) end: u32,
}

impl Ids {
    pub(super) fn iter(self) -> std::ops::Range<u32> {
        self.start..self.end
    }

    pub(super) fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    /// What is left after the first `n`.
    pub(super) fn skip(self, n: usize) -> Ids {
        Ids { start: (self.start + n as u32).min(self.end), end: self.end }
    }

    fn of<T>(arena: &[T], start: usize) -> Ids {
        Ids { start: start as u32, end: arena.len() as u32 }
    }
}

/// A delimited group of token trees.
#[derive(Clone, Copy, Debug)]
pub(super) struct Group {
    pub(super) delim: Delim,
    pub(super) open: Token,
    pub(super) close: Token,
    pub(super) tts: Ids,
}

impl Group {
    pub(super) fn range(&self) -> TextRange {
        TextRange::new(self.open.range.start(), self.close.range.end())
    }
}

/// rustc's `tokenstream::TokenTree`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Tt {
    Token(Token),
    Delimited(Group),
}

impl Tt {
    pub(super) fn range(&self) -> TextRange {
        match self {
            Tt::Token(it) => it.range,
            Tt::Delimited(it) => it.range(),
        }
    }
}

/// The tokens of a macro definition's body (`ast::MacroDef::body`'s tokens), as rustc's
/// parser hands them to `compile_declarative_macro` and to the early lint pass.
pub(super) struct Tokens<'t> {
    /// The definition's text, from `base` on.
    text: &'t str,
    base: TextSize,
    tts: Vec<Tt>,
    /// The body: in braces the rules; for `macro m(..) { .. }`, `(..) => { .. }`.
    pub(super) top: Ids,
}

impl<'t> Tokens<'t> {
    /// The tokens of `def`; `span` gives the edition of the span at an offset and whether it
    /// has the root context.
    pub(super) fn of(def: ast::Macro<'t>, span: &impl Fn(TextSize) -> (Edition, bool)) -> Option<Tokens<'t>> {
        let node = def.syntax();
        let mut tokens =
            // alloc: once per macro definition, only when a lint of it (allowed by default) may fire
            Tokens { text: node.text(), base: node.text_range().start(), tts: Vec::new(), top: Ids::default() };
        // alloc: once per macro definition, only when a lint of it (allowed by default) may fire
        let mut scratch = Vec::new();
        match def {
            ast::Macro::MacroRules(it) => tokens.top = tokens.group(it.token_tree()?, span, &mut scratch).tts,
            ast::Macro::MacroDef(it) => {
                let body = it.body()?;
                match it.args() {
                    None => tokens.top = tokens.group(body, span, &mut scratch).tts,
                    // rustc's `parse_item_decl_macro`: `macro m(..) { .. }` is the one rule
                    // `(..) => { .. }`.
                    Some(args) => {
                        let args = tokens.group(args, span, &mut scratch);
                        let body = tokens.group(body, span, &mut scratch);
                        let between = TextRange::new(args.close.range.end(), body.open.range.start());
                        let (edition, root) = span(between.start());
                        let arrow = Token { kind: Kind::Punct(Punct::FatArrow), range: between, edition, root };
                        let start = tokens.tts.len();
                        tokens.tts.extend([Tt::Delimited(args), Tt::Token(arrow), Tt::Delimited(body)]);
                        tokens.top = Ids::of(&tokens.tts, start);
                    }
                }
            }
        }
        Some(tokens)
    }

    pub(super) fn get(&self, ids: Ids) -> &[Tt] {
        &self.tts[ids.start as usize..ids.end as usize]
    }

    pub(super) fn text(&self, range: TextRange) -> &'t str {
        let start = usize::from(range.start() - self.base);
        let end = usize::from(range.end() - self.base);
        self.text.get(start..end).unwrap_or("")
    }

    /// The name of an identifier: rustc's symbol, without `r#`.
    pub(super) fn name(&self, range: TextRange) -> &'t str {
        let text = self.text(range);
        text.strip_prefix("r#").unwrap_or(text)
    }

    /// Whether `tt` is the identifier `word`, not written `r#` (rustc's `is_keyword`).
    pub(super) fn is_word(&self, tt: &Tt, word: &str) -> bool {
        matches!(tt, Tt::Token(it) if it.kind == Kind::Ident { raw: false } && self.text(it.range) == word)
    }

    /// The group `tt` is: its children appended to the arena after those of the groups in
    /// it, so that each group's are one run.
    fn group(
        &mut self,
        tt: ast::TokenTree<'_>,
        span: &impl Fn(TextSize) -> (Edition, bool),
        scratch: &mut Vec<Tt>,
    ) -> Group {
        let token = |kind, range: TextRange| {
            let (edition, root) = span(range.start());
            Token { kind, range, edition, root }
        };
        let range = tt.syntax().text_range();
        let mut delim = Delim::Paren;
        let (mut open, mut close) = (None, None);
        let mark = scratch.len();
        // Whether the last token pushed is punctuation with nothing after it yet.
        let mut glue = false;
        for child in tt.syntax().children_with_tokens() {
            let t: ast::Token<'_> = match child {
                NodeOrToken::Node(node) => {
                    glue = false;
                    if let Some(inner) = ast::TokenTree::cast(node) {
                        let inner = self.group(inner, span, scratch);
                        scratch.push(Tt::Delimited(inner));
                    }
                    continue;
                }
                NodeOrToken::Token(it) => it,
            };
            let kind = t.kind();
            let at = t.text_range();
            if kind.is_l_delimiter() && at.start() == range.start() {
                delim = delim_of(kind);
                open = Some(token(Kind::Open(delim), at));
                continue;
            }
            if kind.is_r_delimiter() && at.end() == range.end() {
                close = Some(token(Kind::Close(delim_of(kind)), at));
                continue;
            }
            if kind.is_trivia() {
                glue = false;
                continue;
            }
            if kind.is_punct() && kind != SyntaxKind::UNDERSCORE {
                for (offset, c) in t.text().char_indices() {
                    let at = TextRange::at(at.start() + TextSize::from(offset as u32), TextSize::of(c));
                    let Some(punct) = Punct::of(c) else {
                        scratch.push(Tt::Token(token(Kind::Other, at)));
                        glue = false;
                        continue;
                    };
                    if glue
                        && let Some(Tt::Token(last)) = scratch.last_mut()
                        && let Kind::Punct(before) = last.kind
                        && let Some(glued) = before.glue(punct)
                    {
                        last.kind = Kind::Punct(glued);
                        last.range = last.range.cover(at);
                    } else {
                        scratch.push(Tt::Token(token(Kind::Punct(punct), at)));
                    }
                    glue = true;
                }
                continue;
            }
            glue = false;
            let kind = match kind {
                SyntaxKind::OUTER_DOC_COMMENT | SyntaxKind::INNER_DOC_COMMENT => Kind::DocComment,
                SyntaxKind::LIFETIME_IDENT => Kind::Lifetime,
                SyntaxKind::UNDERSCORE => Kind::Ident { raw: false },
                _ if kind.is_any_identifier() => Kind::Ident { raw: t.text().starts_with("r#") },
                _ if kind.is_literal() => Kind::Literal(lit_of(kind, t.text())),
                _ => Kind::Other,
            };
            scratch.push(Tt::Token(token(kind, at)));
        }
        let open = open.unwrap_or_else(|| token(Kind::Open(delim), TextRange::empty(range.start())));
        let close = close.unwrap_or_else(|| {
            // A macro file's span map has no piece at its end: the context is its last byte's.
            let (edition, root) = span(range.end().checked_sub(TextSize::from(1)).filter(|_| !range.is_empty()).unwrap_or(range.start()));
            Token { kind: Kind::Close(delim), range: TextRange::empty(range.end()), edition, root }
        });
        let start = self.tts.len();
        self.tts.extend(scratch.drain(mark..));
        Group { delim, open, close, tts: Ids::of(&self.tts, start) }
    }
}

fn delim_of(kind: SyntaxKind) -> Delim {
    match kind {
        SyntaxKind::L_CURLY | SyntaxKind::R_CURLY => Delim::Brace,
        SyntaxKind::L_BRACK | SyntaxKind::R_BRACK => Delim::Bracket,
        _ => Delim::Paren,
    }
}

fn lit_of(kind: SyntaxKind, text: &str) -> Lit {
    match kind {
        SyntaxKind::STRING if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') => Lit::Str,
        SyntaxKind::INT_NUMBER if unsuffixed_usize(text) => Lit::Int,
        _ => Lit::Other,
    }
}

/// Whether `text` is an integer literal with no suffix whose value fits a `usize`.
fn unsuffixed_usize(text: &str) -> bool {
    let (digits, radix) = match text.get(..2) {
        Some("0x") => (&text[2..], 16),
        Some("0o") => (&text[2..], 8),
        Some("0b") => (&text[2..], 2),
        _ => (text, 10),
    };
    let mut value: usize = 0;
    let mut any = false;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        let Some(digit) = c.to_digit(radix) else { return false };
        let Some(next) = value.checked_mul(radix as usize).and_then(|it| it.checked_add(digit as usize)) else {
            return false;
        };
        value = next;
        any = true;
    }
    any
}

#[cfg(test)]
mod tests {
    use syntax::{Parse, SourceFile};

    use super::*;

    fn parse(text: &str) -> Parse<SourceFile> {
        SourceFile::parse(text, Edition::Edition2021)
    }

    fn tokens(parse: &Parse<SourceFile>) -> Tokens<'_> {
        let def = parse.root().descendants().find_map(ast::Macro::cast).unwrap();
        Tokens::of(def, &|_| (Edition::Edition2021, true)).unwrap()
    }

    fn kinds(tokens: &Tokens<'_>, ids: Ids) -> Vec<String> {
        tokens
            .get(ids)
            .iter()
            .map(|it| match it {
                Tt::Token(t) => format!("{:?}", t.kind),
                Tt::Delimited(g) => format!("{:?}[{}]", g.delim, kinds(tokens, g.tts).join(" ")),
            })
            .collect()
    }

    #[test]
    fn punctuation_written_together_is_glued() {
        let parse = parse("macro_rules! m { ($a:tt :: $b:tt || =>= $(x)*= ) => {} }");
        let tokens = tokens(&parse);
        let top = kinds(&tokens, tokens.top);
        assert_eq!(top[1], "Punct(FatArrow)");
        let Tt::Delimited(lhs) = tokens.get(tokens.top)[0] else { panic!() };
        let lhs = kinds(&tokens, lhs.tts);
        assert_eq!(
            lhs,
            [
                "Punct(Dollar)",
                "Ident { raw: false }",
                "Punct(Colon)",
                "Ident { raw: false }",
                "Punct(PathSep)",
                "Punct(Dollar)",
                "Ident { raw: false }",
                "Punct(Colon)",
                "Ident { raw: false }",
                "Punct(OrOr)",
                "Punct(FatArrow)",
                "Punct(Eq)",
                "Punct(Dollar)",
                "Paren[Ident { raw: false }]",
                "Punct(StarEq)",
            ]
        );
    }

    #[test]
    fn space_and_comments_keep_punctuation_apart() {
        let parse = parse("macro_rules! m { (: : |/**/| /// doc\n) => {} }");
        let tokens = tokens(&parse);
        let Tt::Delimited(lhs) = tokens.get(tokens.top)[0] else { panic!() };
        assert_eq!(
            kinds(&tokens, lhs.tts),
            ["Punct(Colon)", "Punct(Colon)", "Punct(Or)", "Punct(Or)", "DocComment"]
        );
    }

    #[test]
    fn a_macro_with_parameters_is_one_rule() {
        let parse = parse("macro m($e:expr) { $e }");
        let tokens = tokens(&parse);
        let top = kinds(&tokens, tokens.top);
        assert_eq!(top.len(), 3);
        assert_eq!(top[1], "Punct(FatArrow)");
        assert!(top[0].starts_with("Paren[") && top[2].starts_with("Brace["));
    }

    #[test]
    fn names_and_literals_are_told_apart() {
        let parse = parse(r#"macro_rules! m { (r#expr _ 'a "s" "s"x 0x1f 1u8) => {} }"#);
        let tokens = tokens(&parse);
        let Tt::Delimited(lhs) = tokens.get(tokens.top)[0] else { panic!() };
        let tts = tokens.get(lhs.tts);
        let Tt::Token(raw) = tts[0] else { panic!() };
        assert_eq!(raw.kind, Kind::Ident { raw: true });
        assert_eq!(tokens.name(raw.range), "expr");
        assert_eq!(
            kinds(&tokens, lhs.tts)[1..],
            [
                "Ident { raw: false }",
                "Lifetime",
                "Literal(Str)",
                "Literal(Other)",
                "Literal(Int)",
                "Literal(Other)",
            ]
        );
    }
}
