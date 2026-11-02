//! What rustc's server answers to `ts_to_string`: `rustc_ast_pretty::pprust::tts_to_string`,
//! that is `print_tts(stream, convert_dollar_crate: false)` on a fresh Oppen printer
//! (`print/pp.rs`), byte for byte.
//!
//! rustc's server holds glued tokens (`::`, `..=`, `'a`), where [`Stream`] holds one tree per
//! character as the bridge does. A punctuation mark that is `Joint` or `JointHidden` glues to
//! the next as rustc's `Token::glue` does, left to right, which is what rustc's lexer and its
//! `push_tree_with_gluing` made of the same characters; a `-`-prefixed number literal is a `-`
//! (`JointHidden`) and the number, as rustc's `ToInternal` splits it. The printer then sees
//! rustc's tokens, and `space_between` decides as it does.

mod pp;

use std::cell::RefCell;

use intern::Symbol;
use span::{Edition, Span};

use crate::stream::{Delimiter, DocKind, IdentKind, LitKind, Spacing, Stream, Tree};

/// rustc's `INDENT_UNIT`.
const INDENT_UNIT: isize = 4;

struct Scratch {
    printer: pp::Printer,
    /// The text of the token being printed.
    word: String,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> =
        // alloc: once per thread, grown to the largest stream printed and reused
        RefCell::new(Scratch { printer: pp::Printer::default(), word: String::new() });
}

/// Appends rustc's `tts_to_string` of `trees` to `out`. `edition` is the edition of an
/// identifier's span, asked only of `async`, `await`, `dyn`, `try` and `gen` before `!` or
/// `(`, the identifiers whose being a keyword depends on it.
pub(crate) fn to_string(trees: &[Tree], out: &mut String, edition: impl FnMut(Span) -> Edition) {
    SCRATCH.with_borrow_mut(|scratch| {
        let Scratch { printer, word } = scratch;
        printer.reset();
        let mut state = State { p: printer, word, out, edition };
        state.print_tts(trees);
        state.p.eof(state.out);
    });
}

/// rustc's token kinds for punctuation (`rustc_ast::token::TokenKind`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Op {
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
    SingleQuote,
}

impl Op {
    /// rustc's `ToInternal` of a `Punct`.
    pub(crate) fn of(ch: u8) -> Option<Op> {
        Some(match ch {
            b'=' => Op::Eq,
            b'<' => Op::Lt,
            b'>' => Op::Gt,
            b'!' => Op::Bang,
            b'~' => Op::Tilde,
            b'+' => Op::Plus,
            b'-' => Op::Minus,
            b'*' => Op::Star,
            b'/' => Op::Slash,
            b'%' => Op::Percent,
            b'^' => Op::Caret,
            b'&' => Op::And,
            b'|' => Op::Or,
            b'@' => Op::At,
            b'.' => Op::Dot,
            b',' => Op::Comma,
            b';' => Op::Semi,
            b':' => Op::Colon,
            b'#' => Op::Pound,
            b'$' => Op::Dollar,
            b'?' => Op::Question,
            b'\'' => Op::SingleQuote,
            _ => return None,
        })
    }

    /// rustc's `Token::glue` of two punctuation tokens.
    pub(crate) fn glue(self, next: Op) -> Option<Op> {
        Some(match (self, next) {
            (Op::Eq, Op::Eq) => Op::EqEq,
            (Op::Eq, Op::Gt) => Op::FatArrow,
            (Op::Lt, Op::Eq) => Op::Le,
            (Op::Lt, Op::Lt) => Op::Shl,
            (Op::Lt, Op::Le) => Op::ShlEq,
            (Op::Lt, Op::Minus) => Op::LArrow,
            (Op::Gt, Op::Eq) => Op::Ge,
            (Op::Gt, Op::Gt) => Op::Shr,
            (Op::Gt, Op::Ge) => Op::ShrEq,
            (Op::Bang, Op::Eq) => Op::Ne,
            (Op::Plus, Op::Eq) => Op::PlusEq,
            (Op::Minus, Op::Eq) => Op::MinusEq,
            (Op::Minus, Op::Gt) => Op::RArrow,
            (Op::Star, Op::Eq) => Op::StarEq,
            (Op::Slash, Op::Eq) => Op::SlashEq,
            (Op::Percent, Op::Eq) => Op::PercentEq,
            (Op::Caret, Op::Eq) => Op::CaretEq,
            (Op::And, Op::Eq) => Op::AndEq,
            (Op::And, Op::And) => Op::AndAnd,
            (Op::Or, Op::Eq) => Op::OrEq,
            (Op::Or, Op::Or) => Op::OrOr,
            (Op::Shl, Op::Eq) => Op::ShlEq,
            (Op::Shr, Op::Eq) => Op::ShrEq,
            (Op::Dot, Op::Dot) => Op::DotDot,
            (Op::Dot, Op::DotDot) => Op::DotDotDot,
            (Op::DotDot, Op::Dot) => Op::DotDotDot,
            (Op::DotDot, Op::Eq) => Op::DotDotEq,
            (Op::Colon, Op::Colon) => Op::PathSep,
            _ => return None,
        })
    }

    /// `token_kind_to_string`.
    pub(crate) fn text(self) -> &'static str {
        match self {
            Op::Eq => "=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::EqEq => "==",
            Op::Ne => "!=",
            Op::Ge => ">=",
            Op::Gt => ">",
            Op::Bang => "!",
            Op::Tilde => "~",
            Op::OrOr => "||",
            Op::AndAnd => "&&",
            Op::Plus => "+",
            Op::Minus => "-",
            Op::Star => "*",
            Op::Slash => "/",
            Op::Percent => "%",
            Op::Caret => "^",
            Op::And => "&",
            Op::Or => "|",
            Op::Shl => "<<",
            Op::Shr => ">>",
            Op::PlusEq => "+=",
            Op::MinusEq => "-=",
            Op::StarEq => "*=",
            Op::SlashEq => "/=",
            Op::PercentEq => "%=",
            Op::CaretEq => "^=",
            Op::AndEq => "&=",
            Op::OrEq => "|=",
            Op::ShlEq => "<<=",
            Op::ShrEq => ">>=",
            Op::At => "@",
            Op::Dot => ".",
            Op::DotDot => "..",
            Op::DotDotDot => "...",
            Op::DotDotEq => "..=",
            Op::Comma => ",",
            Op::Semi => ";",
            Op::Colon => ":",
            Op::PathSep => "::",
            Op::RArrow => "->",
            Op::LArrow => "<-",
            Op::FatArrow => "=>",
            Op::Pound => "#",
            Op::Dollar => "$",
            Op::Question => "?",
            Op::SingleQuote => "'",
        }
    }
}

/// One of rustc's token trees, seen through the bridge's trees.
#[derive(Clone, Copy, Debug)]
enum Tok<'t> {
    Op(Op),
    /// A punctuation byte that is none of rustc's: the bridge's `Punct::new` refuses them, so
    /// one only comes from a tree blaze made; it prints as itself and glues to nothing.
    Other(u8),
    Ident { sym: Symbol, kind: IdentKind, span: Span },
    /// `'` glued to an identifier.
    Lifetime { sym: Symbol, kind: IdentKind },
    /// `symbol` without the `-` of a number the client made negative.
    Literal { kind: LitKind, symbol: &'t str, suffix: Option<Symbol> },
    Doc { kind: DocKind, inner: bool, data: Symbol },
    Group { delimiter: Delimiter, stream: &'t Stream, open: Spacing },
}

impl Tok<'_> {
    /// `Token::is_punct`.
    fn is_punct(&self) -> bool {
        matches!(self, Tok::Op(_) | Tok::Other(_))
    }
}

/// The trees of a stream as rustc's tokens: glued, and with a negative number split.
struct Cursor<'t> {
    trees: &'t [Tree],
    index: usize,
    /// The `-` of the negative number at `index` was given already.
    minus_given: bool,
}

impl<'t> Cursor<'t> {
    fn new(trees: &'t [Tree]) -> Self {
        Cursor { trees, index: 0, minus_given: false }
    }

    /// The token at the cursor without gluing, and its spacing.
    fn peek_one(&self) -> Option<(Tok<'t>, Spacing)> {
        let tree = self.trees.get(self.index)?;
        Some(match tree {
            Tree::Punct { ch, spacing, .. } => match Op::of(*ch) {
                Some(op) => (Tok::Op(op), *spacing),
                None => (Tok::Other(*ch), *spacing),
            },
            Tree::Ident { sym, kind, span, spacing } => {
                (Tok::Ident { sym: *sym, kind: *kind, span: *span }, *spacing)
            }
            Tree::Literal { kind, symbol, suffix, spacing, .. } => {
                let text = symbol.as_str();
                match negative(*kind, text) {
                    Some(_) if !self.minus_given => (Tok::Op(Op::Minus), Spacing::JointHidden),
                    Some(number) => (Tok::Literal { kind: *kind, symbol: number, suffix: *suffix }, *spacing),
                    None => (Tok::Literal { kind: *kind, symbol: text, suffix: *suffix }, *spacing),
                }
            }
            Tree::DocComment { kind, inner, data, spacing, .. } => {
                (Tok::Doc { kind: *kind, inner: *inner, data: *data }, *spacing)
            }
            Tree::Group { delimiter, stream, open, spacing, .. } => {
                (Tok::Group { delimiter: *delimiter, stream, open: *open }, *spacing)
            }
        })
    }

    fn bump_one(&mut self) {
        let splits = match &self.trees[self.index] {
            Tree::Literal { kind, symbol, .. } => negative(*kind, symbol.as_str()).is_some(),
            _ => false,
        };
        if splits && !self.minus_given {
            self.minus_given = true;
        } else {
            self.index += 1;
            self.minus_given = false;
        }
    }

    /// The next of rustc's tokens: `try_glue_to_last`, applied while the token is joint.
    fn next(&mut self) -> Option<(Tok<'t>, Spacing)> {
        let (mut tok, mut spacing) = self.peek_one()?;
        self.bump_one();
        while spacing != Spacing::Alone
            && let Tok::Op(op) = tok
            && let Some((next, next_spacing)) = self.peek_one()
        {
            let glued = match next {
                Tok::Op(next) => match op.glue(next) {
                    Some(glued) => Tok::Op(glued),
                    None => break,
                },
                Tok::Ident { sym, kind, .. } if op == Op::SingleQuote => Tok::Lifetime { sym, kind },
                _ => break,
            };
            self.bump_one();
            tok = glued;
            spacing = next_spacing;
        }
        Some((tok, spacing))
    }
}

/// The digits of a number literal the client made negative, which rustc's `ToInternal` makes
/// a `-` and the number.
fn negative(kind: LitKind, symbol: &str) -> Option<&str> {
    match kind {
        LitKind::Integer | LitKind::Float => symbol.strip_prefix('-'),
        _ => None,
    }
}

/// `Ident::is_reserved` of `sym` (`rustc_span::symbol`, the keywords of that commit).
fn is_reserved(sym: Symbol, span: Span, edition: &mut impl FnMut(Span) -> Edition) -> bool {
    match sym.as_str() {
        // Special.
        "$crate" | "{{root}}" | "_"
        // Used, every edition.
        | "as" | "break" | "const" | "continue" | "crate" | "else" | "enum" | "extern"
        | "false" | "fn" | "for" | "if" | "impl" | "in" | "let" | "loop" | "match" | "mod"
        | "move" | "mut" | "pub" | "ref" | "return" | "self" | "Self" | "static" | "struct"
        | "super" | "trait" | "true" | "type" | "unsafe" | "use" | "where" | "while"
        // Unused, every edition.
        | "abstract" | "become" | "box" | "do" | "final" | "macro" | "override" | "priv"
        | "typeof" | "unsized" | "virtual" | "yield" => true,
        "async" | "await" | "dyn" | "try" => edition(span) >= Edition::Edition2018,
        "gen" => edition(span) >= Edition::Edition2024,
        _ => false,
    }
}

/// `space_between`: whether two tokens printed after an `Alone` one get a space.
fn space_between(tt1: &Tok<'_>, tt2: &Tok<'_>, edition: &mut impl FnMut(Span) -> Edition) -> bool {
    match (tt1, tt2) {
        // No space after line doc comments.
        (Tok::Doc { kind: DocKind::Line, .. }, _) => false,
        // `.` + NON-PUNCT: `x.y`, `tup.0`
        (Tok::Op(Op::Dot), tt2) if !tt2.is_punct() => false,
        // `$` + IDENT: `$e`
        (Tok::Op(Op::Dollar), Tok::Ident { .. }) => false,
        // NON-PUNCT + `,` `;` `.`: `foo,`, `x = 3;`, `x.y`
        (tt1, Tok::Op(Op::Comma | Op::Semi | Op::Dot)) if !tt1.is_punct() => false,
        // IDENT + `!`: `println!()`, but `if !x { ... }` needs a space after the `if`
        (Tok::Ident { sym, kind, span }, Tok::Op(Op::Bang))
            if *kind == IdentKind::Raw || !is_reserved(*sym, *span, edition) =>
        {
            false
        }
        // IDENT|`fn`|`Self`|`pub` + `(`: `f(3)`, `fn(x: u8)`, `Self()`, `pub(crate)`,
        // but `let (a, b) = (1, 2)` needs a space after the `let`
        (Tok::Ident { sym, kind, span }, Tok::Group { delimiter: Delimiter::Parenthesis, .. })
            if *kind == IdentKind::Raw
                || matches!(sym.as_str(), "fn" | "Self" | "pub")
                || !is_reserved(*sym, *span, edition) =>
        {
            false
        }
        // `#` + `[`: `#[attr]`
        (Tok::Op(Op::Pound), Tok::Group { delimiter: Delimiter::Bracket, .. }) => false,
        _ => true,
    }
}

/// `idents_would_merge`.
fn idents_would_merge(tt1: &Tok<'_>, tt2: &Tok<'_>) -> bool {
    matches!(tt1, Tok::Ident { .. }) && matches!(tt2, Tok::Ident { .. })
}

/// `token_kind_to_string` of a token that is not a group, into `word`.
fn token_text(tok: &Tok<'_>, word: &mut String) {
    word.clear();
    match *tok {
        Tok::Op(op) => word.push_str(op.text()),
        Tok::Other(ch) => word.push(ch as char),
        Tok::Ident { sym, kind, .. } => {
            word.push_str(ident_prefix(kind));
            word.push_str(sym.as_str());
        }
        Tok::Lifetime { sym, kind } => {
            // `IdentPrinter`'s `RawLifetime` writes `'r#` and the name. A forced keyword
            // lifetime is one rustc's printer has no mode for; it prints as its two tokens.
            word.push('\'');
            word.push_str(ident_prefix(kind));
            word.push_str(sym.as_str());
        }
        Tok::Literal { kind, symbol, suffix } => literal_text(kind, symbol, suffix, word),
        Tok::Doc { kind, inner, data } => {
            word.push_str(match (kind, inner) {
                (DocKind::Line, false) => "///",
                (DocKind::Line, true) => "//!",
                (DocKind::Block, false) => "/**",
                (DocKind::Block, true) => "/*!",
            });
            word.push_str(data.as_str());
            if kind == DocKind::Block {
                word.push_str("*/");
            }
        }
        Tok::Group { .. } => unreachable!("a group prints through print_delimited"),
    }
}

fn ident_prefix(kind: IdentKind) -> &'static str {
    match kind {
        IdentKind::Normal => "",
        IdentKind::Raw => "r#",
        IdentKind::ForcedKeyword => "k#",
    }
}

/// `literal_to_string`: each kind's quotes and `#`s, then the suffix.
pub(crate) fn literal_text(kind: LitKind, symbol: &str, suffix: Option<Symbol>, out: &mut String) {
    let hashes = |out: &mut String, n: u8| out.extend(std::iter::repeat_n('#', n as usize));
    match kind {
        LitKind::Byte => {
            out.push_str("b'");
            out.push_str(symbol);
            out.push('\'');
        }
        LitKind::Char => {
            out.push('\'');
            out.push_str(symbol);
            out.push('\'');
        }
        LitKind::Str => {
            out.push('"');
            out.push_str(symbol);
            out.push('"');
        }
        LitKind::StrRaw(n) | LitKind::ByteStrRaw(n) | LitKind::CStrRaw(n) => {
            out.push_str(match kind {
                LitKind::StrRaw(_) => "r",
                LitKind::ByteStrRaw(_) => "br",
                _ => "cr",
            });
            hashes(out, n);
            out.push('"');
            out.push_str(symbol);
            out.push('"');
            hashes(out, n);
        }
        LitKind::ByteStr => {
            out.push_str("b\"");
            out.push_str(symbol);
            out.push('"');
        }
        LitKind::CStr => {
            out.push_str("c\"");
            out.push_str(symbol);
            out.push('"');
        }
        LitKind::Integer | LitKind::Float | LitKind::Err => out.push_str(symbol),
    }
    if let Some(suffix) = suffix {
        out.push_str(suffix.as_str());
    }
}

struct State<'s, F> {
    p: &'s mut pp::Printer,
    word: &'s mut String,
    out: &'s mut String,
    edition: F,
}

impl<F: FnMut(Span) -> Edition> State<'_, F> {
    /// `print_tts`.
    fn print_tts(&mut self, trees: &[Tree]) {
        let mut cursor = Cursor::new(trees);
        let mut current = cursor.next();
        while let Some((tt, spacing)) = current {
            self.print_tt(&tt);
            let next = cursor.next();
            if let Some((next_tt, _)) = &next {
                if spacing == Spacing::Alone && space_between(&tt, next_tt, &mut self.edition) {
                    self.p.space();
                } else if spacing != Spacing::Alone && idents_would_merge(&tt, next_tt) {
                    self.p.space();
                }
            }
            current = next;
        }
    }

    /// `print_tt`; the spacing after it is the cursor's (a group's is its close delimiter's).
    fn print_tt(&mut self, tt: &Tok<'_>) {
        match *tt {
            Tok::Group { delimiter, stream, open } => {
                self.print_delimited(delimiter, open, stream);
            }
            _ => {
                token_text(tt, self.word);
                self.p.word(self.word, self.out);
                if let Tok::Doc { .. } = tt {
                    self.p.hardbreak();
                }
            }
        }
    }

    /// `print_mac_common` with no header, bang or ident, as `print_tt` calls it.
    fn print_delimited(&mut self, delimiter: Delimiter, open_spacing: Spacing, stream: &Stream) {
        match delimiter {
            Delimiter::Brace => {
                self.p.cbox(INDENT_UNIT);
                self.p.word("{", self.out);
                // Respect `Alone` and print a space, unless the list is empty.
                let open_space = open_spacing == Spacing::Alone && !stream.is_empty();
                if open_space {
                    self.p.space();
                }
                self.p.ibox(0);
                self.print_tts(stream.trees());
                self.p.end();
                // `bclose`: no comments to print, so only the space before `}`.
                if open_space {
                    self.p.break_offset_if_not_bol(1, -INDENT_UNIT);
                }
                self.p.word("}", self.out);
                self.p.end();
            }
            Delimiter::Parenthesis | Delimiter::Bracket | Delimiter::Invisible => {
                let (open, close) = match delimiter {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Bracket => ("[", "]"),
                    // `OpenInvisible`/`CloseInvisible` print as empty words.
                    _ => ("", ""),
                };
                self.p.word(open, self.out);
                self.p.ibox(0);
                self.print_tts(stream.trees());
                self.p.end();
                self.p.word(close, self.out);
            }
        }
    }
}

#[cfg(test)]
mod tests;
