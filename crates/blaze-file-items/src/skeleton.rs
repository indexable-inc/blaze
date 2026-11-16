//! The bodies an outline leaves unlexed, and the text the parser reads in their place.
//!
//! A `fn` item's body that cannot hold an item or a macro call adds nothing to an outline but
//! its extent and its bytes (an entry's digests are over its bytes, crate docs): it is found by
//! a token scan of the text outside such bodies, its end by `blaze_lex::skip_body` without
//! lexing it, and the parser is handed the *skeleton*, the text with each such body's inside
//! cut out (`{ .. }` becomes `{}`). The walk maps the skeleton's offsets back to the file's
//! ([`Skipped`]).
//!
//! A body is found as the first `{` outside `( )`, `[ ]` and `{ }` after `fn` and a name (a
//! `fn` type, `fn(..)`, has no name), before a `;`. In code the parser takes, that `{` opens
//! the `fn`'s body: a signature holds braces only inside brackets or generic arguments (a
//! const argument), and a brace inside generic arguments taken for the body is a balanced
//! group with no item in it, whose cutting changes no entry. Every region cut is a balanced
//! `{ .. }` of tokens whose bytes hold no item keyword and no `!` after a path
//! ([`may_hold_items`]): whatever the parser makes of it, it holds no entry and no item.

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, skip_body, tokenize};
use blaze_own::{AVec, Alloc, Vec};

/// A body left unlexed: the file offsets of its `{` and its `}`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Skipped {
    pub(crate) open: u32,
    pub(crate) close: u32,
}

impl Skipped {
    /// The bytes the skeleton holds fewer than the file for this body.
    pub(crate) fn cut(self) -> u32 {
        self.close - self.open - 1
    }
}

/// The bodies of `text` an outline leaves unlexed, in order, and the skeleton text.
pub(crate) fn skeleton<'a>(text: &str, cpu: Cpu, alloc: Alloc<'a>) -> (AVec<'a, Skipped>, AVec<'a, u8>) {
    let mut skipped: AVec<'a, Skipped> = Vec::new_in(alloc);
    let bytes = text.as_bytes();
    // A shebang line is lexed by the parser's own rule: the scan starts past it.
    let mut pos = match bytes.starts_with(b"#!") && !bytes.starts_with(b"#![") {
        true => blaze_memchr::memchr(b'\n', bytes).map_or(bytes.len(), |at| at + 1),
        false => 0,
    };
    let mut state = State::Outside;
    // Frontmatter may open the file (after a shebang), never text after a skipped body.
    let mut frontmatter = FrontmatterAllowed::Yes;
    'restart: while pos < bytes.len() {
        let mut at = pos;
        for token in tokenize(&text[pos..], frontmatter, cpu) {
            let start = at;
            at += usize::try_from(token.len).expect("a token's length fits usize");
            if matches!(token.kind, TokenKind::Whitespace | TokenKind::LineComment { doc_style: None })
                || matches!(token.kind, TokenKind::BlockComment { doc_style: None, .. })
            {
                continue;
            }
            state = match (state, token.kind) {
                (State::AfterFn, TokenKind::Ident | TokenKind::RawIdent) => State::Signature { depth: 0 },
                (State::Outside | State::AfterFn, TokenKind::Ident) if &text[start..at] == "fn" => State::AfterFn,
                (State::Signature { depth: 0 }, TokenKind::OpenBrace) => {
                    if let Ok(inside) = skip_body(cpu, &text[start..])
                        && !may_hold_items(&bytes[start + 1..start + inside])
                    {
                        let close = start + inside;
                        skipped.push(Skipped {
                            open: u32::try_from(start).expect("a file under 4 GiB"),
                            close: u32::try_from(close).expect("a file under 4 GiB"),
                        });
                        pos = close + 1;
                        frontmatter = FrontmatterAllowed::No;
                        state = State::Outside;
                        continue 'restart;
                    }
                    State::Outside
                }
                (State::Signature { depth: 0 }, TokenKind::Semi | TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace) => {
                    State::Outside
                }
                (State::Signature { depth }, TokenKind::OpenParen | TokenKind::OpenBracket | TokenKind::OpenBrace) => {
                    State::Signature { depth: depth + 1 }
                }
                (State::Signature { depth }, TokenKind::CloseParen | TokenKind::CloseBracket | TokenKind::CloseBrace) => {
                    State::Signature { depth: depth - 1 }
                }
                (State::Signature { depth }, _) => State::Signature { depth },
                (State::AfterFn | State::Outside, _) => State::Outside,
            };
        }
        break;
    }
    let mut out: AVec<'a, u8> = Vec::with_capacity_in(bytes.len(), alloc);
    let mut from = 0usize;
    for body in &skipped {
        let open = usize::try_from(body.open).expect("a file offset fits usize");
        let close = usize::try_from(body.close).expect("a file offset fits usize");
        out.extend_from_slice(&bytes[from..=open]);
        from = close;
    }
    out.extend_from_slice(&bytes[from..]);
    (skipped, out)
}

/// Where the scan is: outside a `fn` signature, right after `fn`, or in a signature at a depth
/// of brackets.
#[derive(Clone, Copy)]
enum State {
    Outside,
    AfterFn,
    Signature { depth: u32 },
}

/// Whether the bytes inside a body may hold an item or a macro call: false only when they hold
/// no item keyword as a word (`fn`, `struct`, `enum`, `union`, `trait`, `impl`, `const` but
/// after `*`, `static` but after `'`, `type`, `mod`, `use`, `extern`, `macro`) and no `!` that
/// may follow a path: a `!` not followed by `=` (a joint `!=` is never a call's) whose byte
/// before, past ASCII whitespace, is an identifier's (not one of the keywords that end no path)
/// or a `>` or a `/`. A byte of 0x80 or more is no identifier byte here, so a keyword after a
/// non-ASCII character counts: every miss is on the safe side. Strings and comments are read
/// as code, also on the safe side.
pub(crate) fn may_hold_items(body: &[u8]) -> bool {
    const ITEMS: [&[u8]; 13] =
        [b"fn", b"struct", b"enum", b"union", b"trait", b"impl", b"const", b"static", b"type", b"mod", b"use", b"extern", b"macro"];
    let mut at = 0;
    while at < body.len() {
        let byte = body[at];
        if is_ident(byte) {
            let start = at;
            while at < body.len() && is_ident(body[at]) {
                at += 1;
            }
            let word = &body[start..at];
            let before = start.checked_sub(1).map(|it| body[it]);
            let item = ITEMS.contains(&word)
                && !(word == b"const" && before_space(body, start) == Some(b'*'))
                && !(word == b"static" && before == Some(b'\''));
            if item {
                return true;
            }
            continue;
        }
        if byte == b'!' && body.get(at + 1) != Some(&b'=') && after_path(body, at) {
            return true;
        }
        at += 1;
    }
    false
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The byte before `at`, past ASCII whitespace.
fn before_space(body: &[u8], at: usize) -> Option<u8> {
    body[..at].iter().rev().copied().find(|byte| !byte.is_ascii_whitespace())
}

/// Whether the `!` at `at` may follow a path.
fn after_path(body: &[u8], at: usize) -> bool {
    const NO_PATH: [&[u8]; 12] = [b"if", b"while", b"return", b"match", b"in", b"else", b"let", b"mut", b"break", b"as", b"move", b"ref"];
    let mut end = at;
    while end > 0 && body[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let Some(&last) = end.checked_sub(1).and_then(|it| body.get(it)) else { return false };
    if last == b'>' || last == b'/' {
        return true;
    }
    if !is_ident(last) {
        return false;
    }
    let mut start = end;
    while start > 0 && is_ident(body[start - 1]) {
        start -= 1;
    }
    // A raw identifier (`r#if`) or one after a non-ASCII character is a name like any other.
    let plain = start == 0 || body[start - 1].is_ascii() && body[start - 1] != b'#';
    !(plain && NO_PATH.contains(&&body[start..end]))
}
