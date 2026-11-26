//! Pure `#![no_std]` syntax tokenizer for `blaze-preview`: produces lightweight colored token
//! spans for Rust, TypeScript/JS, Python, Go, C/C++, Shell, Nix, TOML, JSON, YAML, SQL, Lean,
//! and Flecs with zero regexes and zero OS calls.

use blaze_own::{AString, AVec, Alloc};

/// Semantic syntax token class mapped to palette colors by any UI renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Keyword,
    Function,
    Type,
    String,
    Number,
    Comment,
    Attr,
    Key,
    Punct,
}

/// One highlighted slice on a single line of code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeSpan<'a> {
    pub text: AString<'a>,
    pub kind: TokenKind,
}

/// One syntax-highlighted source line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeLine<'a> {
    pub line_no: u32,
    pub spans: AVec<'a, CodeSpan<'a>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LangStyle {
    CStyle,
    HashComment,
    Sql,
    Lean,
}

fn lang_style(lang: &str) -> LangStyle {
    match lang {
        "Python" | "Python script" | "Shell" | "Shell script" | "Nix" | "Nix flake" | "TOML"
        | "YAML" | "Makefile" | "Justfile" | "Dockerfile" | "Clippy config" | "Rustfmt config"
        | "Cargo manifest" | "Cargo lock" | "uv lock" | "Blaze lock" | "Python proj" | "Ruby"
        | "Ruby script" | "Perl script" => LangStyle::HashComment,
        "SQL" => LangStyle::Sql,
        "Lean 4" => LangStyle::Lean,
        _ => LangStyle::CStyle,
    }
}

fn is_keyword(word: &str, lang: &str) -> bool {
    match lang {
        "Rust" => matches!(
            word,
            "fn" | "let" | "mut" | "pub" | "struct" | "enum" | "impl" | "trait" | "for" | "in"
                | "while" | "loop" | "if" | "else" | "match" | "return" | "use" | "mod" | "const"
                | "static" | "type" | "where" | "as" | "ref" | "move" | "async" | "await"
                | "unsafe" | "extern" | "crate" | "super" | "self" | "Self" | "true" | "false"
                | "break" | "continue"
        ),
        "Python" | "Python script" => matches!(
            word,
            "def" | "class" | "import" | "from" | "as" | "if" | "elif" | "else" | "for" | "in"
                | "while" | "return" | "yield" | "with" | "try" | "except" | "finally" | "raise"
                | "pass" | "break" | "continue" | "and" | "or" | "not" | "is" | "None" | "True"
                | "False" | "async" | "await" | "lambda"
        ),
        "TypeScript" | "TSX" | "JavaScript" | "JSX" | "JS script" | "Bun script" => matches!(
            word,
            "function" | "const" | "let" | "var" | "return" | "if" | "else" | "for" | "while"
                | "switch" | "case" | "break" | "continue" | "import" | "export" | "from"
                | "default" | "class" | "interface" | "type" | "enum" | "async" | "await" | "new"
                | "try" | "catch" | "throw" | "true" | "false" | "null" | "undefined" | "as"
        ),
        "Go" => matches!(
            word,
            "func" | "package" | "import" | "var" | "const" | "type" | "struct" | "interface"
                | "if" | "else" | "for" | "range" | "switch" | "case" | "return" | "go" | "defer"
                | "select" | "chan" | "map" | "true" | "false" | "nil"
        ),
        "Shell" | "Shell script" => matches!(
            word,
            "if" | "then" | "else" | "elif" | "fi" | "for" | "in" | "do" | "done" | "while"
                | "case" | "esac" | "function" | "local" | "export" | "return" | "exit" | "set"
        ),
        "Nix" | "Nix flake" => matches!(
            word,
            "let" | "in" | "with" | "inherit" | "if" | "then" | "else" | "rec" | "import"
                | "true" | "false" | "null"
        ),
        "TOML" | "YAML" | "JSON" | "JSONL" | "Cargo manifest" | "Clippy config"
        | "Rustfmt config" => matches!(word, "true" | "false" | "null"),
        _ => matches!(
            word,
            "fn" | "def" | "func" | "let" | "const" | "var" | "if" | "else" | "for" | "while"
                | "return" | "struct" | "enum" | "class" | "import" | "export" | "use" | "pub"
                | "true" | "false" | "null" | "nil"
        ),
    }
}

/// Tokenize up to `max_lines` of `src` into styled [`CodeLine`]s in `alloc`.
pub fn highlight_code<'a>(
    src: &str,
    lang: &str,
    max_lines: usize,
    alloc: Alloc<'a>,
) -> AVec<'a, CodeLine<'a>> {
    let style = lang_style(lang);
    let mut out = AVec::new_in(alloc);
    for (idx, raw_line) in src.lines().take(max_lines).enumerate() {
        let mut spans = AVec::new_in(alloc);
        tokenize_line(raw_line, lang, style, &mut spans, alloc);
        out.push(CodeLine { line_no: (idx + 1) as u32, spans });
    }
    out
}

fn push_span<'a>(spans: &mut AVec<'a, CodeSpan<'a>>, text: &str, kind: TokenKind, alloc: Alloc<'a>) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.kind == kind
    {
        last.text.push_str(text);
        return;
    }
    spans.push(CodeSpan { text: AString::from_str_in(text, alloc), kind });
}

fn tokenize_line<'a>(
    line: &str,
    lang: &str,
    style: LangStyle,
    spans: &mut AVec<'a, CodeSpan<'a>>,
    alloc: Alloc<'a>,
) {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    // Leading `#` Rust attribute (`#[...]`, `#![...]`) or TOML `[section]` header.
    let trimmed = line.trim_start();
    if matches!(lang, "Rust") && (trimmed.starts_with("#[") || trimmed.starts_with("#![")) {
        push_span(spans, line, TokenKind::Attr, alloc);
        return;
    }
    if matches!(
        lang,
        "TOML" | "Cargo manifest" | "Clippy config" | "Rustfmt config" | "Cargo lock" | "uv lock"
    ) && trimmed.starts_with('[')
        && trimmed.ends_with(']')
    {
        push_span(spans, line, TokenKind::Attr, alloc);
        return;
    }

    while i < len {
        let b = bytes[i];

        // Line comments
        let is_comment = match style {
            LangStyle::CStyle => b == b'/' && i + 1 < len && bytes[i + 1] == b'/',
            LangStyle::HashComment => {
                b == b'#'
                    && (i == 0 || bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b';')
            }
            LangStyle::Sql | LangStyle::Lean => b == b'-' && i + 1 < len && bytes[i + 1] == b'-',
        };
        if is_comment {
            push_span(spans, &line[i..], TokenKind::Comment, alloc);
            return;
        }

        // Quoted strings (`"..."` or `'...'`, avoiding Rust lifetimes `'a`).
        if b == b'"' || (b == b'\'' && !(lang == "Rust" && is_rust_lifetime(bytes, i))) {
            let quote = b;
            let start = i;
            i += 1;
            let mut esc = false;
            while i < len {
                let c = bytes[i];
                i += 1;
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == quote {
                    break;
                }
            }
            push_span(spans, &line[start..i], TokenKind::String, alloc);
            continue;
        }

        // Numbers
        if b.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < len
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
            {
                i += 1;
            }
            push_span(spans, &line[start..i], TokenKind::Number, alloc);
            continue;
        }

        // Identifiers / keywords / types / TOML keys
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < len
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'-')
            {
                // Don't swallow `-` in C-style expressions like `a-b`.
                if bytes[i] == b'-' && style != LangStyle::HashComment {
                    break;
                }
                i += 1;
            }
            let word = &line[start..i];
            let mut look = i;
            while look < len && bytes[look] == b' ' {
                look += 1;
            }
            let is_kv_key = style == LangStyle::HashComment
                && look < len
                && (bytes[look] == b'=' || bytes[look] == b':');
            let is_call = look < len && (bytes[look] == b'(' || bytes[look] == b'!');
            let is_primitive_ty = lang == "Rust"
                && matches!(
                    word,
                    "bool" | "char" | "str" | "u8" | "u16" | "u32" | "u64" | "u128" | "usize"
                        | "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "f32" | "f64"
                );
            let is_pascal_type = word.as_bytes()[0].is_ascii_uppercase()
                && (word.bytes().any(|c| c.is_ascii_lowercase()) || word.len() >= 2);
            let kind = if is_keyword(word, lang) {
                TokenKind::Keyword
            } else if is_kv_key {
                TokenKind::Key
            } else if is_primitive_ty || is_pascal_type {
                TokenKind::Type
            } else if is_call {
                TokenKind::Function
            } else {
                TokenKind::Plain
            };
            push_span(spans, word, kind, alloc);
            continue;
        }

        // Punctuation vs whitespace
        if b == b' ' || b == b'\t' {
            let start = i;
            while i < len && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            push_span(spans, &line[start..i], TokenKind::Plain, alloc);
            continue;
        }

        // Advance one UTF-8 char safely.
        let ch_len = line[i..].chars().next().map_or(1, char::len_utf8);
        let kind = if b.is_ascii_punctuation() { TokenKind::Punct } else { TokenKind::Plain };
        push_span(spans, &line[i..i + ch_len], kind, alloc);
        i += ch_len;
    }
}

fn is_rust_lifetime(bytes: &[u8], i: usize) -> bool {
    if i + 1 >= bytes.len() {
        return false;
    }
    let next = bytes[i + 1];
    if !next.is_ascii_alphabetic() && next != b'_' {
        return false;
    }
    // `'a'` is a char literal, `'a` followed by non-quote is a lifetime.
    i + 2 >= bytes.len() || bytes[i + 2] != b'\''
}
