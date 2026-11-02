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

use crate::{
    html::{Writer, escape},
    links,
    model::Doc,
};

/// The ids a page has used, so a heading in docs gets one of its own.
#[derive(Default)]
pub(crate) struct Ids {
    used: HashMap<String, usize>,
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
#[derive(Debug, Default, PartialEq, Eq)]
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
    pub(crate) unknown: Vec<String>,
    pub(crate) classes: Vec<String>,
}

impl LangString {
    pub(crate) fn parse(info: &str) -> LangString {
        let mut out = LangString { rust: true, ..LangString::default() };
        let mut seen_rust = false;
        let mut seen_other = false;
        let mut seen_custom = false;
        for token in info.split(|c: char| c == ',' || c.is_whitespace()).filter(|it| !it.is_empty())
        {
            let token = token.trim_matches(|c| c == '{' || c == '}');
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
                "rust" => {
                    seen_rust = true;
                }
                "custom" => seen_custom = true,
                "test_harness" | "standalone_crate" => {
                    seen_rust = !seen_other || seen_rust;
                }
                "compile_fail" => {
                    out.compile_fail = true;
                    out.no_run = true;
                    seen_rust = !seen_other || seen_rust;
                }
                x if x.starts_with("edition") => {
                    out.edition = Some(x["edition".len()..].to_owned());
                    seen_rust = !seen_other || seen_rust;
                }
                x if x.starts_with('E') && x.len() == 5 && x[1..].parse::<u32>().is_ok() => {
                    seen_rust = !seen_other || seen_rust;
                }
                x if x.starts_with('.') => out.classes.push(x[1..].to_owned()),
                x if x.starts_with("class=") => {
                    out.classes.push(x["class=".len()..].trim_matches('"').to_owned())
                }
                other => {
                    seen_other = true;
                    out.unknown.push(other.to_owned());
                }
            }
        }
        out.rust &= !seen_custom && (!seen_other || seen_rust);
        out
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
        (" ignore", Some(format!("This example is not tested on {}", stdx::list::list(&lang.ignore_on, stdx::list::Join::Or, false))))
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
