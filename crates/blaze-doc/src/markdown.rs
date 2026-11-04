//! Docs as HTML: rustdoc's Markdown (pulldown-cmark with its options), headings with anchors,
//! Rust examples highlighted with their hidden `# ` lines left out, intra-doc links pointing
//! where they resolved.
//!
//! The code block attributes and hidden lines follow `html/markdown.rs` in librustdoc at rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (`LangString::parse`, `map_line`).

use blaze_hash::HashMap;
use pulldown_cmark::{
    BrokenLink, BrokenLinkCallback, CodeBlockKind, CowStr, Event, HeadingLevel, LinkType, Parser,
    Tag, TagEnd,
};
use std::alloc::Global;

use crate::{
    html::{Writer, escape},
    links,
    model::Doc,
};

/// The ids a page has used, so a heading in docs gets one of its own.
#[derive(Default)]
pub(crate) struct Ids {
    used: HashMap<String, usize, Global>,
}

impl Ids {
    pub(crate) fn unique(&mut self, id: &str) -> String {
        match self.used.get_mut(id) {
            None => {
                self.used.insert(id.to_owned(), 0);
                id.to_owned()
            }
            Some(count) => {
                *count += 1;
                let candidate = format!("{id}-{count}");
                self.used.insert(candidate.clone(), 0);
                candidate
            }
        }
    }
}

/// How links and headings in one block of docs render.
pub(crate) struct Context<'a, 'w> {
    /// The page: a target's URL from it ([`Writer::url`], `None` where it has no documentation
    /// to point at) and the title a resolved link shows on hover ([`Writer::title`]).
    pub(crate) writer: &'a Writer<'w>,
    /// How far down a `#` heading goes: 1 makes it an `<h2>`.
    pub(crate) heading_offset: u32,
}

fn parser<'a, F: BrokenLinkCallback<'a>>(markdown: &'a str, callback: F) -> Parser<'a, F> {
    Parser::new_with_broken_link_callback(markdown, links::options(), Some(callback))
}

/// The docs as a block of HTML.
pub(crate) fn render(doc: &Doc, cx: &Context<'_, '_>, ids: &mut Ids) -> String {
    let mut callback =
        |broken: BrokenLink<'_>| Some((broken.reference.to_string().into(), "".into()));
    let events: Vec<Event<'_>> = parser(&doc.markdown, &mut callback).collect();
    let events = rewrite(events, doc, cx, Some(ids), false);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events.into_iter());
    out
}

/// The first paragraph as one line of HTML, for lists of items.
pub(crate) fn summary(doc: &Doc, cx: &Context<'_, '_>) -> String {
    let first = crate::model::first_paragraph(&doc.markdown);
    // A reference link of the first paragraph is often defined further down.
    let mut none = |_: BrokenLink<'_>| None;
    let whole = parser(&doc.markdown, &mut none);
    let defs = whole.reference_definitions();
    let mut callback = |broken: BrokenLink<'_>| match defs.get(&broken.reference) {
        Some(def) => Some((def.dest.clone(), def.title.clone().unwrap_or_else(|| "".into()))),
        None => Some((broken.reference.to_string().into(), "".into())),
    };
    let events: Vec<Event<'_>> = parser(first, &mut callback).collect();
    let events = rewrite(events, doc, cx, None, true);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events.into_iter());
    out
}

/// The first paragraph as plain text, at most about `limit` characters, for the search index
/// and the page's description.
pub(crate) fn plain_summary(markdown: &str, limit: usize) -> String {
    let first = crate::model::first_paragraph(markdown);
    let mut callback =
        |broken: BrokenLink<'_>| Some((broken.reference.to_string().into(), "".into()));
    let mut out = String::new();
    for event in parser(first, &mut callback) {
        match event {
            Event::Text(text) | Event::Code(text) => out.push_str(&text),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            _ => {}
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() <= limit {
        return out;
    }
    let mut cut: String = out.chars().take(limit).collect();
    if let Some(space) = cut.rfind(' ') {
        cut.truncate(space);
    }
    cut.push('…');
    cut
}

fn rewrite<'a>(
    events: Vec<Event<'a>>,
    doc: &Doc,
    cx: &Context<'_, '_>,
    mut ids: Option<&mut Ids>,
    inline: bool,
) -> Vec<Event<'a>> {
    let mut out: Vec<Event<'a>> = Vec::with_capacity(events.len());
    let mut iter = events.into_iter().peekable();
    while let Some(event) = iter.next() {
        match event {
            Event::Start(Tag::Link { link_type: kind, dest_url: dest, title, id }) => {
                let mut inner = vec![];
                for event in iter.by_ref() {
                    if matches!(event, Event::End(TagEnd::Link)) {
                        break;
                    }
                    inner.push(event);
                }
                let inner = rewrite(inner, doc, cx, None, true);
                let key = dest.to_string();
                match doc.links.get(&key) {
                    Some(target) => {
                        let inner = match doc.texts.get(&key) {
                            Some(text) if shows_link_itself(&inner, &key) => {
                                let had_code = inner.iter().any(|it| matches!(it, Event::Code(_)));
                                let text: CowStr<'a> = text.clone().into();
                                vec![if had_code { Event::Code(text) } else { Event::Text(text) }]
                            }
                            _ => inner,
                        };
                        match cx.writer.url(target) {
                            Some(url) => {
                                out.push(Event::Start(Tag::Link {
                                    link_type: LinkType::Inline,
                                    dest_url: url.into(),
                                    title: Writer::title(target).into(),
                                    id: "".into(),
                                }));
                                out.extend(inner);
                                out.push(Event::End(TagEnd::Link));
                            }
                            None => out.extend(inner),
                        }
                    }
                    None if matches!(
                        kind,
                        LinkType::ShortcutUnknown
                            | LinkType::CollapsedUnknown
                            | LinkType::ReferenceUnknown
                    ) =>
                    {
                        // Not a link after all: the brackets stay, as rustdoc leaves them.
                        out.push(Event::Text("[".into()));
                        out.extend(inner);
                        out.push(Event::Text("]".into()));
                        if kind == LinkType::CollapsedUnknown {
                            out.push(Event::Text("[]".into()));
                        }
                    }
                    None => {
                        out.push(Event::Start(Tag::Link {
                            link_type: kind,
                            dest_url: dest,
                            title,
                            id,
                        }));
                        out.extend(inner);
                        out.push(Event::End(TagEnd::Link));
                    }
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                let mut inner = vec![];
                for event in iter.by_ref() {
                    if matches!(event, Event::End(TagEnd::Heading(_))) {
                        break;
                    }
                    inner.push(event);
                }
                let inner = rewrite(inner, doc, cx, None, true);
                let mut text = String::new();
                for event in &inner {
                    if let Event::Text(it) | Event::Code(it) = event {
                        text.push_str(it);
                    }
                }
                let mut body = String::new();
                pulldown_cmark::html::push_html(&mut body, inner.into_iter());
                if inline {
                    out.push(Event::Html(body.into()));
                    continue;
                }
                let level = (heading_number(level) + cx.heading_offset).min(6);
                let id = match ids.as_deref_mut() {
                    Some(ids) => ids.unique(&slug(&text)),
                    None => slug(&text),
                };
                out.push(Event::Html(
                    format!(
                        "<h{level} id=\"{id}\"><a class=\"doc-anchor\" href=\"#{id}\">§</a>{body}</h{level}>\n"
                    )
                    .into(),
                ));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let mut text = String::new();
                for event in iter.by_ref() {
                    match event {
                        Event::End(TagEnd::CodeBlock) => break,
                        Event::Text(it) => text.push_str(&it),
                        _ => {}
                    }
                }
                if inline {
                    continue;
                }
                out.push(Event::Html(code_block(&kind, &text).into()));
            }
            Event::Start(Tag::Paragraph) | Event::End(TagEnd::Paragraph) if inline => {}
            Event::Start(Tag::Image { .. }) if inline => {
                // An image in a summary line shows as its text.
                for event in iter.by_ref() {
                    match event {
                        Event::End(TagEnd::Image) => break,
                        Event::Text(it) => out.push(Event::Text(it)),
                        _ => {}
                    }
                }
            }
            Event::SoftBreak if inline => out.push(Event::Text(" ".into())),
            Event::Html(ref html) | Event::InlineHtml(ref html) if inline => {
                // Block HTML (`<details>`, `<div>`) does not belong in a line.
                if !html.trim_start().starts_with("<div")
                    && !html.trim_start().starts_with("<details")
                {
                    out.push(event);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether a link's text is the link itself (`[Foo]`, `` [`Foo`] ``), which rustdoc shows
/// without a disambiguator.
fn shows_link_itself(inner: &[Event<'_>], key: &str) -> bool {
    let mut text = String::new();
    for event in inner {
        match event {
            Event::Text(it) | Event::Code(it) => text.push_str(it),
            _ => return false,
        }
    }
    text == key.replace('`', "")
}

fn heading_number(level: HeadingLevel) -> u32 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// rustdoc's heading id: "Hello, world!" is `hello-world`.
pub(crate) fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            if c.is_ascii() {
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        } else if c.is_whitespace() && c.is_ascii() {
            out.push('-');
        }
    }
    out
}

/// What a code block's info string says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LangString {
    pub(crate) rust: bool,
    pub(crate) ignore: bool,
    pub(crate) no_run: bool,
    pub(crate) should_panic: bool,
    pub(crate) compile_fail: bool,
    /// The platforms of `ignore-<platform>`; empty for a plain `ignore`.
    pub(crate) ignore_on: Vec<String>,
    /// The year of an `edition2021`.
    pub(crate) edition: Option<String>,
    /// `test_harness`: the example's own `#[test]`s are run in a harness.
    pub(crate) test_harness: bool,
    /// `standalone_crate`: the example is never merged with others.
    pub(crate) standalone_crate: bool,
    /// The error codes a `compile_fail` example must fail with, like `E0308`.
    pub(crate) error_codes: Vec<String>,
    pub(crate) unknown: Vec<String>,
    pub(crate) classes: Vec<String>,
}

impl LangString {
    /// rustdoc's `LangString::parse` with error codes read (a nightly's rustdoc reads them):
    /// the info string split by its `TagIterator` ([`tokens`]), each word as rustdoc takes it.
    /// An info string that does not parse is no Rust, as rustdoc has it.
    pub(crate) fn parse(info: &str) -> LangString {
        let mut out = LangString { rust: true, ..LangString::default() };
        let (mut seen_rust, mut seen_other, mut seen_custom) = (false, false, false);
        let (tokens, error) = tokens(info);
        for token in tokens {
            let token = match token {
                Token::Lang(it) => it,
                Token::Class(it) | Token::KeyValue("class", it) => {
                    out.classes.push(it.to_owned());
                    continue;
                }
                Token::KeyValue(..) => continue,
            };
            match token {
                "should_panic" => {
                    out.should_panic = true;
                    seen_rust = !seen_other;
                }
                "no_run" => {
                    out.no_run = true;
                    seen_rust = !seen_other;
                }
                "ignore" => {
                    out.ignore = true;
                    seen_rust = !seen_other;
                }
                x if x.starts_with("ignore-") => {
                    out.ignore_on.push(x["ignore-".len()..].to_owned());
                    seen_rust = !seen_other;
                }
                "rust" => seen_rust = true,
                "custom" => seen_custom = true,
                "test_harness" => {
                    out.test_harness = true;
                    seen_rust = !seen_other || seen_rust;
                }
                "compile_fail" => {
                    out.compile_fail = true;
                    out.no_run = true;
                    seen_rust = !seen_other || seen_rust;
                }
                "standalone_crate" => {
                    out.standalone_crate = true;
                    seen_rust = !seen_other || seen_rust;
                }
                // An edition rustc does not know is none; either way the word says nothing of
                // whether the block is Rust.
                x if x.starts_with("edition") => {
                    let year = &x["edition".len()..];
                    out.edition = matches!(year, "2015" | "2018" | "2021" | "2024" | "future")
                        .then(|| year.to_owned());
                }
                // `E` and four characters: an error code when they are a number (`u32`'s
                // parse, which takes a leading `+`), else a word of another language.
                x if x.starts_with('E') && x.len() == 5 => {
                    if x[1..].parse::<u32>().is_ok() {
                        out.error_codes.push(x.to_owned());
                        seen_rust = !seen_other || seen_rust;
                    } else {
                        seen_other = true;
                    }
                }
                other => {
                    seen_other = true;
                    out.unknown.push(other.to_owned());
                }
            }
        }
        // `ignore-foo` overrides `ignore`.
        if !out.ignore_on.is_empty() {
            out.ignore = false;
        }
        out.rust &= !seen_custom && (!seen_other || seen_rust) && !error;
        out
    }
}

/// A token of a code block's info string (rustdoc's `LangStringToken`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token<'a> {
    Lang(&'a str),
    /// `.class` in a `{ }` block.
    Class(&'a str),
    /// `key=value` in a `{ }` block.
    KeyValue(&'a str, &'a str),
}

fn is_leading_char(c: char) -> bool {
    c == '_' || c == '-' || c == ':' || c.is_ascii_alphabetic() || c.is_ascii_digit()
}

fn is_bareword_char(c: char) -> bool {
    is_leading_char(c) || ".!#$%&*+/;<>?@^|~".contains(c)
}

fn is_separator(c: char) -> bool {
    c == ' ' || c == ',' || c == '\t'
}

/// rustdoc's `TagIterator` over `data`: its tokens up to the first error, and whether there
/// was one. Words are separated by spaces, commas and tabs; a `"quoted"` word may hold any
/// other character; `{ .class key=value }` is a block of attributes; `(...)` is a comment.
fn tokens(data: &str) -> (Vec<Token<'_>>, bool) {
    let mut tags =
        Tags { inner: data.char_indices().peekable(), data, in_block: false, error: false };
    // alloc: once per code block, its few words
    let mut out = Vec::new();
    while let Some(token) = tags.next() {
        out.push(token);
    }
    (out, tags.error)
}

struct Tags<'a> {
    inner: std::iter::Peekable<std::str::CharIndices<'a>>,
    data: &'a str,
    in_block: bool,
    error: bool,
}

impl<'a> Tags<'a> {
    fn skip_separators(&mut self) -> Option<usize> {
        while let Some(&(pos, c)) = self.inner.peek() {
            if !is_separator(c) {
                return Some(pos);
            }
            self.inner.next();
        }
        None
    }

    /// From after the opening quote at `start`: the text up to the closing one.
    fn string(&mut self, start: usize) -> Option<(usize, usize)> {
        for (pos, c) in self.inner.by_ref() {
            if c == '"' {
                return Some((start + 1, pos));
            }
        }
        self.error = true;
        None
    }

    fn class(&mut self, start: usize) -> Option<Token<'a>> {
        while let Some(&(pos, c)) = self.inner.peek() {
            if is_bareword_char(c) {
                self.inner.next();
                continue;
            }
            let class = &self.data[start + 1..pos];
            if class.is_empty() {
                self.error = true;
                return None;
            }
            return self.after_token().then_some(Token::Class(class));
        }
        let class = &self.data[start + 1..];
        if class.is_empty() {
            self.error = true;
            return None;
        }
        self.after_token().then_some(Token::Class(class))
    }

    fn word(&mut self, start: usize) -> Option<(usize, usize)> {
        while let Some(&(pos, c)) = self.inner.peek() {
            if !is_bareword_char(c) {
                return Some((start, pos));
            }
            self.inner.next();
        }
        self.error = true;
        None
    }

    fn key_value(&mut self, c: char, start: usize) -> Option<Token<'a>> {
        let key = if c == '"' { self.string(start)? } else { self.word(start)? };
        if key.0 == key.1 {
            self.error = true;
            return None;
        }
        if self.inner.next().map(|it| it.1) != Some('=') {
            self.error = true;
            return None;
        }
        let value = match self.inner.next() {
            Some((pos, '"')) => self.string(pos)?,
            Some((pos, c)) if is_bareword_char(c) => self.word(pos)?,
            _ => {
                self.error = true;
                return None;
            }
        };
        if value.0 == value.1 {
            self.error = true;
            return None;
        }
        self.after_token()
            .then(|| Token::KeyValue(&self.data[key.0..key.1], &self.data[value.0..value.1]))
    }

    /// Whether what follows a token may: a `}`, a separator, a comment or the end.
    fn after_token(&mut self) -> bool {
        match self.inner.peek() {
            Some(&(_, c)) if c == '}' || is_separator(c) || c == '(' => true,
            Some(_) => {
                self.error = true;
                false
            }
            None => true,
        }
    }

    fn in_block(&mut self) -> Option<Token<'a>> {
        match self.inner.next() {
            Some((_, '}')) => {
                self.in_block = false;
                self.next()
            }
            Some((pos, '.')) => self.class(pos),
            Some((pos, c)) if c == '"' || is_leading_char(c) => self.key_value(c, pos),
            _ => {
                self.error = true;
                None
            }
        }
    }

    fn skip_comment(&mut self) -> bool {
        for (_, c) in self.inner.by_ref() {
            if c == ')' {
                return true;
            }
        }
        self.error = true;
        false
    }

    fn outside_block(&mut self, start: usize) -> Option<Token<'a>> {
        while let Some((pos, c)) = self.inner.next() {
            if c == '"' {
                if pos != start {
                    self.error = true;
                    return None;
                }
                let (from, to) = self.string(pos)?;
                if let Some(&(_, c)) = self.inner.peek()
                    && c != '{'
                    && !is_separator(c)
                    && c != '('
                {
                    self.error = true;
                    return None;
                }
                return Some(Token::Lang(&self.data[from..to]));
            } else if c == '{' {
                self.in_block = true;
                return self.next();
            } else if is_separator(c) {
                if pos != start {
                    return Some(Token::Lang(&self.data[start..pos]));
                }
                return self.next();
            } else if c == '(' {
                if !self.skip_comment() {
                    return None;
                }
                if pos != start {
                    return Some(Token::Lang(&self.data[start..pos]));
                }
                return self.next();
            } else if (pos == start && is_leading_char(c)) || (pos != start && is_bareword_char(c))
            {
                continue;
            } else {
                self.error = true;
                return None;
            }
        }
        let token = &self.data[start..];
        (!token.is_empty()).then_some(Token::Lang(token))
    }

    fn next(&mut self) -> Option<Token<'a>> {
        if self.error {
            return None;
        }
        let Some(start) = self.skip_separators() else {
            if self.in_block {
                self.error = true;
            }
            return None;
        };
        match self.in_block {
            true => self.in_block(),
            false => self.outside_block(start),
        }
    }
}

/// A doctest line rustdoc leaves out of the rendered example: `# ` lines and a lone `#`; `##`
/// shows as `#`.
fn shown_line(line: &str) -> Option<std::borrow::Cow<'_, str>> {
    let trimmed = line.trim();
    if trimmed.starts_with("##") {
        Some(std::borrow::Cow::Owned(line.replacen("##", "#", 1)))
    } else if trimmed.starts_with("# ") || trimmed == "#" {
        None
    } else {
        Some(std::borrow::Cow::Borrowed(line))
    }
}

/// A doctest line as it is compiled: a hidden `# ` line and a lone `#` are code without the
/// mark (rustdoc's `map_line`), and `##` is a line that starts with `#`.
pub(crate) fn tested_line(line: &str) -> std::borrow::Cow<'_, str> {
    let trimmed = line.trim();
    if trimmed.starts_with("##") {
        std::borrow::Cow::Owned(line.replacen("##", "#", 1))
    } else if let Some(hidden) = trimmed.strip_prefix("# ") {
        std::borrow::Cow::Borrowed(hidden)
    } else if trimmed == "#" {
        std::borrow::Cow::Borrowed("")
    } else {
        std::borrow::Cow::Borrowed(line)
    }
}

fn code_block(kind: &CodeBlockKind<'_>, text: &str) -> String {
    let lang = match kind {
        CodeBlockKind::Fenced(info) => LangString::parse(info),
        CodeBlockKind::Indented => LangString { rust: true, ..LangString::default() },
    };
    let text = text.strip_suffix('\n').unwrap_or(text);
    if !lang.rust {
        let class = lang.unknown.first().map(|it| format!("language-{it}")).unwrap_or_default();
        let classes = std::iter::once(class)
            .chain(lang.classes.iter().cloned())
            .filter(|it| !it.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return format!(
            "<div class=\"example-wrap\"><pre class=\"{classes}\"><code>{}</code></pre></div>\n",
            escape(text)
        );
    }
    let shown: Vec<_> = text.lines().filter_map(shown_line).collect();
    let code = shown.join("\n");
    let (wrap, tooltip) = if lang.ignore {
        (" ignore", Some("This example is not tested".to_owned()))
    } else if !lang.ignore_on.is_empty() {
        (
            " ignore",
            Some(format!(
                "This example is not tested on {}",
                stdx::list::list(&lang.ignore_on, stdx::list::Join::Or, false)
            )),
        )
    } else if lang.compile_fail {
        (" compile_fail", Some("This example deliberately fails to compile".to_owned()))
    } else if lang.should_panic {
        (" should_panic", Some("This example panics".to_owned()))
    } else if let Some(edition) = &lang.edition {
        (" edition", Some(format!("This example runs with edition {edition}")))
    } else {
        ("", None)
    };
    let tooltip = tooltip
        .map(|it| format!("<a href=\"#\" class=\"tooltip\" title=\"{it}\">ⓘ</a>"))
        .unwrap_or_default();
    format!(
        "<div class=\"example-wrap{wrap}\">{tooltip}<pre class=\"rust rust-example-rendered\"><code>{}</code></pre></div>\n",
        crate::highlight::rust(&code)
    )
}

#[cfg(test)]
mod tests {
    use super::{LangString, plain_summary, slug};

    #[test]
    fn lang_strings_follow_rustdoc() {
        assert!(LangString::parse("").rust);
        assert!(LangString::parse("rust,no_run").rust);
        assert!(LangString::parse("ignore").rust);
        assert!(!LangString::parse("text").rust);
        assert!(!LangString::parse("sh").rust);
        assert!(LangString::parse("compile_fail,E0277").rust);
        assert!(LangString::parse("text,ignore").ignore);
        assert!(!LangString::parse("text,ignore").rust);
        // rustdoc's `TagIterator`: attribute blocks, comments, quotes, and an error is no Rust.
        let it = LangString::parse("rust {.foo class=bar} (a comment, here) \"no_run\"");
        assert!(it.rust && it.no_run && it.classes == ["foo", "bar"] && it.unknown.is_empty());
        assert!(!LangString::parse("rust{").rust);
        assert!(!LangString::parse("rust,no_run=").rust);
        // An edition is no word of Rust; an unknown one is none.
        assert!(!LangString::parse("edition2018,sh").rust);
        assert_eq!(LangString::parse("edition2018").edition.as_deref(), Some("2018"));
        assert_eq!(LangString::parse("edition2077").edition, None);
        // `E` and four characters that are no number is another language's word, not unknown.
        let it = LangString::parse("compile_fail,E0308,Eabcd");
        assert!(it.error_codes == ["E0308"] && it.unknown.is_empty());
        assert!(!LangString::parse("Eabcd").rust);
        // `ignore-foo` overrides `ignore`.
        let it = LangString::parse("ignore,ignore-windows");
        assert!(!it.ignore && it.ignore_on == ["windows"]);
    }

    #[test]
    fn slugs_are_rustdocs() {
        assert_eq!(slug("Hello, world!"), "hello-world");
        assert_eq!(slug("Safety"), "safety");
        assert_eq!(slug("`foo_bar` notes"), "foo_bar-notes");
    }

    #[test]
    fn plain_summaries_are_one_line() {
        assert_eq!(plain_summary("A *thing*\nthat `does`.\n\nMore.", 100), "A thing that does.");
    }
}
