//! Inline components in an agent's words: `<Path loc="/Volumes/Projects/blaze">blaze</Path>`, drawn as a chip (an icon
//! and the label) that a press opens. This crate is the grammar only: what a tag is, its props and its label, and how a
//! prop becomes a place to open. The vocabulary (which names exist, each one's icon and which prop says where it goes)
//! is scene data, the `ui.inline` Flecs module (harness-native-ui-prims/scene/inline.flecs), so a new component is an
//! entity, not code. The file kinds a path's icon comes from are [`files`], shared with a cell's FileRef and Icon.
//!
//! A tag is `<` and a capitalised name, props, then either `/>` or `>` words `</Name>` on the same paragraph. Anything
//! else (`Vec<T>`, `a < b`, an unclosed tag) is plain words, so ordinary prose and type names never turn into chips.
//! Prop values are strings (`"..."`, `'...'`, or the same inside braces as JSX writes them), numbers (`{12}`), or an
//! expression (`{blazePath}`), which the words cannot evaluate: [`Tag::prop`] reports it as an error naming the prop.
pub mod files;

/// A prop's value as written.
#[derive(Clone, Debug, PartialEq)]
pub enum Prop {
    /// A string: `"x"`, `'x'`, `{"x"}`, `{'x'}`, `` {`x`} ``.
    Text(String),
    /// A number: `{12}` or `12`.
    Number(f64),
    /// Anything else in braces (`{blazePath}`): a name or expression only a cell could evaluate.
    Expr(String),
}

/// One inline component: its name, its props in order, and its label (its words, or for a self-closing tag its first
/// prop's value, a path cut to its last part).
#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    pub name: String,
    pub props: Vec<(String, Prop)>,
    pub label: String,
}

impl Tag {
    /// Prop `name` as a string (a number written as one), or an error naming the tag, the prop and why.
    pub fn prop(&self, name: &str) -> Result<String, String> {
        match self.props.iter().find(|(k, _)| k == name).map(|(_, v)| v) {
            Some(Prop::Text(s)) => Ok(s.clone()),
            Some(Prop::Number(n)) => Ok(fmt_number(*n)),
            Some(Prop::Expr(e)) => Err(format!(
                "<{}> {name}={{{e}}} is an expression the words cannot evaluate: write the value as a string, {name}=\"...\"",
                self.name
            )),
            None => Err(format!("<{}> has no {name} prop", self.name)),
        }
    }

    /// Prop `name` as a line number, when there is one.
    pub fn line(&self, name: &str) -> Result<Option<u32>, String> {
        match self.props.iter().find(|(k, _)| k == name).map(|(_, v)| v) {
            None => Ok(None),
            Some(Prop::Number(n)) if *n >= 1.0 && n.fract() == 0.0 && *n <= f64::from(u32::MAX) => Ok(Some(*n as u32)),
            Some(Prop::Text(s)) if s.parse::<u32>().is_ok_and(|n| n >= 1) => Ok(s.parse().ok()),
            Some(_) => Err(format!("<{}> {name} is not a line number (1 or more)", self.name)),
        }
    }
}

fn fmt_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { n.to_string() }
}

/// The tag starting at `chars[at]` (a `<`), and the index just past it; None when what is there is not a whole tag.
#[must_use]
pub fn tag(chars: &[char], at: usize) -> Option<(Tag, usize)> {
    if chars.get(at) != Some(&'<') || !chars.get(at + 1).is_some_and(char::is_ascii_uppercase) {
        return None;
    }
    let mut i = at + 1;
    let name = ident(chars, &mut i)?;
    let mut props = Vec::new();
    loop {
        skip_space(chars, &mut i);
        match chars.get(i)? {
            '/' => {
                if chars.get(i + 1) != Some(&'>') {
                    return None;
                }
                let label = props.first().and_then(|(_, v): &(String, Prop)| match v {
                    Prop::Text(s) => Some(tail(s)),
                    Prop::Number(n) => Some(fmt_number(*n)),
                    Prop::Expr(_) => None,
                })?;
                return Some((Tag { name, props, label }, i + 2));
            }
            '>' => {
                i += 1;
                break;
            }
            _ => {
                let key = ident(chars, &mut i)?;
                skip_space(chars, &mut i);
                if chars.get(i) != Some(&'=') {
                    return None;
                }
                i += 1;
                skip_space(chars, &mut i);
                props.push((key, value(chars, &mut i)?));
            }
        }
    }
    // Words up to `</Name>`, on this paragraph, with no other tag inside.
    let close: Vec<char> = format!("</{name}>").chars().collect();
    let start = i;
    while i < chars.len() {
        if chars[i] == '\n' || (chars[i] == '<' && !chars[i..].starts_with(&close)) {
            return None;
        }
        if chars[i..].starts_with(&close) {
            let label: String = chars[start..i].iter().collect::<String>().trim().to_owned();
            if label.is_empty() {
                return None;
            }
            return Some((Tag { name, props, label }, i + close.len()));
        }
        i += 1;
    }
    None
}

fn skip_space(chars: &[char], i: &mut usize) {
    while chars.get(*i).is_some_and(|c| *c == ' ' || *c == '\t') {
        *i += 1;
    }
}

fn ident(chars: &[char], i: &mut usize) -> Option<String> {
    let start = *i;
    while chars.get(*i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-') {
        *i += 1;
    }
    (*i > start).then(|| chars[start..*i].iter().collect())
}

fn quoted(chars: &[char], i: &mut usize) -> Option<String> {
    let q = *chars.get(*i)?;
    if !matches!(q, '"' | '\'' | '`') {
        return None;
    }
    let end = (*i + 1..chars.len()).find(|&j| chars[j] == q && chars[j - 1] != '\\')?;
    let s: String = chars[*i + 1..end].iter().collect();
    *i = end + 1;
    Some(s.replace(&format!("\\{q}"), &q.to_string()))
}

fn value(chars: &[char], i: &mut usize) -> Option<Prop> {
    match chars.get(*i)? {
        '{' => {
            let end = (*i + 1..chars.len()).find(|&j| chars[j] == '}')?;
            let inner: String = chars[*i + 1..end].iter().collect::<String>().trim().to_owned();
            *i = end + 1;
            let ic: Vec<char> = inner.chars().collect();
            let mut k = 0;
            if let Some(s) = quoted(&ic, &mut k).filter(|_| k == ic.len()) {
                return Some(Prop::Text(s));
            }
            Some(inner.parse::<f64>().map_or(Prop::Expr(inner), Prop::Number))
        }
        '"' | '\'' => quoted(chars, i).map(Prop::Text),
        c if c.is_ascii_digit() => {
            let start = *i;
            while chars.get(*i).is_some_and(|c| c.is_ascii_digit() || *c == '.') {
                *i += 1;
            }
            chars[start..*i].iter().collect::<String>().parse().ok().map(Prop::Number)
        }
        _ => None,
    }
}

/// A path's last part (`/a/b/` gives `b`), what a self-closing path tag shows.
#[must_use]
pub fn tail(path: &str) -> String {
    let t = path.trim_end_matches('/');
    let last = t.rsplit('/').next().unwrap_or(t);
    if last.is_empty() { path.to_owned() } else { last.to_owned() }
}

/// What a press on a component opens, as the host's `open:` action takes it (src/open.rs): a web link as written, else a
/// path made absolute (`~` from `home`, a relative one from `cwd`, the session's directory), with `:<line>` before it when
/// the tag gives one. A target that is neither is an error naming it.
pub fn open_target(loc: &str, line: Option<u32>, cwd: &str, home: &str) -> Result<String, String> {
    if loc.starts_with("https://") || loc.starts_with("http://") {
        return Ok(loc.to_owned());
    }
    let path = if let Some(rest) = loc.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else if loc == "~" {
        home.to_owned()
    } else if loc.starts_with('/') {
        loc.to_owned()
    } else if loc.is_empty() || loc.contains("://") {
        return Err(format!("{loc:?} is not a path or an http(s) link"));
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), loc.trim_start_matches("./"))
    };
    Ok(match line {
        Some(n) => format!("{n}:{path}"),
        None => path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Option<(Tag, usize)> {
        let c: Vec<char> = s.chars().collect();
        tag(&c, 0)
    }

    /// The grammar: JSX's prop forms, a self-closing tag's label, and the words that must stay words.
    #[test]
    fn tags_and_words() {
        let (t, n) = at(r#"<Path loc="/Volumes/Projects/blaze">blaze</Path> repo"#).unwrap();
        assert_eq!((t.name.as_str(), t.label.as_str(), n), ("Path", "blaze", 48));
        assert_eq!(t.prop("loc").unwrap(), "/Volumes/Projects/blaze");
        let (t, _) = at("<Path loc={'~/a/b.rs'} line={12} />").unwrap();
        assert_eq!((t.label.as_str(), t.line("line").unwrap()), ("b.rs", Some(12)));
        let (t, _) = at("<Path loc = {blazePath}>blaze</Path>").unwrap();
        assert!(t.prop("loc").unwrap_err().contains("loc={blazePath} is an expression"));
        for words in ["<T>", "Vec<T> and more", "<Path loc=\"x\">no close", "<Path loc=\"x\">a\nb</Path>", "< Path />", "<path loc=\"x\"/>"] {
            assert!(at(words).is_none(), "{words:?} is words");
        }
    }

    #[test]
    fn targets() {
        assert_eq!(open_target("~/x", None, "/c", "/Users/a").unwrap(), "/Users/a/x");
        assert_eq!(open_target("src/main.rs", Some(3), "/c/", "/h").unwrap(), "3:/c/src/main.rs");
        assert_eq!(open_target("https://a.b/c", Some(3), "/c", "/h").unwrap(), "https://a.b/c");
        assert!(open_target("ftp://x", None, "/c", "/h").is_err());
    }
}
