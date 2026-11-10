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
//!
//! What a self-closing tag shows, and whether a mark follows its label, is the vocabulary's too ([`Form`]), installed
//! from the scene once it loads ([`install`]): `<PR repo="indexable-inc/ix" n={7652}/>` reads `#7652` because
//! `ui.inline.PR` says `label: "#{n}"`. It is process-wide because the typesetter that sets the words has no world.
//!
//! A web address is read the same way backwards: a component whose form has an `href` format (`ui.inline.PR`:
//! `https://github.com/{repo}/pull/{n}`) is what a link to such an address means, so `<Link href="https://github.com/
//! indexable-inc/ix/pull/7652">`, `[the fix](https://github.com/indexable-inc/ix/pull/7652)` and the bare address all
//! become `<PR repo="indexable-inc/ix" n={7652}/>` ([`from_url`]); the agent need not pick the component. A component
//! whose form names its `url` prop (`ui.inline.Link`: `href`) is the one links are, and a link that stays one shows its
//! host after its words ([`Tag::meta`]).
pub mod files;

use std::collections::BTreeMap;
use std::sync::RwLock;

/// What the vocabulary says of one component's words: `label`, a format over its props ([`format`]) a self-closing tag
/// shows (empty: its first prop's value, a path cut to its last part), `mark`, whether its label is followed by a mark
/// its live state picks (a pull request's CI check), `href`, the format of the web address it opens (read backwards by
/// [`from_url`]), and `url`, the prop that holds a web address when the component is a link (`ui.inline.Link`'s `href`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Form {
    pub label: String,
    pub mark: bool,
    /// Not a chip but the words themselves, cited: no icon, the surrounding face, a dotted underline (`ui.inline.Cite`).
    pub plain: bool,
    pub href: String,
    pub url: String,
}

// alloc: the vocabulary lives for the process; the scene installs it once, words read it.
static FORMS: RwLock<BTreeMap<String, Form>> = RwLock::new(BTreeMap::new());

/// Make `forms` (by component name) the vocabulary's words, replacing what was installed.
pub fn install(forms: BTreeMap<String, Form>) {
    *FORMS.write().unwrap_or_else(std::sync::PoisonError::into_inner) = forms;
}

/// Whether `name` is a component of the installed vocabulary (`Path`, `Cite`, ...).
#[must_use]
pub fn known(name: &str) -> bool {
    FORMS.read().unwrap_or_else(std::sync::PoisonError::into_inner).contains_key(name)
}

fn form(name: &str) -> Option<Form> {
    FORMS.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(name).cloned()
}

/// The props that make `fmt` (an `href` format, `https://github.com/{repo}/pull/{n}`) read `url`: each `{prop}` takes
/// the text up to the literal after it (the last one up to the address's end, its query or its fragment), never empty
/// and at most two path parts (`{repo}` before `/pull/` takes `owner/name`; the last prop one). A longer
/// address that goes on past the format by a path (`/pull/7652/files`) still reads. None when it does not fit.
#[must_use]
pub fn unformat(fmt: &str, url: &str) -> Option<Vec<(String, String)>> {
    // The format's pieces: literals and prop names, alternating from a literal (maybe empty).
    let mut lits: Vec<&str> = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    let mut rest = fmt;
    loop {
        match rest.find('{') {
            Some(i) => {
                let close = rest[i..].find('}')? + i;
                lits.push(&rest[..i]);
                names.push(&rest[i + 1..close]);
                rest = &rest[close + 1..];
            }
            None => {
                lits.push(rest);
                break;
            }
        }
    }
    if names.is_empty() || fmt.contains("{{") {
        return None;
    }
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let mut at = url.strip_prefix(lits[0])?;
    let mut props = Vec::new();
    for (k, name) in names.iter().enumerate() {
        let next = lits[k + 1];
        let value = if next.is_empty() {
            // The last prop: to the end, or to a path that goes on past the format.
            let v = at.split('/').next().unwrap_or(at);
            at = &at[v.len()..];
            v
        } else {
            let i = at.find(next)?;
            let v = &at[..i];
            // At most two path parts (`owner/name`), so a longer path is no fit.
            if v.matches('/').count() > 1 {
                return None;
            }
            at = &at[i + next.len()..];
            v
        };
        if value.is_empty() {
            return None;
        }
        props.push(((*name).to_owned(), value.to_owned()));
    }
    (at.is_empty() || at.starts_with('/')).then_some(props)
}

/// The component a link to `url` is, by the vocabulary's `href` formats ([`unformat`]), as a tag of its props: its
/// label `words` when it was written with words of its own (not the address again), else its form's. None when no
/// component's address fits, or one fits whose label its props cannot fill.
#[must_use]
pub fn from_url(url: &str, words: Option<&str>) -> Option<Tag> {
    let forms = FORMS.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (name, form, props) = forms
        .iter()
        .filter(|(_, f)| !f.href.is_empty())
        .find_map(|(n, f)| unformat(&f.href, url).map(|p| (n.clone(), f.clone(), p)))?;
    drop(forms);
    let props: Vec<(String, Prop)> = props
        .into_iter()
        .map(|(k, v)| {
            let p = if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) && v.len() < 16 {
                v.parse::<f64>().map_or(Prop::Text(v.clone()), Prop::Number)
            } else {
                Prop::Text(v)
            };
            (k, p)
        })
        .collect();
    let mut tag = Tag { name, props, label: String::new(), mark: form.mark, plain: form.plain, meta: String::new() };
    let words = words.map(str::trim).filter(|w| !w.is_empty() && !w.contains("://") && *w != bare(url));
    tag.label = match words {
        Some(w) => w.to_owned(),
        None if form.label.is_empty() => return None,
        None => format(&form.label, &tag).ok()?,
    };
    Some(tag)
}

/// A web address's host, `www.` off (`https://www.example.com/a` gives `example.com`).
#[must_use]
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let h = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    h.strip_prefix("www.").unwrap_or(h)
}

/// A web address as words: without its scheme, `www.` or a trailing `/` (`example.com/doc`).
#[must_use]
pub fn bare(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    rest.strip_suffix('/').unwrap_or(rest)
}

/// What a tag whose form is a link's (`url` names its prop) becomes: the component its address is, when one's `href`
/// fits ([`from_url`]: its words kept), else itself with its host after its words (none when its words are the address).
fn as_link(mut tag: Tag, form: &Form, own_words: bool) -> Tag {
    let Ok(url) = tag.prop(&form.url) else { return tag };
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return tag;
    }
    if let Some(t) = from_url(&url, own_words.then_some(tag.label.as_str())) {
        return t;
    }
    if own_words && tag.label != bare(&url) && !tag.label.contains("://") {
        tag.meta = host(&url).to_owned();
    } else {
        tag.label = bare(&url).to_owned();
    }
    tag
}

/// `fmt` with each `{prop}` replaced by `tag`'s prop of that name ([`Tag::prop`]); an error naming the tag and the prop
/// when it has none or it is an expression. `{{` and `}}` are a brace.
pub fn format(fmt: &str, tag: &Tag) -> Result<String, String> {
    let mut out = String::new(); // alloc: the formatted words outlive this call.
    let mut rest = fmt;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        let Some(end) = tail.find('}').filter(|_| tail.starts_with('{')) else {
            return Err(format!("<{}>'s format {fmt:?} has an unmatched brace", tag.name));
        };
        out.push_str(&tag.prop(&tail[1..end])?);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

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

/// One inline component: its name, its props in order, its label (its words, or for a self-closing tag its [`Form`]'s
/// label, or with none its first prop's value, a path cut to its last part), whether a mark follows the label, and
/// `meta`, quiet words after the label (a link's host; empty: none).
#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    pub name: String,
    pub props: Vec<(String, Prop)>,
    pub label: String,
    pub mark: bool,
    /// Its form's `plain`: the words themselves, cited, not a chip.
    pub plain: bool,
    pub meta: String,
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
                let form = form(&name).unwrap_or_default();
                let mut tag = Tag { name, props, label: String::new(), mark: form.mark, plain: form.plain, meta: String::new() };
                // A label its props cannot fill (a missing prop) leaves the tag as words, shown as written.
                tag.label = if form.label.is_empty() && !form.url.is_empty() {
                    // A link with no words: its address, shown bare (or what it is, `as_link`).
                    bare(&tag.prop(&form.url).ok()?).to_owned()
                } else if form.label.is_empty() {
                    tag.props.first().and_then(|(_, v)| match v {
                        Prop::Text(s) => Some(tail(s)),
                        Prop::Number(n) => Some(fmt_number(*n)),
                        Prop::Expr(_) => None,
                    })?
                } else {
                    format(&form.label, &tag).ok()?
                };
                let tag = if form.url.is_empty() { tag } else { as_link(tag, &form, false) };
                return Some((tag, i + 2));
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
            let form = form(&name).unwrap_or_default();
            let tag = Tag { name, props, label, mark: form.mark, plain: form.plain, meta: String::new() };
            let tag = if form.url.is_empty() { tag } else { as_link(tag, &form, true) };
            return Some((tag, i + close.len()));
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

    /// A form's label over the props, its mark, and a tag its format cannot fill staying words.
    #[test]
    fn forms() {
        install(vocabulary());
        let (t, _) = at(r#"<PR repo="indexable-inc/ix" n={7652}/>"#).unwrap();
        assert_eq!((t.label.as_str(), t.mark), ("#7652", true));
        assert_eq!(format("https://github.com/{repo}/pull/{n}", &t).unwrap(), "https://github.com/indexable-inc/ix/pull/7652");
        assert_eq!(format("{{n}}", &t).unwrap(), "{n}");
        assert!(format("{n", &t).unwrap_err().contains("unmatched brace"));
        assert!(at(r#"<PR repo="indexable-inc/ix"/>"#).is_none(), "no n: words");
        let (t, _) = at(r#"<Path loc="a/b"/>"#).unwrap();
        assert_eq!((t.label.as_str(), t.mark), ("b", false));
    }

    /// The scene's words for the tests: a pull request and an issue by their addresses, and a link.
    fn vocabulary() -> BTreeMap<String, Form> {
        let gh = |kind: &str, mark| Form {
            label: "#{n}".to_owned(),
            mark,
            href: format!("https://github.com/{{repo}}/{kind}/{{n}}"),
            url: String::new(),
            plain: false,
        };
        BTreeMap::from([
            ("PR".to_owned(), gh("pull", true)),
            ("Issue".to_owned(), gh("issues", false)),
            ("Link".to_owned(), Form { url: "href".to_owned(), ..Form::default() }),
        ])
    }

    /// A link to a GitHub pull request or issue is one, its own words kept; other links show their host after them.
    #[test]
    fn links_become_what_they_point_at() {
        install(vocabulary());
        let pr = "https://github.com/indexable-inc/blaze/pull/1770";
        let t = from_url(pr, None).unwrap();
        assert_eq!((t.name.as_str(), t.label.as_str(), t.mark), ("PR", "#1770", true));
        assert_eq!((t.prop("repo").unwrap(), t.prop("n").unwrap()), ("indexable-inc/blaze".to_owned(), "1770".to_owned()));
        assert_eq!(from_url(&format!("{pr}/files?w=1"), Some("the charts")).unwrap().label, "the charts");
        assert_eq!(from_url("https://github.com/a/b/issues/9#top", None).unwrap().name, "Issue");
        for no in ["https://github.com/a/b", "https://github.com/a/b/pull/", "https://gitlab.com/a/b/pull/1", "https://github.com/a/b/pulls"] {
            assert!(from_url(no, None).is_none(), "{no} is no component");
        }
        let (t, _) = at(&format!(r#"<Link href="{pr}">charts</Link>"#)).unwrap();
        assert_eq!((t.name.as_str(), t.label.as_str()), ("PR", "charts"));
        let (t, _) = at(&format!(r#"<Link href="{pr}"/>"#)).unwrap();
        assert_eq!((t.name.as_str(), t.label.as_str()), ("PR", "#1770"));
        let (t, _) = at(r#"<Link href="https://www.example.com/doc/">the design doc</Link>"#).unwrap();
        assert_eq!((t.name.as_str(), t.label.as_str(), t.meta.as_str()), ("Link", "the design doc", "example.com"));
        let (t, _) = at(r#"<Link href="https://www.example.com/doc/"/>"#).unwrap();
        assert_eq!((t.label.as_str(), t.meta.as_str()), ("example.com/doc", ""));
    }

    #[test]
    fn targets() {
        assert_eq!(open_target("~/x", None, "/c", "/Users/a").unwrap(), "/Users/a/x");
        assert_eq!(open_target("src/main.rs", Some(3), "/c/", "/h").unwrap(), "3:/c/src/main.rs");
        assert_eq!(open_target("https://a.b/c", Some(3), "/c", "/h").unwrap(), "https://a.b/c");
        assert!(open_target("ftp://x", None, "/c", "/h").is_err());
    }
}
