//! Highlighting from blaze's own lexer: a small fixed set of [`Class`]es read from the tokens
//! and their neighbours (an identifier before `(` is a function, before `!` a macro), and the
//! header of the item a finding is in (`fn main`), for the frame's context.

use blaze_lex::{Cpu, FrontmatterAllowed, LiteralKind, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};

use super::text::Style;

/// What a piece of code is, for its colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Class {
    Plain,
    Keyword,
    Type,
    Function,
    Macro,
    Str,
    Number,
    Comment,
    Lifetime,
    Attribute,
    /// A stand-in for a control character or a byte that is not UTF-8.
    Escape,
    /// In a message: code (between backticks), and the backticks themselves.
    Code,
    Tick,
}

impl Class {
    /// Its look in the terminal's own 16 colours: few colours; keywords and types carry the
    /// structure, comments recede, most code is plain.
    pub(crate) const fn style(self) -> Style {
        match self {
            Class::Plain => Style::PLAIN,
            Class::Keyword => Style::fg(5),
            Class::Type => Style::fg(3),
            Class::Function => Style::fg(4),
            Class::Macro => Style::fg(6),
            Class::Str => Style::fg(2),
            Class::Number => Style::fg(3),
            Class::Comment => Style { italic: true, ..Style::DIM },
            Class::Lifetime => Style { italic: true, ..Style::fg(6) },
            Class::Attribute => Style::DIM,
            Class::Escape => Style { inverse: true, ..Style::PLAIN },
            Class::Code => Style::BOLD,
            Class::Tick => Style::DIM,
        }
    }
}

const KEYWORDS: &[&str] = &[
    "Self", "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "type", "union", "unsafe", "use", "where",
    "while", "yield",
];

const PRIMITIVES: &[&str] = &[
    "bool", "char", "f16", "f32", "f64", "f128", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8", "u16",
    "u32", "u64", "u128", "usize",
];

fn trivia(kind: TokenKind) -> bool {
    matches!(kind, TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. })
}

/// The longest UTF-8 prefix of `text`: what the lexer reads; the rest is drawn plain.
fn valid(text: &[u8]) -> &str {
    match core::str::from_utf8(text) {
        Ok(text) => text,
        Err(error) => core::str::from_utf8(&text[..error.valid_up_to()]).unwrap_or(""),
    }
}

/// The classes of the bytes `lo..hi` of a file: sorted, disjoint `(start, end, class)` runs;
/// a byte in none is plain.
pub(crate) struct Classes<'a> {
    runs: AVec<'a, (u32, u32, Class)>,
}

/// A file kind's highlighting: the classes of a range of its bytes, and the header of the item
/// a finding is in. A kind is a unit struct implementing this and a line in [`highlighter`].
pub(crate) trait Highlighter {
    /// The runs of `lo..hi`, appended to `runs` sorted and disjoint (they may reach outside).
    fn runs(&self, text: &[u8], lo: u32, hi: u32, runs: &mut AVec<'_, (u32, u32, Class)>);

    /// The header of the item starting at `item`, when the kind has items.
    fn head<'t>(&self, text: &'t [u8], item: u32) -> Option<Head<'t>> {
        let _ = (text, item);
        None
    }
}

/// Rust, from blaze-lex's tokens.
pub(crate) struct Rust;
/// TOML (a manifest), from a scan of its lines.
pub(crate) struct Toml;
/// Any other file: plain text in the same frame.
pub(crate) struct Plain;

/// The highlighter for the file shown as `path`, by its name.
pub(crate) fn highlighter(path: &str) -> &'static dyn Highlighter {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.ends_with(".rs") {
        &Rust
    } else if name.ends_with(".toml") || name == "Cargo.lock" {
        &Toml
    } else {
        &Plain
    }
}

impl Highlighter for Plain {
    fn runs(&self, _: &[u8], _: u32, _: u32, _: &mut AVec<'_, (u32, u32, Class)>) {}
}

impl Highlighter for Rust {
    fn runs(&self, text: &[u8], lo: u32, hi: u32, runs: &mut AVec<'_, (u32, u32, Class)>) {
        rust_runs(text, lo, hi, runs);
    }

    fn head<'t>(&self, text: &'t [u8], item: u32) -> Option<Head<'t>> {
        head(text, item)
    }
}

impl Highlighter for Toml {
    /// Line by line from the line holding `lo`: a multi-line string opened above it is the one
    /// case drawn wrong, as plain text.
    fn runs(&self, text: &[u8], lo: u32, hi: u32, runs: &mut AVec<'_, (u32, u32, Class)>) {
        let from = text[..lo as usize].iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
        let hi = (hi as usize).min(text.len());
        let mut at = from;
        // Whether the line so far is before its `=` (a key) or after it (a value).
        let mut key_side = true;
        let mut header = false;
        let mut depth = 0u32;
        while at < hi {
            let byte = text[at];
            let start = at as u32;
            let run = |runs: &mut AVec<'_, (u32, u32, Class)>, end: usize, class: Class| {
                runs.push((start, end as u32, class));
                end
            };
            at = match byte {
                b'\n' => {
                    key_side = true;
                    header = false;
                    at + 1
                }
                b'#' => {
                    let end = text[at..].iter().position(|&b| b == b'\n').map_or(text.len(), |it| at + it);
                    run(runs, end, Class::Comment)
                }
                b'[' if key_side && depth == 0 && line_start(text, at) => {
                    header = true;
                    let end = text[at..].iter().position(|&b| b == b'\n' || b == b'#').map_or(text.len(), |it| at + it);
                    let close = text[at..end].iter().rposition(|&b| b == b']').map_or(end, |it| at + it + 1);
                    run(runs, close, Class::Type)
                }
                b'"' | b'\'' => {
                    let triple = text[at..].starts_with(if byte == b'"' { b"\"\"\"" } else { b"'''" });
                    let end = string_end(text, at, byte, triple);
                    run(runs, end, if key_side && !header && depth == 0 { Class::Function } else { Class::Str })
                }
                b'=' if depth == 0 => {
                    key_side = false;
                    at + 1
                }
                b'[' | b'{' => {
                    depth += 1;
                    at + 1
                }
                b']' | b'}' => {
                    depth = depth.saturating_sub(1);
                    at + 1
                }
                b',' if depth > 0 => {
                    // In an inline table, a key comes after a comma.
                    key_side = text[..at].iter().rposition(|&b| b == b'{' || b == b'[').is_some_and(|it| text[it] == b'{');
                    at + 1
                }
                _ if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'+' => {
                    let end = text[at..]
                        .iter()
                        .position(|&b| !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+' | b'.' | b':')))
                        .map_or(text.len(), |it| at + it);
                    let word = &text[at..end];
                    let class = if key_side {
                        Class::Function
                    } else if word == b"true" || word == b"false" {
                        Class::Keyword
                    } else if word[0].is_ascii_digit() || word[0] == b'+' || word[0] == b'-' || word == b"inf" || word == b"nan" {
                        Class::Number
                    } else {
                        Class::Plain
                    };
                    // A dotted key's dots stay plain: the key's words are runs of their own.
                    let end = if key_side { at + word.iter().position(|&b| b == b'.').unwrap_or(word.len()) } else { end };
                    let end = end.max(at + 1);
                    if class == Class::Plain { end } else { run(runs, end, class) }
                }
                _ => at + 1,
            };
            if byte == b'{' && !key_side {
                key_side = true;
            }
        }
    }
}

fn line_start(text: &[u8], at: usize) -> bool {
    text[..at].iter().rev().take_while(|&&b| b != b'\n').all(|&b| b == b' ' || b == b'\t')
}

/// Past the string opening at `at` with `quote` (`"` with escapes, `'` literal).
fn string_end(text: &[u8], at: usize, quote: u8, triple: bool) -> usize {
    let open = if triple { 3 } else { 1 };
    let mut i = at + open;
    while i < text.len() {
        let b = text[i];
        if quote == b'"' && b == b'\\' {
            i += 2;
            continue;
        }
        if !triple && b == b'\n' {
            return i;
        }
        if b == quote && (!triple || text[i..].starts_with(&[quote; 3])) {
            return i + open;
        }
        i += 1;
    }
    text.len()
}

impl<'a> Classes<'a> {
    /// `kind`'s classes of the bytes `lo..hi`.
    pub(crate) fn of(kind: &dyn Highlighter, text: &[u8], lo: u32, hi: u32, alloc: Alloc<'a>) -> Classes<'a> {
        let mut runs = AVec::new_in(alloc);
        kind.runs(text, lo, hi, &mut runs);
        Classes { runs }
    }

    /// The class of the byte at `byte`.
    pub(crate) fn at(&self, byte: u32) -> Class {
        let at = self.runs.partition_point(|run| run.1 <= byte);
        match self.runs.get(at) {
            Some(&(start, _, class)) if start <= byte => class,
            _ => Class::Plain,
        }
    }
}

/// Lexed from the file's start (a string or comment may open far above `lo`), keeping the
/// tokens that end past `lo`, up to the first that starts at or past `hi`.
fn rust_runs(text: &[u8], lo: u32, hi: u32, runs: &mut AVec<'_, (u32, u32, Class)>) {
    {
        let source = valid(text);
        let mut tokens: AVec<'_, (u32, u32, TokenKind)> = AVec::new_in(*runs.allocator());
        let mut at = 0u32;
        for token in tokenize(source, FrontmatterAllowed::Yes, Cpu::BASELINE) {
            let end = at + token.len;
            if at >= hi {
                break;
            }
            if end > lo {
                tokens.push((at, end, token.kind));
            }
            at = end;
        }
        let next_significant = |from: usize| (from..tokens.len()).find(|&it| !trivia(tokens[it].2));
        let mut attribute: Option<u32> = None;
        let mut macro_bang = usize::MAX;
        let mut after_fn = false;
        for i in 0..tokens.len() {
            let (start, end, kind) = tokens[i];
            if let Some(depth) = attribute.as_mut() {
                match kind {
                    TokenKind::OpenBracket => *depth += 1,
                    TokenKind::CloseBracket => *depth = depth.saturating_sub(1),
                    _ => {}
                }
                if *depth == 0 && kind == TokenKind::CloseBracket {
                    attribute = None;
                }
                runs.push((start, end, Class::Attribute));
                continue;
            }
            let class = match kind {
                TokenKind::Whitespace => continue,
                TokenKind::LineComment { .. } | TokenKind::BlockComment { .. } => Class::Comment,
                TokenKind::Literal { kind, .. } => match kind {
                    LiteralKind::Int { .. } | LiteralKind::Float { .. } => Class::Number,
                    _ => Class::Str,
                },
                TokenKind::Lifetime { .. } | TokenKind::RawLifetime => Class::Lifetime,
                TokenKind::Pound => {
                    let next = next_significant(i + 1);
                    let opens = match next.map(|it| tokens[it].2) {
                        Some(TokenKind::OpenBracket) => true,
                        Some(TokenKind::Bang) => {
                            next.and_then(|it| next_significant(it + 1)).map(|it| tokens[it].2)
                                == Some(TokenKind::OpenBracket)
                        }
                        _ => false,
                    };
                    if opens {
                        attribute = Some(0);
                        Class::Attribute
                    } else {
                        Class::Plain
                    }
                }
                TokenKind::Bang if i == macro_bang => Class::Macro,
                TokenKind::Ident | TokenKind::RawIdent => {
                    let word = &source[start as usize..end as usize];
                    let next = next_significant(i + 1);
                    let next_kind = next.map(|it| tokens[it].2);
                    // `name!` but not `name != x`: the `!` with no `=` right after it.
                    let bang = next_kind == Some(TokenKind::Bang)
                        && next.is_some_and(|it| tokens.get(it + 1).is_none_or(|after| after.2 != TokenKind::Eq));
                    let class = if bang {
                        macro_bang = next.unwrap_or(usize::MAX);
                        Class::Macro
                    } else if KEYWORDS.contains(&word) {
                        Class::Keyword
                    } else if after_fn || next_kind == Some(TokenKind::OpenParen) {
                        Class::Function
                    } else if word.starts_with(|c: char| c.is_ascii_uppercase()) || PRIMITIVES.contains(&word) {
                        Class::Type
                    } else {
                        Class::Plain
                    };
                    after_fn = word == "fn";
                    if class == Class::Plain {
                        continue;
                    }
                    class
                }
                _ => {
                    after_fn = false;
                    continue;
                }
            };
            if class != Class::Plain {
                runs.push((start, end, class));
            }
        }
    }
}

/// An item's header: where its keyword is (a file byte, for its line) and what to call it
/// (`fn` and `main`; `impl` and what follows it up to its body).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Head<'t> {
    pub(crate) at: u32,
    pub(crate) keyword: &'t str,
    pub(crate) name: &'t str,
}

const ITEMS: &[&str] = &["enum", "fn", "impl", "macro_rules", "mod", "static", "struct", "trait", "type", "union"];
const QUALIFIERS: &[&str] = &["async", "default", "extern", "pub", "safe", "unsafe"];

/// The header of the item that starts at `item` in `text`: its attributes, doc comments,
/// visibility and qualifiers passed over. `None` when no item keyword comes first.
pub(crate) fn head(text: &[u8], item: u32) -> Option<Head<'_>> {
    let source = valid(text.get(item as usize..)?);
    let mut tokens = tokenize(source, FrontmatterAllowed::No, Cpu::BASELINE)
        .scan(0u32, |at, token| {
            let start = *at;
            *at += token.len;
            Some((start, *at, token.kind))
        })
        .filter(|token| !trivia(token.2))
        .take(256);
    let mut depth = 0u32;
    let mut pending_attribute = false;
    while let Some((start, end, kind)) = tokens.next() {
        if depth > 0 || pending_attribute {
            match kind {
                TokenKind::OpenBracket | TokenKind::OpenParen => {
                    depth += 1;
                    pending_attribute = false;
                }
                TokenKind::CloseBracket | TokenKind::CloseParen => depth = depth.saturating_sub(1),
                _ => {}
            }
            continue;
        }
        match kind {
            // `#[..]`, `#![..]`: skipped to the bracket that closes it.
            TokenKind::Pound => pending_attribute = true,
            TokenKind::Bang => {}
            // `pub(crate)`: the parenthesis is skipped like an attribute's bracket.
            TokenKind::OpenParen => depth += 1,
            TokenKind::Literal { .. } => {}
            TokenKind::Ident => {
                let word = &source[start as usize..end as usize];
                if QUALIFIERS.contains(&word) {
                    continue;
                }
                let keyword = match word {
                    "const" => {
                        let (s, e, _) = tokens.next()?;
                        let next = &source[s as usize..e as usize];
                        if matches!(next, "fn" | "unsafe" | "async" | "extern") {
                            continue;
                        }
                        return Some(Head { at: item + start, keyword: word, name: next });
                    }
                    word if ITEMS.contains(&word) => word,
                    _ => return None,
                };
                if keyword == "impl" {
                    // What follows, up to the body or a `where`.
                    let mut last = end;
                    for (s, e, kind) in tokens.by_ref() {
                        if kind == TokenKind::OpenBrace || &source[s as usize..e as usize] == "where" {
                            break;
                        }
                        last = e;
                    }
                    let name = source[end as usize..last as usize].trim();
                    return Some(Head { at: item + start, keyword, name });
                }
                let mut name = tokens.next()?;
                if name.2 == TokenKind::Bang {
                    name = tokens.next()?;
                }
                let name = &source[name.0 as usize..name.1 as usize];
                return Some(Head { at: item + start, keyword, name });
            }
            _ => return None,
        }
    }
    None
}
