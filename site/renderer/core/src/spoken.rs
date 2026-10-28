//! The spoken version of a page: the lines a voice reads, in page order, each tied to the block it stands for. It is
//! not the article read out. By default each block's line is derived from its content:
//!
//! - prose (Lede, P, Dim, Li) loses its inline markup; a code span is said as words (`inline::say_code`), a link
//!   its label, a reference its label without an identifier before a colon, and a parenthesis of bare references
//!   (`([[a]], [[b]])`, a citation) is dropped;
//! - a Section's title becomes a transition ("Next: Prior art."), an H3 its title, an Issue "Open question: ...", a
//!   Goal its title and its line;
//! - a Table is read row by row against its header ("Function in the main crate: cargo 1.76 seconds, ...");
//! - an Img is its alt text ("On screen: ..."); an Art (an animated drawing) and a Figure (a lesson's diagram) are not
//!   read: their label is for screen readers, and the paragraphs beside them say the same in words;
//! - a lesson's Step is its title, then its blocks; a Quiz or Tap is "Question: " and its prompt (an author marks
//!   question steps Silent); the course path reads each theme's title and its lessons' numbers and titles and its
//!   updates' titles, and What's new (UpdateLog) each update's title under its date;
//! - Code and Command blocks are skipped, citations are never read, References, charts, Pickers and
//!   the benchmark history (BenchHistory) are skipped (a `Say` on the section around a chart tells what it shows),
//!   and the RFC index reads each RFC's number and title.
//!
//! An author overrides any block's line with `Say: {"..."}` (Drama 3 directions such as `[warm]` allowed). On a
//! container (Section, Issue, Goal, Step, Li, Ul, Ol) the `Say` stands for its whole subtree: its children are not read, so one
//! authored line can tell a whole section. The `Silent` tag drops a block and everything under it. A page can carry
//! `Intro` (said first; the default is its title, and for an RFC its number) and `Outro` (said last). Derived lines
//! never carry a direction: brackets and angle brackets are taken out of them, so only an author's `Say` steers the
//! voice.
//!
//! Deterministic: the lines are a function of the world alone. The build hashes them (gen/build.ts) to tell whether a
//! page's generated audio still matches.

use std::sync::LazyLock;

use regex_lite::Regex;

use crate::comments::is_comment;
use crate::inline::{Refs, plain};
use crate::js;
use crate::render::{Page, Site, is_page};
use crate::text::{dedent, prose};
use crate::world::{Entity, Id};

/// The line said before the first block: the page's `Intro`, or its title. Its element is the page's `<h1>`.
pub const INTRO: &str = "_intro";
/// The page's `Outro`, said last. It has no element.
pub const OUTRO: &str = "_outro";

/// One spoken line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// Unique on the page: the block's id, or `_intro` / `_outro`.
    pub id: String,
    /// The block it stands for (the element with `data-say` equal to it): its path under the page.
    pub block: String,
    /// What the voice says. An author's `Say` text verbatim (directions included), else the derived line.
    pub text: String,
    /// True when the text is the author's (`Say`, `Intro`, `Outro`), false when derived.
    pub authored: bool,
}

static RFC_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bRFC 0+([0-9]+)").expect("RFC_NUMBER compiles"));
static MILLIS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([0-9]) ms\b").expect("MILLIS compiles"));
static SECONDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([0-9]) s\b").expect("SECONDS compiles"));
/// A parenthesis holding only bare references, `([[a]], [[b]])`: a citation, not read.
static CITATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\s*\(\s*\[\[[\w-]+\]\](?:\s*,\s*\[\[[\w-]+\]\])*\s*\)").expect("CITATION compiles")
});
static NUMBERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9]+\.\s").expect("NUMBERED compiles"));

/// A derived line made safe and natural to say: no characters Drama 3 reads as directions, units and RFC numbers
/// as words, one line, ending in punctuation.
fn sayable(text: &str) -> String {
    let t: String = text
        .chars()
        .map(|c| match c {
            '[' | ']' | '<' | '>' | '{' | '}' | '|' => ' ',
            c => c,
        })
        .collect();
    let t = RFC_NUMBER.replace_all(&t, "RFC $1");
    let t = MILLIS.replace_all(&t, "$1 milliseconds");
    let t = SECONDS.replace_all(&t, "$1 seconds");
    // "the [[fn_cache]]" with a label that starts with "the" would say "the the".
    let mut words: Vec<&str> = Vec::new();
    for w in t.split_whitespace() {
        let repeat = words.last().is_some_and(|p| p.eq_ignore_ascii_case(w));
        if !(repeat && ["the", "a", "an"].iter().any(|x| x.eq_ignore_ascii_case(w))) {
            words.push(w);
        }
    }
    let t = words.join(" ");
    let t = t.trim_end_matches([',', ';']).to_string();
    if t.is_empty() || t.ends_with(['.', '?', '!', ':']) { t } else { format!("{t}.") }
}

/// An author's text: as written, on one line.
fn authored(text: &str) -> String {
    prose(&dedent(text))
}

fn text_of(e: &Entity, comp: &str) -> Option<String> {
    e.value(comp).map(|v| js::string(v.get("text")))
}

struct Ctx<'a, 'w> {
    s: &'a Site<'w>,
    page: &'a Page,
    lines: Vec<Line>,
    /// Section titles said so far: the first is said plainly, the rest as "Next: ...".
    sections: usize,
}

impl Ctx<'_, '_> {
    fn e(&self, n: Id) -> &Entity {
        self.s.world.get(n)
    }

    fn block_id(&self, n: Id) -> String {
        self.e(n).path[self.e(self.page.node).path.len() + 1..].to_string()
    }

    fn plain(&self, t: &str, n: Id) -> Result<String, String> {
        let refs = Refs { world: self.s.world, parent: self.s.refs, served: self.s.served };
        plain(&CITATION.replace_all(t, ""), &refs, &self.e(n).path)
    }

    fn push(&mut self, id: String, block: String, text: String, authored: bool) {
        if !crate::js::trim(&text).is_empty() {
            self.lines.push(Line { id, block, text, authored });
        }
    }
}

/// Every block under `n` in order, as `render::children` walks them.
fn children(n: Id, ctx: &mut Ctx) -> Result<(), String> {
    let kids: Vec<Id> = ctx
        .e(n)
        .children
        .iter()
        .copied()
        .filter(|c| !is_page(ctx.e(*c)) && !is_comment(ctx.e(*c)))
        .collect();
    for c in kids {
        block(c, ctx)?;
    }
    Ok(())
}

fn block(n: Id, ctx: &mut Ctx) -> Result<(), String> {
    let e = ctx.e(n);
    if e.has_tag("Silent") {
        return Ok(());
    }
    let id = ctx.block_id(n);
    let walk = ["Section", "Issue", "Goal", "Li", "Step"].iter().any(|c| e.has(c))
        || e.has_tag("Ul")
        || e.has_tag("Ol");
    if let Some(say) = text_of(e, "Say") {
        // An authored line tells the block and everything under it: a container's children are not read.
        ctx.push(id.clone(), id, authored(&say), true);
        return Ok(());
    }
    if let Some(t) = derive(n, ctx)? {
        ctx.push(id.clone(), id, sayable(&t), false);
    }
    if walk { children(n, ctx) } else { Ok(()) }
}

/// The default line for a block, before `sayable`; None for a block that is not read.
fn derive(n: Id, ctx: &mut Ctx) -> Result<Option<String>, String> {
    let e = ctx.e(n);
    if e.has_tag("RfcIndex") {
        let parent = e.parent.ok_or("RfcIndex has no parent")?;
        let mut items = Vec::new();
        for c in ctx.s.world.rfcs(parent) {
            let r = ctx.e(c);
            let num = js::string(r.value("Number").and_then(|v| v.get("value")));
            items.push(format!("RFC {num}, {}.", text_of(r, "Title").unwrap_or_default()));
        }
        return Ok(Some(items.join(" ")));
    }
    if e.has_tag("Course") {
        let sources = ctx.s.world.course_sources(n)?;
        let c = crate::course::build(ctx.s.world, &sources)?;
        let mut items = Vec::new();
        for (b, theme) in c.themes.iter().enumerate() {
            items.push(format!("{}.", theme.title));
            for node in c.nodes.iter().filter(|x| x.theme == b) {
                let title = text_of(ctx.e(node.id), "Title").unwrap_or_default();
                items.push(match node.kind {
                    crate::course::Kind::Lesson(i) => format!("Lesson {i}, {title}."),
                    crate::course::Kind::Update { .. } => format!("Update: {title}."),
                });
            }
        }
        return Ok(Some(items.join(" ")));
    }
    if e.has_tag("UpdateLog") {
        // Newest first, as the page lists them: the date once, then each update's title.
        let mut out = Vec::new();
        for p in ctx.s.pages {
            let Some(cw) = ctx.s.world.course_in(p.node) else { continue };
            let c = crate::course::build(ctx.s.world, &ctx.s.world.course_sources(cw)?)?;
            let mut day = String::new();
            for node in c.updates_newest_first(ctx.s.world) {
                let crate::course::Kind::Update { date, .. } = &node.kind else { continue };
                if *date != day {
                    out.push(format!("{}:", crate::course::date(date, true)));
                    day = date.clone();
                }
                out.push(format!("{}.", text_of(ctx.e(node.id), "Title").unwrap_or_default()));
            }
        }
        return Ok(Some(out.join(" ")));
    }
    if e.has_tag("References") || e.has("BenchHistory") {
        return Ok(None);
    }
    for comp in ["Lede", "P", "Dim", "Li"] {
        if let Some(t) = text_of(e, comp) {
            return ctx.plain(&prose(&dedent(&t)), n).map(Some);
        }
    }
    if let Some(t) = text_of(e, "H3") {
        return ctx.plain(&prose(&dedent(&t)), n).map(Some);
    }
    if e.has("Code") || e.has("Command") || e.has("Figure") {
        return Ok(None);
    }
    // A question is read as its prompt; its choices are for the eye.
    for comp in ["Quiz", "Tap"] {
        if let Some(v) = e.value(comp) {
            let prompt = ctx.plain(&prose(&js::string(v.get("prompt"))), n)?;
            return Ok(Some(format!("Question: {prompt}")));
        }
    }
    if let Some(v) = e.value("Step") {
        return ctx.plain(&js::string(v.get("title")), n).map(Some);
    }
    if let Some(i) = e.value("Img") {
        let alt = js::string(i.get("alt"));
        return Ok((!alt.is_empty()).then(|| format!("On screen: {alt}")));
    }
    if let Some(v) = e.value("Section") {
        let title = ctx.plain(&js::string(v.get("title")), n)?;
        let first = ctx.sections == 0;
        ctx.sections += 1;
        return Ok(Some(if first || NUMBERED.is_match(&title) {
            title
        } else {
            format!("Next: {title}")
        }));
    }
    if let Some(v) = e.value("Issue") {
        let title = ctx.plain(&js::string(v.get("title")), n)?;
        let status = match e.targets("Status").and_then(|t| t.first()).map(String::as_str) {
            Some("Open") => "Open question",
            Some("Resolved") => "Settled question",
            _ => "Question",
        };
        return Ok(Some(format!("{status}: {title}")));
    }
    if let Some(v) = e.value("Goal") {
        let (title, line) =
            (ctx.plain(&js::string(v.get("title")), n)?, ctx.plain(&js::string(v.get("line")), n)?);
        return Ok(Some(if line.is_empty() { title } else { format!("{title}. {line}") }));
    }
    if e.has_tag("Table") {
        return table(n, ctx).map(Some);
    }
    Ok(None)
}

/// A table read row by row: with a header, "first cell: header value, header value."; without, its cells in order.
fn table(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    let mut head: Option<Vec<String>> = None;
    let mut rows = Vec::new();
    for &r in &ctx.e(n).children.clone() {
        let cells: Vec<Id> = ctx.e(r).children.clone();
        let is_head = cells.iter().all(|c| ctx.e(*c).has("Th"));
        let mut texts = Vec::new();
        for c in cells {
            let comp = if ctx.e(c).has("Th") { "Th" } else { "Td" };
            let t = text_of(ctx.e(c), comp).unwrap_or_default();
            texts.push(ctx.plain(&prose(&t), c)?);
        }
        if is_head && head.is_none() {
            head = Some(texts);
            continue;
        }
        let line = match &head {
            Some(h) if texts.len() > 1 => {
                let rest: Vec<String> = texts[1..]
                    .iter()
                    .enumerate()
                    .map(|(i, v)| match h.get(i + 1) {
                        Some(k) if !k.is_empty() => format!("{k} {v}"),
                        _ => v.clone(),
                    })
                    .collect();
                format!("{}: {}.", texts[0], rest.join(", "))
            }
            _ => format!("{}.", texts.join(", ")),
        };
        rows.push(line);
    }
    Ok(rows.join(" "))
}

/// The page's spoken lines, in order: the intro, every block's line, the outro.
pub fn lines(s: &Site, page: &Page) -> Result<Vec<Line>, String> {
    let mut ctx = Ctx { s, page, lines: Vec::new(), sections: 0 };
    let p = ctx.e(page.node);
    match text_of(p, "Intro") {
        Some(t) => ctx.push(INTRO.into(), INTRO.into(), authored(&t), true),
        None => {
            let title = text_of(p, "Title").unwrap_or_default();
            let t = match p.value("Number") {
                Some(num) if p.is_a("Rfc") => {
                    format!("RFC {}. {title}", js::string(num.get("value")))
                }
                _ => title,
            };
            ctx.push(INTRO.into(), INTRO.into(), sayable(&t), false);
        }
    }
    children(page.node, &mut ctx)?;
    if let Some(t) = text_of(ctx.e(page.node), "Outro") {
        ctx.push(OUTRO.into(), OUTRO.into(), authored(&t), true);
    }
    Ok(ctx.lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sayable_lines() {
        assert_eq!(
            sayable("cargo 1.76 s, blaze 0.42 s"),
            "cargo 1.76 seconds, blaze 0.42 seconds."
        );
        assert_eq!(sayable("see RFC 0001 [warm] <b>"), "see RFC 1 warm b.");
        assert_eq!(sayable("Open question: Syntax"), "Open question: Syntax.");
        assert_eq!(sayable("Why?"), "Why?");
    }

    #[test]
    fn code_spans() {
        use crate::inline::say_code;
        assert_eq!(say_code("mem::forget"), "mem forget");
        assert_eq!(say_code("Rc<T>"), "Rc of T");
        assert_eq!(say_code("!Forget"), "not Forget");
        assert_eq!(say_code("/ Fs"), "slash Fs");
        assert_eq!(say_code("#[blaze::cache]"), "blaze cache");
        assert_eq!(say_code("build|run"), "build or run");
        assert_eq!(say_code("thread::scope(|s| ...)"), "thread scope");
        assert_eq!(sayable("the the cache"), "the cache.");
        assert_eq!(say_code("with fs \\{ ... }"), "with fs");
        assert!(CITATION.is_match("authority ([[sandbox]])"));
        assert!(CITATION.is_match("hash ([[a]], [[b-c]])"));
    }

    #[test]
    fn say_on_a_container_replaces_its_subtree() {
        use crate::world::{Scalar, Value, World};
        fn ent(name: &str, values: &[(&str, &str, &str)], tags: &[&str], isa: &[&str]) -> Entity {
            Entity {
                name: name.into(),
                values: values
                    .iter()
                    .map(|(c, f, t)| {
                        (c.to_string(), Value::from([(f.to_string(), Scalar::Str(t.to_string()))]))
                    })
                    .collect(),
                tags: tags.iter().map(|t| t.to_string()).collect(),
                isa: isa.iter().map(|t| t.to_string()).collect(),
                ..Entity::default()
            }
        }
        let mut w = World::default();
        let site = w.spawn(None, ent("site", &[], &[], &[])).unwrap();
        let page =
            w.spawn(Some(site), ent("doc", &[("Title", "text", "Doc")], &[], &["Page"])).unwrap();
        let told = w
            .spawn(
                Some(page),
                ent(
                    "told",
                    &[("Section", "title", "Told"), ("Say", "text", "[warm] All of it.")],
                    &[],
                    &[],
                ),
            )
            .unwrap();
        w.spawn(Some(told), ent("p", &[("P", "text", "Not read.")], &[], &[])).unwrap();
        let list = w.spawn(Some(told), ent("list", &[], &["Ul"], &[])).unwrap();
        w.spawn(Some(list), ent("li", &[("Li", "text", "Not read either.")], &[], &[])).unwrap();
        let plain =
            w.spawn(Some(page), ent("plain", &[("Section", "title", "Plain")], &[], &[])).unwrap();
        let ul = w
            .spawn(Some(plain), ent("ul", &[("Say", "text", "The list, told.")], &["Ul"], &[]))
            .unwrap();
        w.spawn(Some(ul), ent("li", &[("Li", "text", "Skipped.")], &[], &[])).unwrap();
        w.spawn(Some(plain), ent("p", &[("P", "text", "Read.")], &[], &[])).unwrap();

        let pages =
            [Page { node: page, url: "/doc".into(), file: "doc.html".into(), doc: "doc".into() }];
        let served = crate::render::Served::default();
        let s = Site { world: &w, site, pages: &pages, nav: &[], refs: None, served: &served };
        let got: Vec<(String, String)> =
            lines(&s, &pages[0]).unwrap().into_iter().map(|l| (l.block, l.text)).collect();
        let want = [
            ("_intro", "Doc."),
            ("told", "[warm] All of it."),
            ("plain", "Plain."),
            ("plain.ul", "The list, told."),
            ("plain.p", "Read."),
        ];
        assert_eq!(got, want.map(|(b, t)| (b.to_string(), t.to_string())));
    }
}
