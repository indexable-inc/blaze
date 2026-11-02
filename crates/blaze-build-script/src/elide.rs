//! Build scripts that provably only print constant cargo directives, read rather than
//! compiled and run (#1296): what such a script prints is known from its source, on every
//! target, cfg and machine. The forms read are Relax's (`front/src/unitgraph_noop.rs` of
//! makepad/makepad at d5bed7bba) and a few more, each proved by the rules rustc applies:
//!
//! - the root file is doc comments and one `fn main() { … }`, nothing else: no attribute but
//!   doc comments (`//!` first, then `///`), no `use`, `extern`, `mod`, other item, return type,
//!   `pub`, `async`, `unsafe` or `const`, so nothing replaces `main`, shadows `println!` or
//!   reads another file;
//! - the body is `println!(…)` and `print!(…)` statements alone (any delimiters), each a
//!   string literal or raw string as the format string with literal arguments: strings,
//!   characters, decimal integers no bigger than `i32::MAX` with no suffix, `true`, `false`;
//!   placeholders `{}` and `{N}` only, every argument used, no escape that makes a brace in the
//!   format string;
//! - every line printed is a cargo directive (`cargo:…`) and the last ends in `\n`.
//!
//! Anything else is `None`, and the script is compiled and run as any other. The tokens are
//! this crate's own ([`lex`]), so elision depends on no front end. They are read one at a
//! time as the forms ask for them, so most scripts (which start with `use`) are refused at
//! their first token, and nothing is allocated per token: a macro call's arguments are held
//! in the task's frame ([`blaze_mem::Frame`]) and what is printed in the caller's buffer
//! ([`elide_into`]), each reused.

pub mod lex;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use blaze_mem::{Frame, Tag, frame, scratch::Lent};

use self::lex::{Kind, Token, Tokens};
use crate::Error;

/// What the script whose root file is `root` prints, when its source proves it: the exact
/// bytes of its standard output, which then go through [`crate::output::parse`] (or
/// [`crate::run::elided`] and [`crate::run::Ran::finish`]) as a run's do. `files` are its
/// source files by path: the root and the module files it declares. A script elision accepts
/// declares none and reads no other file, so only the root is read; `None` when it is not in
/// `files`.
///
/// The verdict is the source's alone: the flags the script itself is compiled with must be
/// ones [`flags_allow`] accepts.
pub fn elide(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>) -> Option<Vec<u8>> {
    // alloc: the edge: the printed bytes, owned, which the caller keeps as the run's output.
    let mut out = Vec::new();
    elide_into(root, files, &mut out).then_some(out)
}

/// [`elide`] into the caller's buffer, which it reuses across scripts: whether the script
/// prints what its source proves, with the bytes written to `out` (emptied first, and left
/// empty when not). Its temporaries are bumped in the task's frame
/// ([`blaze_mem::frame::task`]), so a warm read allocates nothing.
pub fn elide_into(root: &Path, files: &BTreeMap<PathBuf, Vec<u8>>, out: &mut Vec<u8>) -> bool {
    out.clear();
    let Some(source) = files.get(root) else { return false };
    if !may_open_main(source) {
        return false;
    }
    let Ok(source) = std::str::from_utf8(source) else { return false };
    let len = source.len();
    let read = frame::task(Tag::Usercode, |frame| {
        let source = lex::normalize(source, frame);
        printed(Tokens::new(source).ok()?, len, out, frame)
    });
    let kept = read.is_some() && directives_only(out);
    if !kept {
        out.clear();
    }
    kept
}

/// Whether `flags`, the flags blaze compiles a build script itself with (not its unit's
/// `rustflags`, which a script is only told), leave what [`elide`] proves true. Flags can make
/// a warning an error (`-D warnings`, `-D missing-docs`), add crate attributes
/// (`-Z crate-attr`), take std away or change linking, any of which can fail the compile of a
/// script elision would read; only optimization and debug info levels and plain `--cfg`s,
/// which the forms elision reads don't see, are allowed.
pub fn flags_allow(flags: &[String]) -> bool {
    let mut flags = flags.iter().map(String::as_str);
    while let Some(flag) = flags.next() {
        let allowed = match flag {
            "-C" => flags.next().is_some_and(inert_codegen),
            "--cfg" => flags.next().is_some_and(plain_cfg),
            _ => match (flag.strip_prefix("-C"), flag.strip_prefix("--cfg=")) {
                (Some(option), _) => inert_codegen(option),
                (_, Some(cfg)) => plain_cfg(cfg),
                _ => false,
            },
        };
        if !allowed {
            return false;
        }
    }
    true
}

fn inert_codegen(option: &str) -> bool {
    match option.split_once('=') {
        Some(("opt-level", level)) => matches!(level, "0" | "1" | "2" | "3" | "s" | "z"),
        Some(("debuginfo", level)) => matches!(
            level,
            "0" | "1"
                | "2"
                | "none"
                | "line-directives-only"
                | "line-tables-only"
                | "limited"
                | "full"
        ),
        _ => false,
    }
}

/// `name` or `name="value"`, `name` an ASCII identifier and the value with no quote or
/// backslash.
fn plain_cfg(cfg: &str) -> bool {
    let (name, value) = cfg.split_once('=').unwrap_or((cfg, "\"\""));
    let name_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let value = value.strip_prefix('"').and_then(|it| it.strip_suffix('"'));
    name_ok && value.is_some_and(|it| !it.contains(['"', '\\']))
}

/// Whether an elided script's output and a run's agree, for `BLAZE_ELIDE=verify`, which runs
/// both: if not, an error naming the first line that differs.
pub fn verify(elided: &[u8], ran: &[u8]) -> Result<(), Error> {
    if elided == ran {
        return Ok(());
    }
    let mut elided_lines = elided.split_inclusive(|b| *b == b'\n');
    let mut ran_lines = ran.split_inclusive(|b| *b == b'\n');
    let mut line = 1;
    loop {
        let (said, printed) = (elided_lines.next(), ran_lines.next());
        if said != printed {
            let shown = |it: Option<&[u8]>| match it {
                // alloc: error path.
                Some(it) => format!("{:?}", String::from_utf8_lossy(it)),
                // alloc: error path.
                None => "nothing".to_owned(),
            };
            // alloc: error path.
            return Err(Error(format!(
                "the elided build script's output differs from its run's at line {line}: \
                 elision says {}, the run printed {}",
                shown(said),
                shown(printed)
            )));
        }
        line += 1;
    }
}

/// Every line a cargo directive, the last ended.
fn directives_only(printed: &[u8]) -> bool {
    let Some(lines) = printed.strip_suffix(b"\n") else { return printed.is_empty() };
    lines.split(|it| *it == b'\n').all(|line| line.starts_with(b"cargo:"))
}

/// The next token, or none at the end or at a refusal: either leaves the script unread.
fn next<'a>(tokens: &mut Tokens<'a>) -> Option<Token<'a>> {
    tokens.next()?.ok()
}

/// Whether the root file's first token may open the forms read: a comment or doc comment
/// (`/`) or `fn` (`f`), after a byte order mark and white space. Most scripts start with
/// `use` and are refused here, at their first bytes, before the file is validated, normalized
/// or lexed. A byte that is not ASCII may be white space Rust reads (U+200E and the rest), so
/// it is left to the lexer.
fn may_open_main(source: &[u8]) -> bool {
    let source = source.strip_prefix(b"\xef\xbb\xbf").unwrap_or(source);
    let first = source.iter().find(|it| !matches!(it, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c));
    first.is_none_or(|it| matches!(it, b'/' | b'f') || !it.is_ascii())
}

/// What a script may print per byte of its root file for elision to read it. A placeholder
/// names its argument any number of times, so what is printed can grow as the square of the
/// source; past this bound the script is run instead, whose output the sandbox bounds. It
/// keeps every allocation elision makes under `blaze_fuzz::allowed` (1 MiB and 64 bytes per
/// input byte) with room for `String`'s doubling, and no real script comes near it.
const PRINTED_PER_BYTE: usize = 16;

/// The arguments a macro call is read with before they are grown, as many as most calls have.
const ARGS: usize = 8;

/// What is printed so far, never longer than `limit` bytes.
struct Printed<'o> {
    /// The caller's buffer, empty at the start.
    text: &'o mut Vec<u8>,
    limit: usize,
}

impl Printed<'_> {
    /// `piece` added, or `None` when that would pass the limit.
    fn push(&mut self, piece: &str) -> Option<()> {
        if self.text.len() + piece.len() > self.limit {
            return None;
        }
        self.text.extend_from_slice(piece.as_bytes());
        Some(())
    }
}

/// What the script of `tokens`, whose source is `len` bytes, prints, if it is of the forms read
/// and prints no more than [`PRINTED_PER_BYTE`] bytes per byte of it.
fn printed(mut tokens: Tokens<'_>, len: usize, text: &mut Vec<u8>, frame: &Frame) -> Option<()> {
    let mut token = next(&mut tokens)?;
    while token.kind == Kind::InnerDoc {
        token = next(&mut tokens)?;
    }
    while token.kind == Kind::OuterDoc {
        token = next(&mut tokens)?;
    }
    let head_ok = token.is_ident("fn")
        && next(&mut tokens)?.is_ident("main")
        && next(&mut tokens)?.is_punct('(')
        && next(&mut tokens)?.is_punct(')')
        && next(&mut tokens)?.is_punct('{');
    if !head_ok {
        return None;
    }
    // The caller's buffer grows to its high-water mark once: a script of directives prints
    // less than it is.
    text.reserve(len);
    let mut out = Printed { text, limit: PRINTED_PER_BYTE * len };
    let mut args: Vec<Token<'_>, &Frame> = Vec::with_capacity_in(ARGS, frame);
    let mut scratch = Scratch {
        spec: blaze_mem::scratch::lend_string(),
        used: Vec::new_in(frame),
        arg: blaze_mem::scratch::lend_string(),
    };
    let mut token = next(&mut tokens)?;
    while !token.is_punct('}') {
        let newline = match &token {
            it if it.is_ident("println") => true,
            it if it.is_ident("print") => false,
            _ => return None,
        };
        if !next(&mut tokens)?.is_punct('!') {
            return None;
        }
        let (open, close) = match next(&mut tokens)?.kind {
            Kind::Punct('(') => ('(', ')'),
            Kind::Punct('[') => ('[', ']'),
            Kind::Punct('{') => ('{', '}'),
            _ => return None,
        };
        // Arguments are literals and commas, so the first delimiter after the opening one is
        // its close, or the script is of no form read.
        args.clear();
        let end = loop {
            let token = next(&mut tokens)?;
            if matches!(token.kind, Kind::Punct('(' | ')' | '[' | ']' | '{' | '}')) {
                break token;
            }
            args.push(token);
        };
        if !end.is_punct(close) {
            return None;
        }
        format(&args, newline, &mut out, &mut scratch)?;
        token = next(&mut tokens)?;
        // A brace-delimited macro is a statement of its own; a `;` after it would be an empty
        // statement (`redundant_semicolons`). Another needs its `;` unless it ends the block.
        if open != '{' {
            if token.is_punct(';') {
                token = next(&mut tokens)?;
            } else if !token.is_punct('}') {
                return None;
            }
        }
    }
    tokens.next().is_none().then_some(())
}

/// What [`format`] reuses from one macro call to the next.
struct Scratch<'f> {
    /// A format string's value, when an escape made it differ from its text.
    spec: Lent<String>,
    /// Which arguments a placeholder used.
    used: Vec<bool, &'f Frame>,
    /// A string argument's value, when an escape made it differ from its text.
    arg: Lent<String>,
}

/// What `format_args!` (with `println!`'s newline) makes of `args`, added to `out`. The
/// format string is copied in runs between its braces, each argument written where the
/// source has it, with nothing allocated but what `out` and `scratch` grow by.
fn format(
    args: &[Token<'_>],
    newline: bool,
    out: &mut Printed<'_>,
    scratch: &mut Scratch<'_>,
) -> Option<()> {
    let (spec, rest) = args.split_first()?;
    let spec: &str = match spec.kind {
        Kind::Str { escaped: false, escaped_brace: false } => &spec.text[1..spec.text.len() - 1],
        Kind::Str { escaped: true, escaped_brace: false } => {
            scratch.spec.clear();
            spec.push_value(&mut scratch.spec)?;
            &scratch.spec
        }
        Kind::RawStr { value } => value,
        _ => return None,
    };
    // `, value` after the format string, a last `,` allowed: the values are every second
    // token, from the second.
    if rest.iter().step_by(2).any(|it| !it.is_punct(',')) {
        return None;
    }
    if !rest.iter().skip(1).step_by(2).all(displayable) {
        return None;
    }
    let count = rest.len() / 2;
    let used = &mut scratch.used;
    used.clear();
    used.resize(count, false);
    let mut next = 0;
    // Braces are ASCII, so every index below is on a character's boundary.
    let bytes = spec.as_bytes();
    let (mut at, mut copied) = (0, 0);
    while at < bytes.len() {
        match bytes[at] {
            brace @ (b'{' | b'}') if bytes.get(at + 1) == Some(&brace) => {
                out.push(&spec[copied..=at])?;
                at += 2;
                copied = at;
            }
            b'{' => {
                out.push(&spec[copied..at])?;
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
                if index >= count {
                    return None;
                }
                write_arg(&rest[1 + 2 * index], out, &mut scratch.arg)?;
                used[index] = true;
                at = close + 1;
                copied = at;
            }
            b'}' => return None,
            _ => at += 1,
        }
    }
    out.push(&spec[copied..])?;
    if used.contains(&false) {
        return None;
    }
    if newline {
        out.push("\n")?;
    }
    Some(())
}

/// Whether `token` is a literal [`write_arg`] writes.
fn displayable(token: &Token<'_>) -> bool {
    match token.kind {
        Kind::Str { .. } | Kind::RawStr { .. } | Kind::Char(_) => true,
        Kind::Int(value) => value <= i32::MAX as u128,
        Kind::Ident => token.text == "true" || token.text == "false",
        _ => false,
    }
}

/// A literal as `Display` shows it, added to `out`: a string's value (decoded into `scratch`
/// when it has escapes), a character, a number in decimal, `true` or `false`.
fn write_arg(token: &Token<'_>, out: &mut Printed<'_>, scratch: &mut String) -> Option<()> {
    match token.kind {
        Kind::Str { escaped: false, .. } => out.push(&token.text[1..token.text.len() - 1]),
        Kind::Str { escaped: true, .. } => {
            scratch.clear();
            token.push_value(scratch)?;
            out.push(scratch)
        }
        Kind::RawStr { value } => out.push(value),
        Kind::Char(c) => out.push(c.encode_utf8(&mut [0; 4])),
        Kind::Int(value) if value <= i32::MAX as u128 => {
            let mut digits = [0u8; 10];
            let (mut value, mut at) = (value as u32, digits.len());
            loop {
                at -= 1;
                digits[at] = b'0' + (value % 10) as u8;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            out.push(std::str::from_utf8(&digits[at..]).ok()?)
        }
        Kind::Ident if token.text == "true" || token.text == "false" => out.push(token.text),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
