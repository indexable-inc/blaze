//! Text a macro asks to be tokens of (`str::parse::<TokenStream>`, `Literal::from_str`),
//! lexed by the host as rust-analyzer's proc-macro server lexes it (its `token_stream.rs`).
//! Parsed tokens use the current call-site span,
//! as rustc does; offsets into the parsed string are not offsets into the source file.

use intern::Symbol;
use rustc_lexer::{self as lexer, DocStyle, LiteralKind, TokenKind};
use span::Span;
use tt::{DelimiterKind, IdentIsRaw, Leaf, LitKind, Spacing};

use super::wire::Tree;

fn punct_char(kind: TokenKind) -> Option<char> {
    Some(match kind {
        TokenKind::Semi => ';',
        TokenKind::Comma => ',',
        TokenKind::Dot => '.',
        TokenKind::At => '@',
        TokenKind::Pound => '#',
        TokenKind::Tilde => '~',
        TokenKind::Question => '?',
        TokenKind::Colon => ':',
        TokenKind::Dollar => '$',
        TokenKind::Eq => '=',
        TokenKind::Bang => '!',
        TokenKind::Lt => '<',
        TokenKind::Gt => '>',
        TokenKind::Minus => '-',
        TokenKind::And => '&',
        TokenKind::Or => '|',
        TokenKind::Plus => '+',
        TokenKind::Star => '*',
        TokenKind::Slash => '/',
        TokenKind::Caret => '^',
        TokenKind::Percent => '%',
        _ => return None,
    })
}

/// Whether a punctuation mark before a token of `kind` is joint to it.
fn joins(kind: TokenKind) -> bool {
    punct_char(kind).is_some()
        || matches!(
            kind,
            TokenKind::RawLifetime
                | TokenKind::GuardedStrPrefix
                | TokenKind::Lifetime { .. }
                | TokenKind::OpenParen
                | TokenKind::CloseParen
                | TokenKind::OpenBrace
                | TokenKind::CloseBrace
                | TokenKind::OpenBracket
                | TokenKind::CloseBracket
        )
}

fn ident(text: &str, span: Span, is_raw: IdentIsRaw) -> Tree {
    Tree::Leaf(Leaf::Ident(tt::Ident { sym: Symbol::intern(text), span, is_raw }))
}

fn punct(char: char, joint: bool, span: Span) -> Tree {
    let spacing = if joint { Spacing::Joint } else { Spacing::Alone };
    Tree::Leaf(Leaf::Punct(tt::Punct { char, spacing, span }))
}

/// The most bytes a literal's suffix may have: `tt::Literal` keeps its length in a `u8`.
pub const SUFFIX_MOST: usize = u8::MAX as usize;

/// A literal of the lexer's `kind`, `text` its whole text, or why it is none, as rustc's lexer
/// refuses it: unterminated, or of a suffix too long to be kept.
fn literal(
    text: &str,
    span: Span,
    kind: LiteralKind,
    suffix_start: u32,
) -> Result<tt::Literal, String> {
    // alloc: the message of text refused.
    let unterminated = || Err(format!("unterminated literal: `{text}`"));
    let terminated = match kind {
        LiteralKind::Char { terminated }
        | LiteralKind::Byte { terminated }
        | LiteralKind::Str { terminated }
        | LiteralKind::ByteStr { terminated }
        | LiteralKind::CStr { terminated } => terminated,
        LiteralKind::RawStr { n_hashes }
        | LiteralKind::RawByteStr { n_hashes }
        | LiteralKind::RawCStr { n_hashes } => n_hashes.is_some(),
        LiteralKind::Int { .. } | LiteralKind::Float { .. } => true,
    };
    if !terminated {
        return unterminated();
    }
    let (kind, start_offset, end_offset) = match kind {
        LiteralKind::Int { .. } => (LitKind::Integer, 0, 0),
        LiteralKind::Float { .. } => (LitKind::Float, 0, 0),
        LiteralKind::Char { terminated } => (LitKind::Char, 1, terminated as usize),
        LiteralKind::Byte { terminated } => (LitKind::Byte, 2, terminated as usize),
        LiteralKind::Str { terminated } => (LitKind::Str, 1, terminated as usize),
        LiteralKind::ByteStr { terminated } => (LitKind::ByteStr, 2, terminated as usize),
        LiteralKind::CStr { terminated } => (LitKind::CStr, 2, terminated as usize),
        LiteralKind::RawStr { n_hashes } => {
            let n = n_hashes.unwrap_or_default();
            (LitKind::StrRaw(n), 2 + n as usize, 1 + n as usize)
        }
        LiteralKind::RawByteStr { n_hashes } => {
            let n = n_hashes.unwrap_or_default();
            (LitKind::ByteStrRaw(n), 3 + n as usize, 1 + n as usize)
        }
        LiteralKind::RawCStr { n_hashes } => {
            let n = n_hashes.unwrap_or_default();
            (LitKind::CStrRaw(n), 3 + n as usize, 1 + n as usize)
        }
    };
    let Some((lit, suffix)) = text.split_at_checked(suffix_start as usize) else {
        return unterminated();
    };
    let lit = lit.len().checked_sub(end_offset).and_then(|end| lit.get(start_offset..end));
    let Some(lit) = lit else { return unterminated() };
    let suffix = if suffix == "_" { "" } else { suffix };
    if suffix.len() > SUFFIX_MOST {
        let len = suffix.len();
        // alloc: the message of text refused.
        return Err(format!("a literal's suffix of {len} bytes is over the {SUFFIX_MOST} kept"));
    }
    Ok(tt::Literal::new(lit, span, kind, suffix))
}

/// A doc comment as the attribute it stands for.
fn doc(stream: &mut Vec<Tree>, text: &str, style: DocStyle, span: Span) {
    stream.push(punct('#', false, span));
    if style == DocStyle::Inner {
        stream.push(punct('!', false, span));
    }
    // alloc: a doc comment's escaped text, which its literal keeps.
    let value = tt::Literal::new(&text.escape_debug().to_string(), span, LitKind::Str, "");
    stream.push(Tree::Group {
        kind: DelimiterKind::Bracket,
        open: span,
        close: span,
        entire: span,
        // alloc: the doc attribute's three trees.
        trees: vec![
            ident("doc", span, IdentIsRaw::No),
            punct('=', false, span),
            Tree::Leaf(Leaf::Literal(value)),
        ],
    });
}

/// The tokens of `s`, or why it is none.
pub fn stream(s: &str, span: Span) -> Result<Vec<Tree>, String> {
    // alloc: the lexed trees, which the caller keeps.
    let mut groups: Vec<(DelimiterKind, Vec<Tree>)> = vec![(DelimiterKind::Invisible, vec![])];
    let mut offset = 0;
    let mut tokens = lexer::tokenize(s, lexer::FrontmatterAllowed::No).peekable();
    while let Some(token) = tokens.next() {
        let (start, end) = (offset, offset + token.len as usize);
        offset = end;
        let joint = tokens.peek().is_some_and(|next| joins(next.kind));
        let opens = match token.kind {
            TokenKind::OpenParen => Some(DelimiterKind::Parenthesis),
            TokenKind::OpenBrace => Some(DelimiterKind::Brace),
            TokenKind::OpenBracket => Some(DelimiterKind::Bracket),
            _ => None,
        };
        if let Some(kind) = opens {
            // alloc: a group's trees, which the lexed tree keeps.
            groups.push((kind, vec![]));
            continue;
        }
        let closes = match token.kind {
            TokenKind::CloseParen => Some((DelimiterKind::Parenthesis, ')')),
            TokenKind::CloseBrace => Some((DelimiterKind::Brace, '}')),
            TokenKind::CloseBracket => Some((DelimiterKind::Bracket, ']')),
            _ => None,
        };
        if let Some((kind, char)) = closes {
            let open_kind = groups.last().map(|it| it.0);
            if open_kind != Some(kind) {
                return Err(match open_kind {
                    // alloc: the message of text refused.
                    Some(DelimiterKind::Invisible) | None => format!("Unexpected '{char}'"),
                    // alloc: the message of text refused.
                    Some(_) => format!("Expected '{char}'"),
                });
            }
            let (kind, trees) = groups.pop().unwrap();
            let group = Tree::Group { kind, open: span, close: span, entire: span, trees };
            groups.last_mut().ok_or("Unbalanced delimiters")?.1.push(group);
            continue;
        }
        let stream = &mut groups.last_mut().ok_or("Unbalanced delimiters")?.1;
        let text = &s[start..end];
        match token.kind {
            TokenKind::LineComment { doc_style: None }
            | TokenKind::BlockComment { doc_style: None, .. }
            | TokenKind::Whitespace => {}
            TokenKind::LineComment { doc_style: Some(style) } => {
                doc(stream, &s[start + 3..end], style, span)
            }
            TokenKind::BlockComment { doc_style: Some(style), terminated } => {
                let end = if terminated { end - 2 } else { end };
                doc(stream, &s[start + 3..end], style, span)
            }
            // alloc: the message of text refused.
            TokenKind::Unknown => return Err(format!("Unknown token: `{text}`")),
            // alloc: the message of text refused.
            TokenKind::UnknownPrefix => return Err(format!("Unknown prefix: `{text}`")),
            // alloc: the message of text refused.
            TokenKind::ForcedKeywordIdent => return Err(format!("Forced keyword: `{text}`")),
            TokenKind::UnknownPrefixLifetime => {
                // alloc: the message of text refused.
                return Err(format!("Unknown lifetime prefix: `{text}`"));
            }
            // alloc: the message of text refused.
            TokenKind::InvalidIdent => return Err(format!("Invalid identifier: `{text}`")),
            TokenKind::GuardedStrPrefix => {
                let bytes = s.as_bytes();
                stream.push(punct(bytes[start] as char, true, span));
                stream.push(punct(bytes[start + 1] as char, joint, span));
            }
            TokenKind::Ident => stream.push(ident(text, span, IdentIsRaw::No)),
            TokenKind::RawIdent => {
                stream.push(ident(&s[start + 2..end], span, IdentIsRaw::Yes))
            }
            TokenKind::Literal { kind, suffix_start } => {
                let it = literal(text, span, kind, suffix_start)?;
                stream.push(Tree::Leaf(Leaf::Literal(it)))
            }
            TokenKind::RawLifetime | TokenKind::Lifetime { .. } => {
                if let TokenKind::Lifetime { starts_with_number: true } = token.kind {
                    // alloc: the message of text refused.
                    return Err("Lifetime cannot start with a number".to_owned());
                }
                let skip = if token.kind == TokenKind::RawLifetime { 3 } else { 1 };
                stream.push(punct('\'', true, span));
                let is_raw = if token.kind == TokenKind::RawLifetime {
                    IdentIsRaw::Yes
                } else {
                    IdentIsRaw::No
                };
                stream.push(ident(&s[start + skip..end], span, is_raw))
            }
            TokenKind::Eof => break,
            kind => match punct_char(kind) {
                Some(char) => stream.push(punct(char, joint, span)),
                // alloc: the message of text refused.
                None => return Err(format!("Unknown token: `{text}`")),
            },
        }
    }
    match (groups.pop(), groups.is_empty()) {
        (Some((DelimiterKind::Invisible, trees)), true) => Ok(trees),
        // alloc: the message of text refused.
        _ => Err("Mismatched token groups".to_owned()),
    }
}

/// The literal `s` is, with a leading `-` for a number.
pub fn literal_from_str(s: &str, span: Span) -> Option<tt::Literal> {
    let mut tokens = lexer::tokenize(s, lexer::FrontmatterAllowed::No);
    let first = tokens.next()?;
    let (minus, lit) =
        if first.kind == TokenKind::Minus { (1, tokens.next()?) } else { (0, first) };
    if tokens.next().is_some() {
        return None;
    }
    let TokenKind::Literal { kind, suffix_start } = lit.kind else { return None };
    if minus == 1 && !matches!(kind, LiteralKind::Int { .. } | LiteralKind::Float { .. }) {
        return None;
    }
    let it = literal(&s[minus..], span, kind, suffix_start).ok()?;
    if minus == 0 {
        return Some(it);
    }
    let (text, suffix) = it.text_and_suffix();
    // alloc: a negative literal's text, which the literal keeps.
    Some(tt::Literal::new(&format!("-{text}"), span, it.kind, suffix))
}

#[cfg(test)]
mod tests {
    use super::{
        super::{at, tokens},
        *,
    };
    use crate::bridge::lex;

    #[test]
    fn text_is_lexed_as_the_proc_macro_server_lexes_it() {
        let lexed = tokens("fn f<'a>(x: &'a u8) -> r#u8 { x.0 + -1i32 } /// doc", at(0, 0));
        assert_eq!(
            lexed.unwrap().to_string(),
            "fn f <'a >(x : &'a u8) -> r#u8 {x . 0 + - 1i32} # [doc = \" doc\"]"
        );
        assert!(tokens("(]", at(0, 0)).is_err());
        assert!(tokens("a ` b", at(0, 0)).is_err());
        let one = lex::literal_from_str("-1.5f32", at(0, 0)).unwrap();
        assert_eq!(one.text_and_suffix(), ("-1.5", "f32"));
        assert!(lex::literal_from_str("-\"no\"", at(0, 0)).is_none());
    }

    #[test]
    fn text_rustc_refuses_to_lex_is_an_error() {
        for text in ["r\"", "r#", "br\"", "br#", "cr\"", "cr#", "\"open", "b'x"] {
            assert!(tokens(text, at(0, 0)).is_err(), "{text}");
            assert!(lex::literal_from_str(text, at(0, 0)).is_none(), "{text}");
        }
        let long = format!("1{}", "a".repeat(lex::SUFFIX_MOST + 1));
        assert!(tokens(&long, at(0, 0)).is_err());
        assert!(tokens(&long[..lex::SUFFIX_MOST + 1], at(0, 0)).is_ok());
    }
}
