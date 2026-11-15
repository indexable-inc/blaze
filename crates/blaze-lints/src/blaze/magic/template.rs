//! Format templates as `repeated_format` compares them: the text of a `format_args!` template
//! with each placeholder made one, so `"{}: cannot read {path}"` and `"{0}: cannot read {1:?}"`
//! are one sentence.

use std::alloc::Global;

use intern::Symbol;

/// The fewest words outside its placeholders a template has for two copies of it to be one
/// sentence written twice. Below it a template is a label or a layout (`"{}: {}"`, `"{} ms"`,
/// `"took {}"`, `"{} of {}"`) whose copies a shared function would not make clearer and which
/// a reader does not search for; at three a template says what happened to what
/// (`"{} cannot be read from {}"` is four). Two words is where labels and sentences overlap
/// (`"took {} ms"` beside `"cannot read {}"`), so two is left alone: a miss, not a false finding.
pub(crate) const TEMPLATE_WORDS_FLOOR: usize = 3;

/// The placeholder every `{..}` becomes.
const PLACEHOLDER: &[u8] = b"{}";

/// The template written as the string token `token` (its quotes and any `r#` included), each
/// placeholder made [`PLACEHOLDER`], `{{` and `}}` kept, trimmed. `None` for a token that is
/// not a string, or a `{` that is never closed.
pub(crate) fn normalise(token: &str) -> Option<Symbol> {
    let inner = unquote(token)?;
    let bytes = inner.as_bytes();
    let mut out: Vec<u8, Global> = Vec::with_capacity_in(bytes.len(), Global);
    let mut at = 0;
    while at < bytes.len() {
        match (bytes[at], bytes.get(at + 1)) {
            (b'{', Some(b'{')) | (b'}', Some(b'}')) => {
                out.extend_from_slice(&bytes[at..at + 2]);
                at += 2;
            }
            (b'{', _) => {
                let close = bytes[at..].iter().position(|&it| it == b'}')?;
                out.extend_from_slice(PLACEHOLDER);
                at += close + 1;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    // Only whole ASCII runs were replaced, so what stays is UTF-8.
    let text = core::str::from_utf8(&out).ok()?;
    Some(Symbol::intern(text.trim()))
}

/// The text between the quotes of a string token: `"a"`, `r"a"`, `r#"a"#`.
fn unquote(token: &str) -> Option<&str> {
    let raw = token.strip_prefix('r');
    let token = raw.unwrap_or(token);
    let hashes = token.bytes().take_while(|&it| it == b'#').count();
    let token = token.get(hashes..token.len().checked_sub(hashes)?)?;
    token.strip_prefix('"')?.strip_suffix('"')
}

/// The words of a normalised template outside its placeholders: runs of letters, an escape
/// (`\n`, `\t`) not one.
pub(crate) fn words(text: &str) -> usize {
    let mut count = 0;
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
            in_word = false;
            continue;
        }
        let letter = c.is_alphabetic();
        if letter && !in_word {
            count += 1;
        }
        in_word = letter;
    }
    count
}
