//! Elision as it was before tokens were read on demand: the whole source lexed into a list
//! first, a `Vec` of values and one of flags per macro call, a `String` per character or
//! number argument. Kept as the reference [`crate::elide::elide`] is checked against over seeded
//! scripts.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::elide::{
    directives_only,
    lex::tests::reference::{self as lex, Kind, Token},
};

pub fn elide(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> Option<Vec<u8>> {
    let source = std::str::from_utf8(files.get(root)?).ok()?;
    let source = lex::normalize(source);
    let tokens = lex::lex(&source).ok()?;
    let printed = printed(&tokens)?;
    directives_only(printed.as_bytes()).then(|| printed.into_bytes())
}

fn printed(tokens: &[Token<'_>]) -> Option<String> {
    let mut at = 0;
    while tokens.get(at).is_some_and(|it| it.kind == Kind::InnerDoc) {
        at += 1;
    }
    while tokens.get(at).is_some_and(|it| it.kind == Kind::OuterDoc) {
        at += 1;
    }
    let head = tokens.get(at..at + 5)?;
    let head_ok = head[0].is_ident("fn")
        && head[1].is_ident("main")
        && head[2].is_punct('(')
        && head[3].is_punct(')')
        && head[4].is_punct('{');
    if !head_ok {
        return None;
    }
    at += 5;
    let mut out = String::new();
    loop {
        let token = tokens.get(at)?;
        if token.is_punct('}') {
            at += 1;
            break;
        }
        let newline = match token {
            it if it.is_ident("println") => true,
            it if it.is_ident("print") => false,
            _ => return None,
        };
        if !tokens.get(at + 1)?.is_punct('!') {
            return None;
        }
        let (open, close) = match tokens.get(at + 2)?.kind {
            Kind::Punct('(') => ('(', ')'),
            Kind::Punct('[') => ('[', ']'),
            Kind::Punct('{') => ('{', '}'),
            _ => return None,
        };
        let start = at + 3;
        let end = start
            + tokens[start..].iter().position(|it| {
                matches!(it.kind, Kind::Punct('(' | ')' | '[' | ']' | '{' | '}'))
            })?;
        if !tokens[end].is_punct(close) {
            return None;
        }
        format(&tokens[start..end], newline, &mut out)?;
        at = end + 1;
        if open != '{' {
            match tokens.get(at)? {
                it if it.is_punct(';') => at += 1,
                it if it.is_punct('}') => {}
                _ => return None,
            }
        }
    }
    (at == tokens.len()).then_some(out)
}

fn format(args: &[Token<'_>], newline: bool, out: &mut String) -> Option<()> {
    let (spec, mut rest) = args.split_first()?;
    let spec: &str = match &spec.kind {
        Kind::Str { value, escaped_brace: false } => &**value,
        Kind::RawStr { value } => *value,
        _ => return None,
    };
    let mut values = Vec::new();
    while let Some((comma, after)) = rest.split_first() {
        if !comma.is_punct(',') {
            return None;
        }
        let Some((value, after)) = after.split_first() else { break };
        values.push(display(value)?);
        rest = after;
    }
    let mut used = vec![false; values.len()];
    let mut next = 0;
    let bytes = spec.as_bytes();
    let (mut at, mut copied) = (0, 0);
    while at < bytes.len() {
        match bytes[at] {
            brace @ (b'{' | b'}') if bytes.get(at + 1) == Some(&brace) => {
                out.push_str(&spec[copied..=at]);
                at += 2;
                copied = at;
            }
            b'{' => {
                out.push_str(&spec[copied..at]);
                let close = at + 1 + memchr::memchr(b'}', &bytes[at + 1..])?;
                let argument = &spec[at + 1..close];
                let index = if argument.is_empty() {
                    next += 1;
                    next - 1
                } else if argument.bytes().all(|b| b.is_ascii_digit())
                    && (argument == "0" || !argument.starts_with('0'))
                {
                    argument.parse().ok()?
                } else {
                    return None;
                };
                out.push_str(values.get(index)?);
                used[index] = true;
                at = close + 1;
                copied = at;
            }
            b'}' => return None,
            _ => at += 1,
        }
    }
    out.push_str(&spec[copied..]);
    if used.contains(&false) {
        return None;
    }
    if newline {
        out.push('\n');
    }
    Some(())
}

fn display<'t>(token: &'t Token<'_>) -> Option<Cow<'t, str>> {
    match &token.kind {
        Kind::Str { value, .. } => Some(Cow::Borrowed(&**value)),
        Kind::RawStr { value } => Some(Cow::Borrowed(*value)),
        Kind::Char(c) => Some(Cow::Owned(c.to_string())),
        Kind::Int(value) if *value <= i32::MAX as u128 => Some(Cow::Owned(value.to_string())),
        Kind::Ident if token.text == "true" || token.text == "false" => {
            Some(Cow::Borrowed(token.text))
        }
        _ => None,
    }
}
