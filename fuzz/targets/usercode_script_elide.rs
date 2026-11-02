//! A build script's source (user code) through elision: `blaze_build_script::elide(root,
//! files)` with the input as the root file, and the lexer it reads it with
//! (`elide::lex::normalize` and `lex::lex`).
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - Each token's text is the source's own, in order and not overlapping, and says what its
//!   kind says (a punctuation character, an ASCII identifier, `r#` and one, `'` and one, a
//!   string between its quotes whose value is borrowed exactly when it has no escape, a raw
//!   string between its hashes, a character, a decimal integer of that value, a doc comment
//!   of its kind); between tokens is only whitespace and plain comments.
//! - What is elided is text, every line a cargo directive (`cargo:`), the last ended; `verify`
//!   of it against itself agrees, and anything after `main`'s block is not elided.
//! - Elision reads the root file alone (none without it, another file changes nothing) as
//!   rustc reads a source: a byte order mark first, and `\r\n` for `\n`, change nothing.
//! - Eliding asks for at most `blaze_fuzz::allowed(len)` bytes at once.

#![forbid(unsafe_code)]

use std::{
    borrow::Cow,
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use blaze_build_script::{
    elide::{
        self,
        lex::{self, Kind, Token},
    },
    verify,
};
use blaze_fuzzer::Target;
use blaze_mem::{Tag, frame};

const TARGET: Target = Target {
    name: "usercode_script_elide",
    check,
    seeds,
    dictionary: &[
        b"fn",
        b"main",
        b"println",
        b"print",
        b"!",
        b"(",
        b")",
        b"{",
        b"}",
        b"[",
        b"]",
        b";",
        b",",
        b"\"",
        b"r\"",
        b"r#\"",
        b"\"#",
        b"r#",
        b"'",
        b"\\u{",
        b"\\x",
        b"\\n",
        b"\\\n",
        b"{}",
        b"{0}",
        b"{{",
        b"}}",
        b"cargo:",
        b"cargo::",
        b"//",
        b"//!",
        b"///",
        b"/*",
        b"*/",
        b"/**",
        b"/*!",
        b"\xef\xbb\xbf",
        b"\r\n",
        b"true",
        b"false",
        b"2147483647",
        b"2147483648",
        b"0x",
        b"1_000",
        b"\xe2\x80\xae",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const ROOT: &str = "/p/foo/build.rs";

fn files(source: &[u8]) -> BTreeMap<PathBuf, Vec<u8>> {
    BTreeMap::from([(PathBuf::from(ROOT), source.to_vec())])
}

fn elided(source: &[u8]) -> Option<Vec<u8>> {
    elide::elide(Path::new(ROOT), &files(source))
}

fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a comment's text is a doc comment, as rustc tells them.
fn is_doc(comment: &str) -> bool {
    comment.starts_with("//!")
        || (comment.starts_with("///") && !comment.starts_with("////"))
        || comment.starts_with("/*!")
        || (comment.starts_with("/**") && !comment.starts_with("/***") && comment != "/**/")
}

/// Whether `gap` is only whitespace and comments that are not doc comments.
fn is_gap(gap: &str) -> bool {
    let bytes = gap.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' => at += 1,
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                let end = bytes[at..].iter().position(|it| *it == b'\n').map_or(bytes.len(), |it| at + it);
                if is_doc(&gap[at..end]) {
                    return false;
                }
                at = end;
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                let start = at;
                let mut depth = 1usize;
                at += 2;
                while depth > 0 {
                    match (bytes.get(at), bytes.get(at + 1)) {
                        (None, _) => return false,
                        (Some(b'/'), Some(b'*')) => (depth, at) = (depth + 1, at + 2),
                        (Some(b'*'), Some(b'/')) => (depth, at) = (depth - 1, at + 2),
                        _ => at += 1,
                    }
                }
                if is_doc(&gap[start..at]) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// What a token's text says of its kind.
fn check_token(token: &Token<'_>) {
    let text = token.text;
    let inner = |open: usize, close: usize| text.get(open..text.len().saturating_sub(close));
    let ok = match &token.kind {
        Kind::Punct(c) => text.len() == c.len_utf8() && text.starts_with(*c),
        Kind::Ident => is_ident(text),
        Kind::RawIdent => text.strip_prefix("r#").is_some_and(is_ident),
        Kind::Lifetime => text.strip_prefix('\'').is_some_and(is_ident),
        Kind::Str { escaped, escaped_brace } => {
            let between = inner(1, 1).filter(|_| text.len() >= 2 && text.starts_with('"') && text.ends_with('"'));
            match (token.str_value(), between) {
                (Some(Cow::Borrowed(value)), Some(between)) => {
                    value == between && !between.contains('\\') && !escaped && !escaped_brace
                }
                (Some(Cow::Owned(value)), Some(between)) => {
                    between.contains('\\') && *escaped && (!escaped_brace || value.contains(['{', '}']))
                }
                (None, _) | (_, None) => false,
            }
        }
        Kind::RawStr { value } => {
            let hashes = text.strip_prefix('r').map_or(0, |it| it.len() - it.trim_start_matches('#').len());
            let close = format!("\"{}", "#".repeat(hashes));
            text.starts_with('r')
                && text.len() >= 3 + 2 * hashes
                && inner(2 + hashes, 1 + hashes) == Some(*value)
                && text.ends_with(&close)
                && !value.contains(&close)
        }
        Kind::Char(c) => {
            let between = inner(1, 1).filter(|_| text.len() >= 3 && text.starts_with('\'') && text.ends_with('\''));
            between.is_some_and(|it| it.starts_with('\\') || it.chars().eq([*c]))
        }
        Kind::Int(value) => {
            let digits: String = text.chars().filter(|it| *it != '_').collect();
            text.starts_with(|c: char| c.is_ascii_digit())
                && text.bytes().all(|it| it.is_ascii_digit() || it == b'_')
                && digits.parse::<u128>().ok() == Some(*value)
        }
        Kind::Number => text.starts_with(|c: char| c.is_ascii_digit()),
        Kind::OuterDoc => (text.starts_with("///") || text.starts_with("/**")) && is_doc(text),
        Kind::InnerDoc => (text.starts_with("//!") || text.starts_with("/*!")) && is_doc(text),
    };
    assert!(ok, "a token's text is not of its kind: {token:?}");
}

/// The tokens of `source` against the source itself.
fn check_tokens(source: &str, tokens: &[Token<'_>]) {
    let base = source.as_ptr() as usize;
    let mut end = 0;
    for token in tokens {
        let start = (token.text.as_ptr() as usize).checked_sub(base);
        let start = start.filter(|it| *it >= end && it + token.text.len() <= source.len());
        let Some(start) = start else { panic!("a token is not the source's own, after the last: {token:?}") };
        assert!(!token.text.is_empty(), "an empty token: {token:?}");
        assert!(is_gap(&source[end..start]), "not whitespace or a comment before {token:?}: {:?}", &source[end..start]);
        check_token(token);
        end = start + token.text.len();
    }
    assert!(is_gap(&source[end..]), "not whitespace or a comment after the last token: {:?}", &source[end..]);
}

/// The changes of the input checked against eliding it, one a run (`blaze_fuzzer::case`):
/// another file, a byte order mark first, `\r\n` for `\n` (each changing nothing), and a token
/// after `main`'s block (not elided). The plain test takes every one of each seed.
const CHANGES: usize = 4;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CHANGES));
}

fn check_case(data: &[u8], case: usize) {
    if let Ok(source) = std::str::from_utf8(data) {
        frame::task(Tag::Usercode, |frame| {
            let normalized = lex::normalize(source, frame);
            if let Ok(tokens) = lex::lex(normalized) {
                check_tokens(normalized, &tokens);
            }
        });
    }

    let files = files(data);
    let (most, printed) = blaze_fuzz::largest_allocation(|| elide::elide(Path::new(ROOT), &files));
    assert!(most <= blaze_fuzz::allowed(data.len()), "eliding {} bytes asked for {most} at once", data.len());

    assert_eq!(elide::elide(Path::new("/p/foo/other.rs"), &files), None, "a script whose root is not given is not elided");
    match case {
        0 => {
            let mut more = files;
            more.insert(PathBuf::from("/p/foo/src/other.rs"), b"garbage \xff".to_vec());
            assert_eq!(elide::elide(Path::new(ROOT), &more), printed, "another file changes nothing");
        }
        1 if !data.starts_with("\u{feff}".as_bytes()) => {
            let marked = ["\u{feff}".as_bytes(), data].concat();
            assert_eq!(elided(&marked), printed, "a byte order mark first changes nothing");
        }
        2 if !data.contains(&b'\r') => {
            let mut crlf = Vec::with_capacity(data.len() * 2);
            for byte in data {
                if *byte == b'\n' {
                    crlf.push(b'\r');
                }
                crlf.push(*byte);
            }
            assert_eq!(elided(&crlf), printed, "`\\r\\n` for `\\n` changes nothing");
        }
        _ => {}
    }

    let Some(printed) = printed else { return };
    let text = std::str::from_utf8(&printed).expect("what is elided is text");
    if let Some(lines) = text.strip_suffix('\n') {
        for line in lines.split('\n') {
            assert!(line.starts_with("cargo:"), "an elided line is not a directive: {line:?}");
        }
    } else {
        assert!(text.is_empty(), "the last elided line is not ended: {text:?}");
    }
    assert_eq!(verify(&printed, &printed), Ok(()), "what is elided verifies against itself");
    if case == 3 {
        let after = [data, &b"\n;"[..]].concat();
        assert_eq!(elided(&after), None, "a token after `main`'s block is not elided");
    }
}

/// Scripts of every form elision reads, and of forms it refuses.
fn seeds() -> Vec<Vec<u8>> {
    let sources: &[&str] = &[
        "// Empty `build.rs` so that `[package] links = ...` works in `Cargo.toml`.\nfn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n}\n",
        "fn main() {\n    println!(\"cargo:rerun-if-changed=build.rs\");\n    println!(\"cargo::rustc-check-cfg=cfg(reprocheck)\");\n}\n",
        r#"fn main() { println!("cargo:rustc-env=A={}-{}-{}-{}-{}", "s", 'c', 42, true, false); }"#,
        r#"fn main() { println!("cargo:rustc-env=A={1}{0}{1}{}", "x", "y"); }"#,
        r#"fn main() { println!("cargo:x={}", 2147483647); println!("cargo:x={}", 1_000); println!("cargo:x={}", 007); }"#,
        r#"fn main() { println!("cargo:x={}", "{}"); println!("cargo:x={}", "\u{7b}"); println!("cargo:x={{{}}}", "a"); }"#,
        r#"fn main() { println!("cargo:x={}", "a",); println!("cargo:x=1",); }"#,
        r#"fn main() { println!("cargo:rustc-env=A=\x41\u{42}\t\\\"\'\0"); }"#,
        r###"fn main() { println!(r##"cargo:x="#"##); println!(r#"cargo:x={}"#, r"a\"); }"###,
        "fn main() { print!(\"cargo:a=1\\n\"); print!(\"cargo:b=2\\ncargo:c=\"); print!(\"3\\n\") }",
        "fn main() { println!{\"cargo:a=1\"} println![\"cargo:b=2\"]; println![\"cargo:c=3\"] }",
        "fn main ( ) { println /* a comment */ ! ( \"cargo:a=1\" ) ; }",
        "fn main() { println!(\"cargo:a=\\\n      1\"); }",
        "fn main() {\r\n    println!(\"cargo:a=1\r\ncargo:b=2\");\r\n}\r\n",
        "\u{feff}fn main() { println!(\"cargo:a=é\u{301}✓\"); }",
        "/* a /* nested */ comment */ fn main() {}",
        "//! inner\n/*! inner too */\n/// outer\n/** outer */ /**/ /***/ //// plain\nfn main() {}",
        "fn main() { println!(\"cargo:a=1\"); } // done\n",
        // Refused: a format the forms don't read, an argument too big, a prefixed literal, an
        // unknown escape, a lifetime, a number of another base, an item after `main`.
        "fn main() { println!(\"cargo:x={:?}\", 1); println!(\"cargo:x={}\", 2147483648); }",
        "fn main() { println!(b\"cargo:x\"); }",
        "fn main() { println!(\"cargo:x=\\q\"); }",
        "fn main() { 'a: loop {} 0x1f; 1e5; 340282366920938463463374607431768211456; r#fn; '\\u{7b}'; }",
        "fn main() { println!(\"not a directive\"); }\nfn other() {}",
    ];
    sources.iter().map(|it| it.as_bytes().to_vec()).collect()
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CHANGES, super::check_case);
    }
}
