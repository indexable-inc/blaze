//! Highlighting from blaze's own lexer: a small fixed set of [`Class`]es read from the tokens
//! and their neighbours (an identifier before `(` is a function, before `!` a macro), and the
//! header of the item a finding is in (`fn main`), for the frame's context.

use blaze_lex::{Cpu, FrontmatterAllowed, LiteralKind, TokenKind, tokenize};
use blaze_own::{AVec, Alloc};

/// What a piece of code is. The frame draws structure by brightness (draw.rs, `class_style`):
/// names, keys and keywords in the normal ink, punctuation, operators and comments dim, literals
/// in one soft tint; the classes stay apart so that rule can change in one place.
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
    /// Punctuation and operators.
    Punct,
    /// A stand-in for a control character or a byte that is not UTF-8.
    Escape,
    /// In a message: code (between backticks), and the backticks themselves.
    Code,
    Tick,
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

/// A file kind's highlighting: the classes of a range of its bytes, and where the innermost
/// scope enclosing a byte is headed. A kind is a unit struct implementing this and a line in
/// [`highlighter`].
pub(crate) trait Highlighter {
    /// The runs of `lo..hi`, appended to `runs` sorted and disjoint (they may reach outside).
    fn runs(&self, text: &[u8], lo: u32, hi: u32, runs: &mut AVec<'_, (u32, u32, Class)>);

    /// The first byte of the header of the innermost item enclosing `byte` (a `fn`'s
    /// signature, an `impl`, a TOML table's `[name]`), when the kind has items. `item` is
    /// where the caller's item starts, when known: a scan may begin there.
    fn enclosing(&self, text: &[u8], item: Option<u32>, byte: u32, alloc: Alloc<'_>) -> Option<u32> {
        let _ = (text, item, byte, alloc);
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

    fn enclosing(&self, text: &[u8], item: Option<u32>, byte: u32, alloc: Alloc<'_>) -> Option<u32> {
        rust_enclosing(text, item.filter(|&it| it <= byte).unwrap_or(0), byte, alloc)
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
                    // `[name]` or `[[name]]`: the brackets punctuation, the name a name.
                    header = true;
                    let end = text[at..].iter().position(|&b| b == b'\n' || b == b'#').map_or(text.len(), |it| at + it);
                    let close = text[at..end].iter().rposition(|&b| b == b']').map_or(end, |it| at + it + 1);
                    let open = at + text[at..close].iter().take_while(|&&b| b == b'[').count();
                    let shut = close - text[open..close].iter().rev().take_while(|&&b| b == b']').count();
                    runs.push((start, open as u32, Class::Punct));
                    if shut > open {
                        runs.push((open as u32, shut as u32, Class::Type));
                    }
                    if close > shut {
                        runs.push((shut as u32, close as u32, Class::Punct));
                    }
                    close
                }
                b'"' | b'\'' => {
                    let triple = text[at..].starts_with(if byte == b'"' { b"\"\"\"" } else { b"'''" });
                    let end = string_end(text, at, byte, triple);
                    run(runs, end, if key_side && !header && depth == 0 { Class::Function } else { Class::Str })
                }
                b'=' if depth == 0 => {
                    key_side = false;
                    run(runs, at + 1, Class::Punct)
                }
                b'[' | b'{' => {
                    depth += 1;
                    run(runs, at + 1, Class::Punct)
                }
                b']' | b'}' => {
                    depth = depth.saturating_sub(1);
                    run(runs, at + 1, Class::Punct)
                }
                b',' if depth > 0 => {
                    // In an inline table, a key comes after a comma.
                    key_side = text[..at].iter().rposition(|&b| b == b'{' || b == b'[').is_some_and(|it| text[it] == b'{');
                    run(runs, at + 1, Class::Punct)
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
                _ if byte.is_ascii_punctuation() => run(runs, at + 1, Class::Punct),
                _ => at + 1,
            };
            if byte == b'{' && !key_side {
                key_side = true;
            }
        }
    }

    /// The nearest table header (`[name]`, `[[name]]`) on a line above `byte`'s.
    fn enclosing(&self, text: &[u8], _: Option<u32>, byte: u32, _: Alloc<'_>) -> Option<u32> {
        let byte = (byte as usize).min(text.len());
        let mut end = text[..byte].iter().rposition(|&b| b == b'\n')?;
        // A span on a header's own line is in that table: nothing above it encloses it.
        if text[end + 1..].iter().find(|&&b| b != b' ' && b != b'\t') == Some(&b'[') {
            return None;
        }
        loop {
            let start = text[..end].iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
            let line = &text[start..end];
            if let Some(first) = line.iter().position(|&b| b != b' ' && b != b'\t')
                && line[first] == b'['
            {
                return Some((start + first) as u32);
            }
            if start == 0 {
                return None;
            }
            end = start - 1;
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
                        Class::Punct
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
                    Class::Punct
                }
            };
            if class != Class::Plain {
                runs.push((start, end, class));
            }
        }
    }
}

/// The keywords that make a braced body an item's: the scopes the frame shows the header of.
const ITEMS: &[&str] = &["enum", "fn", "impl", "macro_rules", "mod", "static", "struct", "trait", "union"];

/// The first byte of the header of the innermost item whose braces hold `byte`, lexed from
/// `from` (an item's start, or the file's): a stack of the open braces, each with the start of
/// its statement when that statement names an item (`pub fn f(..) {`, `impl X for Y {`), its
/// attributes passed over. No recursion: the stack is a vector.
fn rust_enclosing(text: &[u8], from: u32, byte: u32, alloc: Alloc<'_>) -> Option<u32> {
    let source = valid(text.get(from as usize..)?);
    let mut stack: AVec<'_, Option<u32>> = AVec::new_in(alloc);
    // The first token of the statement so far (not an attribute's), and whether it names an item.
    let mut statement: Option<u32> = None;
    let mut item = false;
    // `#` seen; then the depth of the attribute's brackets.
    let mut pound = false;
    let mut attribute = 0u32;
    let mut at = from;
    for token in tokenize(source, FrontmatterAllowed::No, Cpu::BASELINE) {
        let start = at;
        at += token.len;
        if start >= byte {
            break;
        }
        let kind = token.kind;
        if trivia(kind) {
            continue;
        }
        if attribute > 0 {
            match kind {
                TokenKind::OpenBracket => attribute += 1,
                TokenKind::CloseBracket => attribute -= 1,
                _ => {}
            }
            continue;
        }
        match kind {
            TokenKind::Pound => {
                pound = true;
                continue;
            }
            TokenKind::Bang if pound => continue,
            TokenKind::OpenBracket if pound => {
                pound = false;
                attribute = 1;
                continue;
            }
            _ => pound = false,
        }
        match kind {
            TokenKind::OpenBrace => {
                stack.push(if item { statement } else { None });
                statement = None;
                item = false;
            }
            TokenKind::CloseBrace => {
                stack.pop();
                statement = None;
                item = false;
            }
            TokenKind::Semi => {
                statement = None;
                item = false;
            }
            _ => {
                statement = statement.or(Some(start));
                let word = &source[(start - from) as usize..(at - from) as usize];
                item |= kind == TokenKind::Ident && ITEMS.contains(&word);
            }
        }
    }
    stack.iter().rev().find_map(|it| *it)
}
