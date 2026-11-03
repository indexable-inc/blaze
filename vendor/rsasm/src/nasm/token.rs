//! The preprocessor's view of a line: text cut into tokens coarse enough to
//! find directives, macro names and parameters in, and to put back together
//! unchanged.
//!
//! The assembler proper lexes a line again once the preprocessor is done
//! with it, so these tokens only have to agree with the lexer about where a
//! name, a number or a string starts and ends. Rendering them back to text is
//! what pastes: `foo%1` with `%1` bound to `bar` becomes the text `foobar`,
//! which the lexer then reads as one name, just as NASM joins adjacent tokens.

/// What kind of text a [`Tok`] is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// A run of blanks, kept as one space.
    Space,
    /// A name, including a leading `$` that marks it as not a keyword.
    Ident,
    /// A number, integer or floating-point, as written.
    Number,
    /// A quoted string, quotes included.
    Str,
    /// A preprocessor token: a `%` with what follows it, such as `%define`,
    /// `%1`, `%{-1}`, `%%loop`, `%$local`, `%+`, `%?` or `%[`.
    Pp,
    /// Anything else, one character at a time (two for `%%`).
    Other,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Tok {
    pub kind: Kind,
    pub text: String,
}

impl Tok {
    pub fn new(kind: Kind, text: impl Into<String>) -> Tok {
        Tok {
            kind,
            text: text.into(),
        }
    }

    pub fn space() -> Tok {
        Tok::new(Kind::Space, " ")
    }

    pub fn is_space(&self) -> bool {
        self.kind == Kind::Space
    }

    pub fn is(&self, text: &str) -> bool {
        self.kind == Kind::Other && self.text == text
    }

    /// The contents of a string token, without its quotes, with a backquoted
    /// string's escapes read.
    pub fn string_value(&self) -> Option<Vec<u8>> {
        if self.kind != Kind::Str {
            return None;
        }
        let q = self.text.as_bytes()[0];
        let inner = &self.text[1..];
        let inner = inner.strip_suffix(q as char).unwrap_or(inner);
        if q != b'`' {
            return Some(inner.as_bytes().to_vec());
        }
        let mut out = Vec::new();
        let b = inner.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'\\' || i + 1 >= b.len() {
                out.push(b[i]);
                i += 1;
                continue;
            }
            i += 1;
            let c = b[i];
            i += 1;
            let simple = match c {
                b'a' => Some(7),
                b'b' => Some(8),
                b't' => Some(9),
                b'n' => Some(10),
                b'v' => Some(11),
                b'f' => Some(12),
                b'r' => Some(13),
                b'e' => Some(27),
                b'0'..=b'7' | b'x' | b'u' | b'U' => None,
                other => Some(other),
            };
            if let Some(v) = simple {
                out.push(v);
                continue;
            }
            let (radix, max, start) = match c {
                b'x' => (16, 2, i),
                b'u' => (16, 4, i),
                b'U' => (16, 8, i),
                _ => (8, 3, i - 1),
            };
            let mut j = start;
            let mut v = 0u32;
            while j < b.len() && j - start < max {
                match (b[j] as char).to_digit(radix) {
                    Some(d) => v = v.wrapping_mul(radix).wrapping_add(d),
                    None => break,
                }
                j += 1;
            }
            i = j;
            if matches!(c, b'u' | b'U') {
                if let Some(ch) = char::from_u32(v) {
                    let mut tmp = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                }
            } else {
                out.push(v as u8);
            }
        }
        Some(out)
    }
}

fn name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || matches!(c, b'_' | b'.' | b'?') || c >= 0x80
}

fn name_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, b'_' | b'$' | b'#' | b'@' | b'~' | b'.' | b'?')
        || c >= 0x80
}

/// Cuts a line into tokens, stopping at a comment.
pub(crate) fn tokenize(line: &str) -> Vec<Tok> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // Slices are only ever cut at ASCII bytes, so they stay on character
    // boundaries.
    let text = |from: usize, to: usize| line[from..to].to_string();
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c == b';' {
            break;
        }
        if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            out.push(Tok::space());
            continue;
        }
        if matches!(c, b'\'' | b'"' | b'`') {
            i += 1;
            while i < b.len() && b[i] != c {
                if c == b'`' && b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push(Tok::new(Kind::Str, text(start, i)));
            continue;
        }
        if c.is_ascii_digit() || (c == b'$' && b.get(1 + i).is_some_and(|d| d.is_ascii_digit())) {
            i += 1;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric()
                    || b[i] == b'_'
                    || (b[i] == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit())))
            {
                i += 1;
            }
            out.push(Tok::new(Kind::Number, text(start, i)));
            continue;
        }
        if name_start(c) || (c == b'$' && b.get(i + 1).is_some_and(|&d| name_start(d))) {
            if c == b'.' && !b.get(i + 1).is_some_and(|&d| name_cont(d)) {
                i += 1;
                out.push(Tok::new(Kind::Other, "."));
                continue;
            }
            i += 1;
            while i < b.len() && name_cont(b[i]) {
                i += 1;
            }
            // Keep multi-byte characters whole.
            while !line.is_char_boundary(i) {
                i += 1;
            }
            out.push(Tok::new(Kind::Ident, text(start, i)));
            continue;
        }
        if c == b'%' {
            if let Some(end) = pp_token_end(b, i) {
                out.push(Tok::new(Kind::Pp, text(start, end)));
                i = end;
                continue;
            }
            if b.get(i + 1) == Some(&b'%') {
                out.push(Tok::new(Kind::Other, "%%"));
                i += 2;
                continue;
            }
        }
        let n = line[i..].chars().next().map_or(1, char::len_utf8);
        i += n;
        out.push(Tok::new(Kind::Other, text(start, i)));
    }
    // A comment leaves the blank before it behind.
    if out.last().is_some_and(Tok::is_space) {
        out.pop();
    }
    out
}

/// Where a preprocessor token starting with the `%` at `i` ends, if one does.
fn pp_token_end(b: &[u8], i: usize) -> Option<usize> {
    let next = *b.get(i + 1)?;
    let run = |mut j: usize| {
        while j < b.len() && name_cont(b[j]) {
            j += 1;
        }
        j
    };
    match next {
        // `%%local` and `%$local`, `%$$outer`.
        b'%' if b
            .get(i + 2)
            .is_some_and(|&c| name_start(c) || c.is_ascii_digit()) =>
        {
            Some(run(i + 2))
        }
        b'$' => {
            let mut j = i + 1;
            while b.get(j) == Some(&b'$') {
                j += 1;
            }
            (j < b.len() && (name_start(b[j]) || b[j].is_ascii_digit())).then(|| run(j))
        }
        b'0'..=b'9' => {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            Some(j)
        }
        // `%-1` and `%+1`, the condition-code forms, and `%+` pasting.
        b'-' if b.get(i + 2).is_some_and(u8::is_ascii_digit) => {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            Some(j)
        }
        b'+' => {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            Some(j)
        }
        b'{' => {
            let mut j = i + 2;
            let mut depth = 1;
            while j < b.len() {
                match b[j] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        b'?' => Some(if b.get(i + 2) == Some(&b'?') {
            i + 3
        } else {
            i + 2
        }),
        b'[' | b'*' => Some(i + 2),
        c if name_start(c) => Some(run(i + 1)),
        _ => None,
    }
}

/// Puts tokens back together as text, carrying out `%+` pastes.
pub(crate) fn render(toks: &[Tok]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.kind == Kind::Pp && t.text == "%+" {
            while out.ends_with(' ') {
                out.pop();
            }
            i += 1;
            while toks.get(i).is_some_and(Tok::is_space) {
                i += 1;
            }
            continue;
        }
        out.push_str(&t.text);
        i += 1;
    }
    out
}

/// The tokens without leading and trailing blanks.
pub(crate) fn trim(toks: &[Tok]) -> &[Tok] {
    let lo = toks
        .iter()
        .position(|t| !t.is_space())
        .unwrap_or(toks.len());
    let hi = toks
        .iter()
        .rposition(|t| !t.is_space())
        .map_or(lo, |i| i + 1);
    &toks[lo..hi]
}

/// Splits tokens at top-level commas; braces group, and are taken off an
/// argument they enclose entirely, as NASM does for macro arguments.
pub(crate) fn split_args(toks: &[Tok]) -> Vec<Vec<Tok>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut depth = 0i32;
    for t in toks {
        match t.kind {
            Kind::Other if t.text == "{" => depth += 1,
            Kind::Other if t.text == "}" => depth -= 1,
            Kind::Other if t.text == "," && depth == 0 => {
                out.push(unbrace(trim(&cur)).to_vec());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(t.clone());
    }
    out.push(unbrace(trim(&cur)).to_vec());
    out
}

fn unbrace(toks: &[Tok]) -> &[Tok] {
    if toks.len() >= 2 && toks[0].is("{") && toks[toks.len() - 1].is("}") {
        // Only if the first brace closes at the end.
        let mut depth = 0;
        for (i, t) in toks.iter().enumerate() {
            if t.is("{") {
                depth += 1;
            } else if t.is("}") {
                depth -= 1;
                if depth == 0 && i + 1 != toks.len() {
                    return toks;
                }
            }
        }
        return trim(&toks[1..toks.len() - 1]);
    }
    toks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<(Kind, String)> {
        tokenize(s).into_iter().map(|t| (t.kind, t.text)).collect()
    }

    #[test]
    fn cuts_directives_parameters_and_strings() {
        use Kind::*;
        assert_eq!(
            kinds("%define foo(x) [x+%1] ; comment"),
            vec![
                (Pp, "%define".into()),
                (Space, " ".into()),
                (Ident, "foo".into()),
                (Other, "(".into()),
                (Ident, "x".into()),
                (Other, ")".into()),
                (Space, " ".into()),
                (Other, "[".into()),
                (Ident, "x".into()),
                (Other, "+".into()),
                (Pp, "%1".into()),
                (Other, "]".into()),
            ]
        );
        assert_eq!(
            kinds("db 'a;b', `\\``, %%x, %$y, %{-1}"),
            vec![
                (Ident, "db".into()),
                (Space, " ".into()),
                (Str, "'a;b'".into()),
                (Other, ",".into()),
                (Space, " ".into()),
                (Str, "`\\``".into()),
                (Other, ",".into()),
                (Space, " ".into()),
                (Pp, "%%x".into()),
                (Other, ",".into()),
                (Space, " ".into()),
                (Pp, "%$y".into()),
                (Other, ",".into()),
                (Space, " ".into()),
                (Pp, "%{-1}".into()),
            ]
        );
    }

    #[test]
    fn renders_pastes() {
        assert_eq!(render(&tokenize("foo %+ bar baz")), "foobar baz");
    }

    #[test]
    fn splits_arguments_with_braces() {
        let args = split_args(&tokenize("a, {b, c}, (d,e)"));
        let texts: Vec<String> = args.iter().map(|a| render(a)).collect();
        assert_eq!(texts, vec!["a", "b, c", "(d", "e)"]);
    }

    #[test]
    fn reads_backquoted_escapes() {
        let t = &tokenize(r"`a\n\x41\101`")[0];
        assert_eq!(t.string_value().unwrap(), b"a\nAA");
    }
}
