//! The site's world as HTML pages, as render/render.ts and render/widgets.ts: one function per element type of
//! content/schema.flecs; a page is an entity that is an instance of the Page prefab. The output matches the TS
//! renderer's byte for byte, so the order things are computed in follows its template literals where that order is
//! visible (citation numbers, the block list).

use std::collections::BTreeMap;

use crate::comments::{self, Discussion, PAGE_BLOCK, is_comment};
use crate::inline::{Refs, esc, inline};
use crate::js;
use crate::text::{dedent, prose};
use crate::world::{Entity, Id, Value, World};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub node: Id,
    /// `/`, `/rfcs`, `/rfcs/capabilities`.
    pub url: String,
    /// The file under dist/: `index.html`, `rfcs.html`, `rfcs/capabilities.html`.
    pub file: String,
    /// Comments are keyed by this: the URL without its slash (`rfcs/capabilities`).
    pub doc: String,
}

/// What a host needs to check a comment on a discussable page: its title and each block's text (the first 280 UTF-16
/// units), the page itself as `_page`.
#[derive(Clone, Debug, PartialEq)]
pub struct Blocks {
    pub doc: String,
    pub title: String,
    /// In the order the TS object has them: `_page` first, then each block as it was rendered.
    pub blocks: Vec<(String, String)>,
}

pub fn is_page(e: &Entity) -> bool {
    e.is_a("Page")
}

pub fn pages(world: &World, site: Id) -> Vec<Page> {
    fn walk(world: &World, site: Id, n: Id, out: &mut Vec<Page>) {
        let base = world.get(site).path.len() + 1;
        for &c in &world.get(n).children {
            let e = world.get(c);
            if !is_page(e) {
                continue;
            }
            let rel = e.path[base..].replace('.', "/");
            let url = if rel == "index" { "/".to_string() } else { format!("/{rel}") };
            let file =
                if url == "/" { "index.html".to_string() } else { format!("{}.html", &url[1..]) };
            let doc = if url == "/" { "index".to_string() } else { url[1..].to_string() };
            out.push(Page { node: c, url, file, doc });
            walk(world, site, c, out);
        }
    }
    let mut out = Vec::new();
    walk(world, site, site, &mut out);
    out
}

fn field(v: Option<&Value>, k: &str) -> String {
    js::string(v.and_then(|v| v.get(k)))
}

fn text(e: &Entity, comp: &str) -> String {
    field(e.value(comp), "text")
}

/// Where the site is served (`Served` on `site`): the origin (`https://ix.dev`) and the path every page sits under
/// (`/blaze`). Both empty when the site has no `Served`: pages at the root, no canonical URL.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Served {
    pub origin: String,
    pub base: String,
}

impl Served {
    /// `site`'s `Served`, checked: an http(s) origin with no path, and a base that is empty or `/a/b` with no trailing
    /// slash.
    pub fn of(world: &World, site: Id) -> Result<Served, String> {
        let Some(v) = world.get(site).value("Served") else { return Ok(Served::default()) };
        let (origin, base) = (field(Some(v), "origin"), field(Some(v), "base"));
        let host = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://"));
        if !host.is_some_and(|h| !h.is_empty() && !h.contains('/')) {
            return Err(format!("site: Served origin {origin:?} is not http(s)://host"));
        }
        let ok_base = base.is_empty()
            || (base.starts_with('/')
                && !base.ends_with('/')
                && !base.contains("//")
                && base.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b)));
        if !ok_base {
            return Err(format!(
                "site: Served base {base:?} is not empty or /path without a trailing slash"
            ));
        }
        Ok(Served { origin, base })
    }

    /// A link as the browser gets it: a site path (`/rfcs/x`, `/`) under the base (`/blaze/rfcs/x`, `/blaze/`);
    /// anything else (`https://...`, `#ref-x`, `//host`) as it is.
    pub fn href(&self, href: &str) -> String {
        if href.starts_with('/') && !href.starts_with("//") {
            format!("{}{href}", self.base)
        } else {
            href.to_string()
        }
    }
}

/// Everything a render reads: the world and the indexes over it the build makes once.
pub struct Site<'w> {
    pub world: &'w World,
    pub site: Id,
    pub pages: &'w [Page],
    /// The sidebar's sections (side.rs), built once.
    pub nav: &'w [crate::side::Group],
    pub refs: Option<Id>,
    pub served: &'w Served,
}

impl Site<'_> {
    fn page_of(&self, n: Id) -> Option<&Page> {
        self.pages.iter().find(|p| p.node == n)
    }
}

struct Ctx<'a, 'w> {
    s: &'a Site<'w>,
    page: &'a Page,
    cited: Vec<Id>,
    discussable: bool,
    blocks: Vec<(String, String)>,
    scripts: Vec<&'static str>,
    discussion: &'a Discussion,
    /// On a page with audio (an `Audio` value, set by the build): the blocks that have a spoken line. Each gets
    /// ` data-say="<id>"`, which the player (client/listen.ts) highlights and plays from.
    say: Option<std::collections::BTreeSet<String>>,
}

impl Ctx<'_, '_> {
    fn e(&self, n: Id) -> &Entity {
        self.s.world.get(n)
    }

    fn inline(&mut self, t: &str, wher: &str) -> Result<String, String> {
        let refs = Refs { world: self.s.world, parent: self.s.refs, served: self.s.served };
        inline(t, &refs, wher, &mut self.cited)
    }

    /// ` data-say=".."` when the block has a spoken line on a page with audio.
    fn say_attr(&self, n: Id) -> String {
        let id = self.block_id(n);
        match &self.say {
            Some(set) if set.contains(&id) => format!(" data-say=\"{}\"", esc(&id)),
            _ => String::new(),
        }
    }

    fn script(&mut self, s: &'static str) {
        if !self.scripts.contains(&s) {
            self.scripts.push(s);
        }
    }

    /// The block's anchor: its path under the page (`motivation.why`).
    fn block_id(&self, n: Id) -> String {
        self.e(n).path[self.e(self.page.node).path.len() + 1..].to_string()
    }
}

#[derive(Default)]
struct Anchor {
    /// ` data-block=".."` for the block's element.
    attr: String,
    /// The count button, inside the element.
    button: String,
    /// The thread, after the element (inside it for an li).
    thread: String,
}

/// What a block a reader can comment on carries, after checking it has a name. Empty outside a Discussable page.
fn anchor(n: Id, ctx: &mut Ctx, plain: &str) -> Result<Anchor, String> {
    let say = ctx.say_attr(n);
    if !ctx.discussable {
        return Ok(Anchor { attr: say, ..Anchor::default() });
    }
    let e = ctx.e(n);
    if e.name.starts_with('#') {
        let parent = e.parent.map(|p| ctx.e(p).path.clone()).unwrap_or_default();
        return Err(format!(
            "{}: a block in a Doc needs a name (anonymous child of {parent})",
            ctx.e(ctx.page.node).path
        ));
    }
    let id = ctx.block_id(n);
    let short = js::slice_utf16(plain, 280).to_string();
    match ctx.blocks.iter_mut().find(|(k, _)| *k == id) {
        Some((_, v)) => *v = short,
        None => ctx.blocks.push((id.clone(), short)),
    }
    let w = ctx.s.world;
    Ok(Anchor {
        attr: format!(" data-block=\"{}\"{say}", esc(&id)),
        button: comments::button(&id, w, n),
        thread: comments::thread(
            &id,
            w,
            n,
            &ctx.page.doc,
            &ctx.page.url,
            ctx.discussion,
            ctx.s.served,
        ),
    })
}

/// A superscript per reference the block cites with (Cites, refs.x).
fn cites(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    let e = ctx.e(n);
    let targets = e.targets("Cites").map(<[String]>::to_vec).unwrap_or_default();
    let mut out = String::new();
    for t in targets {
        let r = ctx.s.world.lookup(&t).filter(|r| ctx.e(*r).has("Ref"));
        let Some(r) = r else {
            return Err(format!("{}: (Cites, {t}) is not a Ref", ctx.e(n).path));
        };
        if !ctx.cited.contains(&r) {
            ctx.cited.push(r);
        }
        let i = ctx.cited.iter().position(|x| *x == r).expect("just pushed") + 1;
        out +=
            &format!("<sup class=\"cite\"><a href=\"#ref-{}\">{i}</a></sup>", esc(&ctx.e(r).name));
    }
    Ok(out)
}

fn prosed(n: Id, comp: &str, ctx: &mut Ctx) -> Result<(String, String), String> {
    let t = prose(&dedent(&text(ctx.e(n), comp)));
    let path = ctx.e(n).path.clone();
    Ok((ctx.inline(&t, &path)?, t))
}

fn children(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    let kids: Vec<Id> = ctx
        .e(n)
        .children
        .iter()
        .copied()
        .filter(|c| !is_page(ctx.e(*c)) && !is_comment(ctx.e(*c)))
        .collect();
    let mut out = Vec::with_capacity(kids.len());
    for c in kids {
        out.push(block(c, ctx)?);
    }
    Ok(out.join("\n"))
}

fn status(e: &Entity) -> String {
    match e.targets("Status").and_then(|t| t.first()) {
        Some(s) => {
            format!("<span class=\"status status-{}\">{}</span>", esc(&s.to_lowercase()), esc(s))
        }
        None => String::new(),
    }
}

const REFERENCES: &str = "<!--references-->";

/// The tags that render through a widget.
const WIDGETS: [&str; 4] = ["RfcIndex", "References", "Course", "UpdateLog"];

fn widget(tag: &str, n: Id, ctx: &mut Ctx) -> Result<String, String> {
    match tag {
        "RfcIndex" => {
            let parent = ctx.e(n).parent.ok_or("RfcIndex has no parent")?;
            let mut items = String::new();
            for c in ctx.s.world.rfcs(parent) {
                let e = ctx.e(c);
                let p = ctx
                    .s
                    .page_of(c)
                    .ok_or_else(|| format!("{}: an Rfc that is not a page", e.path))?;
                let num = js::pad_zero(&field(e.value("Number"), "value"), 4);
                let st = e.targets("Status").and_then(|t| t.first()).cloned().unwrap_or_default();
                let desc = prose(&text(e, "Description"));
                let desc = if desc.is_empty() {
                    String::new()
                } else {
                    format!("<br /><span class=\"dim\">{}</span>", esc(&desc))
                };
                items += &format!(
                    "<li><a href=\"{}\"><span class=\"num mono\">{num}</span> {}</a> <span class=\"status status-{}\">{}</span>{desc}</li>",
                    esc(&ctx.s.served.href(&p.url)),
                    esc(&text(e, "Title")),
                    esc(&st.to_lowercase()),
                    esc(&st)
                );
            }
            Ok(format!("<ul class=\"rfc-index\">{items}</ul>"))
        }
        // Filled in once the whole page has rendered, so it lists citations made after it too.
        "References" => Ok(REFERENCES.into()),
        "Course" => course(n, ctx),
        "UpdateLog" => update_log(n, ctx),
        _ => unreachable!("WIDGETS lists the tags"),
    }
}

/// The HTML the build highlighted for a Code or Command block, if any.
fn highlighted(e: &Entity) -> Option<String> {
    match e.value("Highlighted")?.get("html")? {
        crate::world::Scalar::Str(h) => Some(h.clone()),
        _ => None,
    }
}

fn block(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    if let Some(tag) = WIDGETS.iter().find(|t| ctx.e(n).has_tag(t)) {
        return widget(tag, n, ctx);
    }
    let e = ctx.e(n);
    if let Some(comp) = ["Lede", "P", "Dim"].into_iter().find(|c| e.has(c)) {
        let (html, plain) = prosed(n, comp, ctx)?;
        let cls =
            if comp == "P" { String::new() } else { format!(" class=\"{}\"", comp.to_lowercase()) };
        let a = anchor(n, ctx, &plain)?;
        let c = cites(n, ctx)?;
        return Ok(format!("<p{cls}{}>{html}{c}{}</p>{}", a.attr, a.button, a.thread));
    }
    if e.has("H3") {
        let (html, plain) = prosed(n, "H3", ctx)?;
        let a = anchor(n, ctx, &plain)?;
        return Ok(format!(
            "<h3 id=\"{}\"{}>{html}{}</h3>{}",
            esc(&ctx.block_id(n)),
            a.attr,
            a.button,
            a.thread
        ));
    }
    if e.has("Li") {
        let (html, plain) = prosed(n, "Li", ctx)?;
        let nested = children(n, ctx)?;
        let a = anchor(n, ctx, &plain)?;
        let c = cites(n, ctx)?;
        return Ok(format!("<li{}>{html}{c}{}{nested}{}</li>", a.attr, a.button, a.thread));
    }
    if let Some(c) = e.value("Code") {
        let lang = field(Some(c), "lang");
        let code = dedent(&field(Some(c), "text"));
        // The build highlights rust, toml and bash (highlight.js escapes every text run).
        let html = highlighted(e).unwrap_or_else(|| esc(&code));
        let a = anchor(n, ctx, &code)?;
        return Ok(format!(
            "<figure class=\"codeblock code-roles\"{}><pre class=\"hljs\" data-lang=\"{}\"><code>{html}</code></pre>{}</figure>{}",
            a.attr,
            esc(&lang),
            a.button,
            a.thread
        ));
    }
    if e.has("Command") {
        let cmd = prose(&text(e, "Command"));
        let html = highlighted(e).filter(|h| !h.is_empty()).unwrap_or_else(|| esc(&cmd));
        ctx.script("copy");
        return Ok(format!(
            "<div class=\"cmd code-roles\"{}><pre class=\"hljs\"><code>{html}</code></pre><button type=\"button\" data-copy=\"{}\" aria-label=\"Copy command\">Copy</button></div>",
            ctx.say_attr(n),
            esc(&cmd)
        ));
    }
    if let Some(i) = e.value("Img") {
        let alt = field(Some(i), "alt");
        let (src, w, h) =
            (field(Some(i), "src"), js::to_number(i.get("width")), js::to_number(i.get("height")));
        let a = anchor(n, ctx, &alt)?;
        return Ok(format!(
            "<img src=\"{}\" width=\"{}\" height=\"{}\" alt=\"{}\"{} />{}{}",
            esc(&ctx.s.served.href(&src)),
            js::number(w),
            js::number(h),
            esc(&alt),
            a.attr,
            a.button,
            a.thread
        ));
    }
    if let Some(v) = e.value("Art") {
        // An empty canvas that client/art.ts draws the named piece into; the label is what it shows.
        let (piece, label) = (esc(&field(Some(v), "piece")), esc(&field(Some(v), "label")));
        ctx.script("art");
        return Ok(format!(
            "<div class=\"art\" data-piece=\"{piece}\" role=\"img\" aria-label=\"{label}\"><canvas></canvas></div>"
        ));
    }
    if let Some(v) = e.value("Section") {
        let title = field(Some(v), "title");
        let id = esc(&ctx.block_id(n));
        let a = anchor(n, ctx, &title)?;
        let path = ctx.e(n).path.clone();
        let t = ctx.inline(&title, &path)?;
        let kids = children(n, ctx)?;
        return Ok(format!(
            "<section id=\"{id}\"><h2{}><a class=\"hash\" href=\"#{id}\">{t}</a>{}</h2>{}\n{kids}\n</section>",
            a.attr, a.button, a.thread
        ));
    }
    if let Some(v) = e.value("Issue") {
        let title = field(Some(v), "title");
        let id = esc(&ctx.block_id(n));
        let a = anchor(n, ctx, &title)?;
        let path = ctx.e(n).path.clone();
        let t = ctx.inline(&title, &path)?;
        let st = status(ctx.e(n));
        let kids = children(n, ctx)?;
        return Ok(format!(
            "<div class=\"issue\" id=\"{id}\"><h3{}>{t} {st}{}</h3>{}\n{kids}\n</div>",
            a.attr, a.button, a.thread
        ));
    }
    if let Some(v) = e.value("Goal") {
        // Folded: the title and its one line, and the paragraphs when the reader opens it.
        let (title, line) = (field(Some(v), "title"), field(Some(v), "line"));
        let id = esc(&ctx.block_id(n));
        let a = anchor(n, ctx, &title)?;
        let path = ctx.e(n).path.clone();
        let (t, l) = (ctx.inline(&title, &path)?, ctx.inline(&line, &path)?);
        let kids = children(n, ctx)?;
        return Ok(format!(
            "<details class=\"goal\" id=\"{id}\"><summary><span class=\"goal-head\"><h3{}>{t}</h3><span class=\"goal-line\">{l}</span></span>{}</summary>{}{}<div class=\"goal-body\">\n{kids}\n</div></details>",
            a.attr,
            crate::icons::svg("caret")
                .unwrap_or_default()
                .replace("class=\"icon\"", "class=\"icon chev\""),
            a.button,
            a.thread
        ));
    }
    if let Some(v) = e.value("Step") {
        // One screen of a lesson: stacked under each other without JS, one at a time with it (client/learn.ts).
        let title = field(Some(v), "title");
        let id = esc(&ctx.block_id(n));
        let a = anchor(n, ctx, &title)?;
        let path = ctx.e(n).path.clone();
        let t = ctx.inline(&title, &path)?;
        let kids = children(n, ctx)?;
        return Ok(format!(
            "<section id=\"{id}\" class=\"step\"{}><h2>{t}</h2>\n{kids}\n</section>",
            a.attr
        ));
    }
    if e.has("Quiz") {
        return quiz(n, ctx);
    }
    for comp in ["RowChart", "LineChart"] {
        if let Some(v) = e.value(comp) {
            let note = prose(&field(Some(v), "note"));
            let path = e.path.clone();
            if comp == "LineChart" {
                // The run under the pointer (client/chart.ts); the chart reads whole without it.
                ctx.script("chart");
            }
            let note = ctx.inline(&note, &path)?;
            let id = ctx.block_id(n);
            let html = if comp == "RowChart" {
                crate::chart::row_chart(ctx.s.world, n, &id, &note)?
            } else {
                crate::chart::line_chart(ctx.s.world, n, &id, &note)?
            };
            return Ok(html);
        }
    }
    if e.has_tag("Charts") {
        return Ok(format!("<div class=\"charts\">\n{}\n</div>", children(n, ctx)?));
    }
    if e.has("Picker") {
        // Radio buttons and one panel per Pick; styles/site.css shows the checked one's.
        let picks: Vec<Id> = e.children.clone();
        for &p in &picks {
            if !ctx.e(p).has("Pick") {
                return Err(format!("{}: a Picker's children are Picks", ctx.e(p).path));
            }
        }
        let id = ctx.block_id(n);
        let buttons = crate::chart::picker_buttons(ctx.s.world, n, &id, &picks)?;
        let mut panels = String::new();
        for p in picks {
            panels += &format!("<div class=\"panel\">\n{}\n</div>", children(p, ctx)?);
        }
        return Ok(format!("<div class=\"picker\">{buttons}{panels}</div>"));
    }
    if e.has("BenchHistory") {
        // Filled by the build from the benchmark history (gen/bench/).
        return Ok(format!("<div class=\"bench\">\n{}\n</div>", children(n, ctx)?));
    }
    if let Some(v) = e.value("Tap") {
        // Tap the parts of a diagram: client/learn.ts draws the piece with its parts as buttons and checks the picks
        // against `right`. Without JS the answer is in the fold.
        let [prompt, piece, right, hint, why] =
            ["prompt", "piece", "right", "hint", "why"].map(|f| field(Some(v), f));
        let path = ctx.e(n).path.clone();
        if piece.is_empty() || right.split(',').all(|r| r.trim().is_empty()) {
            return Err(format!("{path}: a Tap needs a piece and the parts that are right"));
        }
        let id = esc(&ctx.block_id(n));
        let a = anchor(n, ctx, &prompt)?;
        let p = ctx.inline(&prose(&prompt), &path)?;
        let hint = ctx.inline(&prose(&hint), &path)?;
        let why = ctx.inline(&prose(&why), &path)?;
        let right: Vec<&str> = right.split(',').map(str::trim).filter(|r| !r.is_empty()).collect();
        ctx.script("learn");
        return Ok(format!(
            "<div class=\"quiz tap\" id=\"{id}\" data-quiz=\"{id}\" data-right=\"{}\"{}><p class=\"prompt\">{p}</p><div class=\"diagram\" data-figure=\"{}\" data-pick=\"\"></div><p class=\"hint\">{hint}</p><details class=\"answer\"><summary>Show the answer</summary><p>{why}</p></details></div>",
            esc(&right.join(",")),
            a.attr,
            esc(&piece)
        ));
    }
    if let Some(v) = e.value("Figure") {
        // A diagram client/learn.ts draws and the reader can touch; without JS its label says what it would show.
        let (piece, label) = (esc(&field(Some(v), "piece")), field(Some(v), "label"));
        let path = ctx.e(n).path.clone();
        let l = ctx.inline(&prose(&label), &path)?;
        ctx.script("learn");
        return Ok(format!(
            "<figure class=\"diagram\" data-figure=\"{piece}\"{}><figcaption>{l}</figcaption></figure>",
            ctx.say_attr(n)
        ));
    }
    if e.has_tag("Ul") || e.has_tag("Ol") {
        let t = if e.has_tag("Ul") { "ul" } else { "ol" };
        return Ok(format!("<{t}>\n{}\n</{t}>", children(n, ctx)?));
    }
    if e.has_tag("Table") {
        return table(n, ctx);
    }
    let tags = if e.tags.is_empty() { "none".to_string() } else { e.tags.join(", ") };
    let comps = if e.values.is_empty() {
        "none".to_string()
    } else {
        e.values.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")
    };
    Err(format!("{}: no element type (tags {tags}; components {comps})", e.path))
}

/// A multiple-choice question: the prompt, the choices (each with its one-line why, which client/learn.ts shows once
/// the reader picks it), and the right one with its why in a fold for a reader without JS.
fn quiz(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    let path = ctx.e(n).path.clone();
    let prompt = field(ctx.e(n).value("Quiz"), "prompt");
    let id = esc(&ctx.block_id(n));
    let a = anchor(n, ctx, &prompt)?;
    let p = ctx.inline(&prose(&prompt), &path)?;
    let (mut items, mut answer, mut count) = (String::new(), None, 0);
    for c in ctx.e(n).children.clone() {
        let ce = ctx.e(c);
        let (cpath, right) = (ce.path.clone(), ce.has_tag("Right"));
        let Some(v) = ce.value("Choice") else {
            return Err(format!("{cpath}: a Quiz's children are Choices"));
        };
        let (text, why) = (field(Some(v), "text"), field(Some(v), "why"));
        let (t, w) = (ctx.inline(&prose(&text), &cpath)?, ctx.inline(&prose(&why), &cpath)?);
        if right {
            if answer.is_some() {
                return Err(format!("{path}: a Quiz has one Right Choice"));
            }
            answer = Some(format!("<strong>{t}</strong> {w}"));
        }
        count += 1;
        items += &format!(
            "<li class=\"choice\"{}><span class=\"text\">{t}</span><span class=\"why\">{w}</span></li>",
            if right { " data-right=\"\"" } else { "" }
        );
    }
    let answer = answer.ok_or_else(|| format!("{path}: a Quiz needs a Right Choice"))?;
    if count < 2 {
        return Err(format!("{path}: a Quiz needs two Choices or more"));
    }
    ctx.script("learn");
    Ok(format!(
        "<div class=\"quiz\" id=\"{id}\" data-quiz=\"{id}\"{}><p class=\"prompt\">{p}</p><ol class=\"choices\">{items}</ol><details class=\"answer\"><summary>Show the answer</summary><p>{answer}</p></details></div>",
        a.attr
    ))
}

/// A lesson's steps, and the ids of its questions (Quiz and Tap blocks, as their path under the page): what the
/// course path counts and client/learn.ts keeps answers under.
pub fn lesson_parts(world: &World, lesson: Id) -> (usize, Vec<String>) {
    fn walk(world: &World, n: Id, base: usize, out: &mut Vec<String>) {
        for &c in &world.get(n).children {
            let e = world.get(c);
            if e.has("Quiz") || e.has("Tap") {
                out.push(e.path[base..].to_string());
            } else if !is_page(e) {
                walk(world, c, base, out);
            }
        }
    }
    let e = world.get(lesson);
    let steps = e.children.iter().filter(|&&c| world.get(c).has("Step")).count();
    let mut quizzes = Vec::new();
    walk(world, lesson, e.path.len() + 1, &mut quizzes);
    (steps, quizzes)
}

/// A lesson's or update's glyph: the drawing gen/glyphs.ts made for its `glyph`, baked onto the page by the build.
fn glyph(ctx: &Ctx, lesson: Id) -> Result<String, String> {
    match ctx.e(lesson).value("Baked").and_then(|v| v.get("html")) {
        Some(crate::world::Scalar::Str(h)) if !h.is_empty() => Ok(h.clone()),
        _ => Err(format!("{}: the glyph is not baked (run the build)", ctx.e(lesson).path)),
    }
}

/// The check on a finished lesson's node.
const DONE_MARK: &str = "<svg viewBox=\"0 0 12 12\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.8\" stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"M3.2 6.3l1.9 1.9 3.7-4.1\"/></svg>";

/// The end of the path: a check in pixel cells on a faint grid, as the lesson's "complete" screen draws it
/// (client/learn.ts `seal`).
fn end_seal() -> String {
    const ON: [(u8, u8); 7] = [(1, 3), (2, 4), (3, 5), (4, 4), (5, 3), (6, 2), (7, 1)];
    let mut cells = String::new();
    for y in 0..7u8 {
        for x in 0..9u8 {
            let on = if ON.contains(&(x, y)) { " class=\"on\"" } else { "" };
            cells += &format!("<rect x=\"{x}.1\" y=\"{y}.1\" width=\"0.8\" height=\"0.8\"{on}/>");
        }
    }
    format!("<svg class=\"seal\" viewBox=\"0 0 9 7\" aria-hidden=\"true\">{cells}</svg>")
}

/// "4 steps · 2 min · 1 question" for a lesson or update.
fn steps_line(world: &World, page: Id) -> (usize, Vec<String>, String) {
    let (steps, quizzes) = lesson_parts(world, page);
    // About 20 seconds a step: a short paragraph, or a question.
    let minutes = (steps * 20).div_ceil(60).max(1);
    let questions = match quizzes.len() {
        0 => String::new(),
        1 => " · 1 question".to_string(),
        q => format!(" · {q} questions"),
    };
    (steps, quizzes, format!("{steps} steps · {minutes} min{questions}"))
}

/// The line above an update's title: "Update to lesson 2 · Oct 28".
fn update_eyebrow(c: &crate::course::Course, i: usize, short: bool) -> String {
    let crate::course::Kind::Update { date, .. } = &c.nodes[i].kind else { return String::new() };
    let to = c.extends(i);
    let what = if to.is_empty() { "Update".to_string() } else { format!("Update to {to}") };
    format!("{what} · {}", crate::course::date(date, short))
}

/// The course path (course.rs lays it out): the way in (the first lesson, until client/learn.ts points it at the
/// reader's next one), then each theme as a band under its heading, its lessons and updates as nodes on a grid of
/// columns (`data-col`), each with its glyph, its eyebrow (Lesson 2; Update to lesson 4 · Oct 27), title, line, steps,
/// minutes and questions, and the path's end. The edges are data on the course (`data-edges`: `from-to` by node index
/// in page order, `:track` for one that crosses a divider), which client/learn.ts strokes between the pins it measures.
/// Without JS it is a list of links per band on a plain dotted line.
fn course(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    let sources = ctx.s.world.course_sources(n)?;
    let c = crate::course::build(ctx.s.world, &sources)
        .map_err(|e| format!("{}: {e}", ctx.e(n).path))?;
    let first = c
        .lessons()
        .next()
        .and_then(|l| ctx.s.page_of(l.id))
        .ok_or_else(|| format!("{}: a Course with no lessons", ctx.e(n).path))?;
    let go = format!(
        "<div class=\"course-top\"><a class=\"course-go\" href=\"{}\"><span class=\"k\">Start lesson 1</span><span class=\"t\">{}</span></a></div>",
        esc(&ctx.s.served.href(&first.url)),
        esc(&text(ctx.e(first.node), "Title"))
    );
    let mut bands = String::new();
    for (b, theme) in c.themes.iter().enumerate() {
        let mut items = String::new();
        for (i, node) in c.nodes.iter().enumerate().filter(|(_, x)| x.theme == b) {
            let e = ctx.e(node.id);
            let p = ctx
                .s
                .page_of(node.id)
                .ok_or_else(|| format!("{}: a lesson or update that is not a page", e.path))?;
            let (steps, quizzes, meta) = steps_line(ctx.s.world, node.id);
            let requires: Vec<&str> =
                node.requires.iter().map(|&r| c.nodes[r].key.as_str()).collect();
            let (cls, eyebrow, line, extra) = match &node.kind {
                crate::course::Kind::Lesson(k) => {
                    ("node", format!("Lesson {k}"), field(e.value("Lesson"), "line"), String::new())
                }
                crate::course::Kind::Update { date, version } => (
                    "node update",
                    format!(
                        "{}<span class=\"new\" hidden>New</span>",
                        esc(&update_eyebrow(&c, i, true))
                    ),
                    field(e.value("Update"), "line"),
                    format!(" data-date=\"{}\" data-version=\"{}\"", esc(date), esc(version)),
                ),
            };
            items += &format!(
                "<li class=\"{cls}\" data-col=\"{}\" data-lesson=\"{}\" data-steps=\"{steps}\" data-quizzes=\"{}\" data-requires=\"{}\"{extra}><a href=\"{}\"><span class=\"pin\" aria-hidden=\"true\"><span class=\"disc\">{}</span><span class=\"badge\">{DONE_MARK}</span></span><span class=\"body\"><span class=\"n\">{eyebrow}</span><span class=\"t\">{}</span><span class=\"l\">{}</span><span class=\"m\">{meta}</span></span></a></li>",
                node.col,
                esc(&node.key),
                esc(&quizzes.join(" ")),
                esc(&requires.join(" ")),
                esc(&ctx.s.served.href(&p.url)),
                glyph(ctx, node.id)?,
                esc(&text(e, "Title")),
                esc(&prose(&line)),
            );
        }
        let accent = if theme.accent.is_empty() {
            String::new()
        } else {
            format!(" theme-{}", esc(&theme.accent))
        };
        bands += &format!(
            "<section class=\"theme{accent}{}\" data-theme=\"{}\" aria-labelledby=\"theme-{}\"><header class=\"theme-head\"><span class=\"theme-k\">Section {}</span><h3 id=\"theme-{}\">{}</h3><p>{}</p></header><ol class=\"path\">{items}</ol></section>",
            if theme.dashed { " dashed" } else { "" },
            esc(&theme.name),
            esc(&theme.name),
            b + 1,
            esc(&theme.name),
            esc(&theme.title),
            esc(&prose(&theme.line)),
        );
    }
    let edges: Vec<String> = c
        .edges
        .iter()
        .map(|e| match e.track {
            Some(t) => format!("{}-{}:{t}", e.from, e.to),
            None => format!("{}-{}", e.from, e.to),
        })
        .collect();
    let lessons = c.lessons().count();
    let end = format!(
        "<div class=\"path-end\"><span class=\"pin\" aria-hidden=\"true\"><span class=\"disc\">{}</span></span><span class=\"body\"><span class=\"t\">The end of the course</span><span class=\"l\">After lesson {lessons}, you know how blaze builds. New updates show up in their section as blaze changes.</span></span></div>",
        end_seal(),
    );
    ctx.script("learn");
    Ok(format!(
        "<div class=\"course\" data-cols=\"{}\" data-tracks=\"{}\" data-edges=\"{}\">{go}{bands}{end}</div>",
        c.cols,
        c.tracks,
        edges.join(" ")
    ))
}

/// What's new: every update on the course, newest first, under its date: its glyph, version and theme's tag, title,
/// line, and what it extends. client/learn.ts marks each one's state and the ones new since the reader's last visit.
fn update_log(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    let path = ctx.e(n).path.clone();
    let mut found = None;
    for p in ctx.s.pages {
        if let Some(cw) = ctx.s.world.course_in(p.node) {
            let sources = ctx.s.world.course_sources(cw)?;
            let c =
                crate::course::build(ctx.s.world, &sources).map_err(|e| format!("{path}: {e}"))?;
            if c.nodes.iter().any(|x| x.is_update()) {
                found = Some((p.clone(), c));
                break;
            }
        }
    }
    let (course_page, c) = found.ok_or_else(|| format!("{path}: no Course has an Update"))?;
    let mut days = String::new();
    let mut day: Option<String> = None;
    let mut items = String::new();
    let newest = c.updates_newest_first(ctx.s.world);
    let keys: Vec<&str> = newest.iter().map(|x| x.key.as_str()).collect();
    for node in newest {
        let i = c.nodes.iter().position(|x| x.key == node.key).expect("a node of the course");
        let crate::course::Kind::Update { date, version } = &node.kind else { continue };
        if day.as_deref() != Some(date) {
            if let Some(d) = day.take() {
                days += &log_day(&d, &items);
                items.clear();
            }
            day = Some(date.clone());
        }
        let e = ctx.e(node.id);
        let p = ctx
            .s
            .page_of(node.id)
            .ok_or_else(|| format!("{}: an Update that is not a page", e.path))?;
        let (steps, quizzes, meta) = steps_line(ctx.s.world, node.id);
        let theme = &c.themes[node.theme];
        let tag = if theme.tag.is_empty() {
            String::new()
        } else {
            let accent = if theme.accent.is_empty() {
                String::new()
            } else {
                format!(" theme-{}", esc(&theme.accent))
            };
            format!(" · <span class=\"tag{accent}\">{}</span>", esc(&theme.tag))
        };
        let to = c.extends(i);
        let to = if to.is_empty() { String::new() } else { format!("Extends {to} · ") };
        items += &format!(
            "<li class=\"node update\" data-lesson=\"{}\" data-steps=\"{steps}\" data-quizzes=\"{}\"><a href=\"{}\"><span class=\"pin\" aria-hidden=\"true\"><span class=\"disc\">{}</span><span class=\"badge\">{DONE_MARK}</span></span><span class=\"body\"><span class=\"n\"><span class=\"v\">v{}</span>{tag}<span class=\"new\" hidden>New</span></span><span class=\"t\">{}</span><span class=\"l\">{}</span><span class=\"m\">{to}{meta}</span></span></a></li>",
            esc(&node.key),
            esc(&quizzes.join(" ")),
            esc(&ctx.s.served.href(&p.url)),
            glyph(ctx, node.id)?,
            esc(version),
            esc(&text(e, "Title")),
            esc(&prose(&field(e.value("Update"), "line"))),
        );
    }
    if let Some(d) = day {
        days += &log_day(&d, &items);
    }
    ctx.script("learn");
    Ok(format!(
        "<div class=\"ulog\" data-updates=\"{}\" data-course=\"{}\">{days}</div>",
        esc(&keys.join(" ")),
        esc(&ctx.s.served.href(&course_page.url))
    ))
}

fn log_day(date: &str, items: &str) -> String {
    format!(
        "<section class=\"ulog-day\"><h2 class=\"ulog-date\"><time datetime=\"{}\">{}</time></h2><ol class=\"ulog-list\">{items}</ol></section>",
        esc(date),
        esc(&crate::course::date(date, false))
    )
}

/// Every child of the table is a row and every child of a row a cell, comments included, as the TS has it.
fn table(n: Id, ctx: &mut Ctx) -> Result<String, String> {
    let rows: Vec<Id> = ctx.e(n).children.clone();
    let (mut thead, mut tbody) = (String::new(), String::new());
    let mut plain_rows = Vec::new();
    for r in rows {
        let cells: Vec<Id> = ctx.e(r).children.clone();
        let head = cells.iter().all(|c| ctx.e(*c).has("Th"));
        let mut html = String::from("<tr>");
        let mut plain = Vec::new();
        for c in cells {
            let comp = if ctx.e(c).has("Th") { "Th" } else { "Td" };
            let tag = comp.to_lowercase();
            let raw = text(ctx.e(c), comp);
            let path = ctx.e(c).path.clone();
            html += &format!("<{tag}>{}</{tag}>", ctx.inline(&prose(&raw), &path)?);
            plain.push(raw);
        }
        html += "</tr>";
        if head {
            thead += &html
        } else {
            tbody += &html
        }
        plain_rows.push(plain.join(" | "));
    }
    let a = anchor(n, ctx, &plain_rows.join("\n"))?;
    let thead = if thead.is_empty() { String::new() } else { format!("<thead>{thead}</thead>") };
    Ok(format!(
        "<div class=\"ref\"{}><table>{thead}<tbody>{tbody}</tbody></table>{}</div>{}",
        a.attr, a.button, a.thread
    ))
}

fn references_html(ctx: &Ctx) -> String {
    if ctx.cited.is_empty() {
        return String::new();
    }
    let items: String = ctx
        .cited
        .iter()
        .map(|r| {
            let e = ctx.e(*r);
            let v = e.value("Ref");
            let note = field(v, "note");
            let note = if note.is_empty() {
                String::new()
            } else {
                format!(" <span class=\"dim\">{}</span>", esc(&prose(&note)))
            };
            format!(
                "<li id=\"ref-{}\"><a href=\"{}\">{}</a>{note}</li>",
                esc(&e.name),
                esc(&ctx.s.served.href(&field(v, "href"))),
                esc(&field(v, "label"))
            )
        })
        .collect();
    format!(
        "<section id=\"references\"><h2>References</h2><ol class=\"references\">{items}</ol></section>"
    )
}

/// RFC 0001 · Draft · Read first: Charter
fn meta(ctx: &Ctx) -> Result<String, String> {
    let e = ctx.e(ctx.page.node);
    let mut parts = Vec::new();
    if let Some(num) = e.value("Number") {
        parts.push(format!("RFC {}", js::pad_zero(&js::string(num.get("value")), 4)));
    }
    // An update: its date and version, as What's new lists it.
    if let Some(u) = e.value("Update") {
        let date = field(Some(u), "date");
        parts.push(format!(
            "<time datetime=\"{}\">{}</time>",
            esc(&date),
            esc(&crate::course::date(&date, false))
        ));
        parts.push(format!("<span class=\"mono\">v{}</span>", esc(&field(Some(u), "version"))));
    }
    if e.targets("Status").is_some() {
        parts.push(status(e));
    }
    let mut deps = Vec::new();
    for d in e.targets("DependsOn").unwrap_or_default() {
        let target = ctx.s.world.lookup(d);
        let p = target
            .and_then(|t| ctx.s.page_of(t))
            .ok_or_else(|| format!("{}: (DependsOn, {d}) is not a page", e.path))?;
        deps.push(format!(
            "<a href=\"{}\">{}</a>",
            esc(&ctx.s.served.href(&p.url)),
            esc(&text(ctx.e(p.node), "Title"))
        ));
    }
    if !deps.is_empty() {
        parts.push(format!("Read first: {}", deps.join(", ")));
    }
    Ok(if parts.is_empty() {
        String::new()
    } else {
        format!("<p class=\"meta\">{}</p>", parts.join(" <span>·</span> "))
    })
}

/// On a lesson or update page, what client/learn.ts needs on `<main>`: its key in the reader's progress, the line above
/// its title on the player's cover, its line, its questions, where the course path is, and the next lesson (for a
/// lesson) or What's new (for an update).
fn lesson_attrs(ctx: &mut Ctx) -> Result<(&'static str, String), String> {
    let e = ctx.e(ctx.page.node);
    let Some(v) = e.value("Lesson").or_else(|| e.value("Update")) else {
        return Ok(("", String::new()));
    };
    let line = prose(&field(Some(v), "line"));
    let path = e.path.clone();
    let parent = e.parent.ok_or("a lesson has no parent")?;
    // The course path is on the page whose Course widget lists this page's parent (the home page).
    let (course_at, c) = ctx
        .s
        .world
        .course_of(ctx.s.pages, parent)?
        .ok_or_else(|| format!("{path}: no page's Course lists this lesson"))?;
    let course = &ctx.s.pages[course_at];
    let i = c.nodes.iter().position(|x| x.id == ctx.page.node).expect("the page is on its course");
    let node = &c.nodes[i];
    let (eyebrow, next) = match &node.kind {
        crate::course::Kind::Lesson(k) => {
            let next = match c.lessons().nth(*k).and_then(|l| ctx.s.page_of(l.id)) {
                Some(p) => format!(
                    " data-next=\"{}\" data-next-title=\"{}\"",
                    esc(&ctx.s.served.href(&p.url)),
                    esc(&text(ctx.e(p.node), "Title"))
                ),
                None => String::new(),
            };
            (format!("Lesson {k}"), next)
        }
        // An update's way on is What's new, its parent page.
        crate::course::Kind::Update { .. } => {
            let log = ctx
                .s
                .page_of(parent)
                .map(|p| format!(" data-log=\"{}\"", esc(&ctx.s.served.href(&p.url))))
                .unwrap_or_default();
            (update_eyebrow(&c, i, false), log)
        }
    };
    let (_, quizzes) = lesson_parts(ctx.s.world, ctx.page.node);
    let key = node.key.clone();
    ctx.script("learn");
    Ok((
        " lesson",
        format!(
            " data-lesson=\"{}\" data-eyebrow=\"{}\" data-line=\"{}\" data-quizzes=\"{}\" data-course=\"{}\"{next}",
            esc(&key),
            esc(&eyebrow),
            esc(&line),
            esc(&quizzes.join(" ")),
            esc(&ctx.s.served.href(&course.url)),
        ),
    ))
}

/// Whether a page takes comments: block by block (`Discussable`, a Doc), or as a whole (any page not `Quiet`).
pub fn talks(e: &crate::world::Entity) -> bool {
    e.has_tag("Discussable") || !e.has_tag("Quiet")
}

/// One page. On a discussable page, comment entities under its blocks are drawn as threads, for `discussion`'s
/// viewer. Returns the HTML and, for a discussable page, its blocks.
pub fn render(
    s: &Site,
    page: &Page,
    discussion: &Discussion,
) -> Result<(String, Option<Blocks>), String> {
    let n = s.world.get(page.node);
    // Comments per block on a Doc; every other page that talks has only the page's own thread.
    let discussable = n.has_tag("Discussable");
    let talk = talks(n);
    let mut ctx = Ctx {
        s,
        page,
        cited: Vec::new(),
        discussable,
        blocks: Vec::new(),
        scripts: Vec::new(),
        discussion,
        say: None,
    };
    // The build sets `Audio {manifest}` on a page whose generated audio matches its spoken lines (gen/build.ts).
    let audio = match n.value("Audio").and_then(|v| v.get("manifest")) {
        Some(crate::world::Scalar::Str(m)) if !m.is_empty() => Some(m.clone()),
        _ => None,
    };
    if audio.is_some() {
        let lines = crate::spoken::lines(s, page)?;
        ctx.say = Some(lines.into_iter().map(|l| l.block).collect());
        ctx.script("listen");
    }
    let title = text(n, "Title");
    if title.is_empty() {
        return Err(format!("{}: a page needs a Title", n.path));
    }
    let mut body = children(page.node, &mut ctx)?;
    let (lesson_class, lesson_attr) = lesson_attrs(&mut ctx)?;
    let refs = references_html(&ctx);
    body = if body.contains(REFERENCES) {
        js::replace_first(&body, REFERENCES, &refs)
    } else {
        body + &refs
    };
    let mut out_blocks = None;
    if talk {
        ctx.script("comments");
        let hint = if discussable {
            "Comments on the page as a whole. To comment on one paragraph, use the button beside it."
        } else {
            "Questions, corrections, ideas: anything about this page."
        };
        // The page's own comments, and those whose block is gone, are children of the page entity.
        body += &format!(
            "\n<section class=\"discussion\" id=\"discussion\" data-block=\"{PAGE_BLOCK}\"><h2>Discussion</h2><p class=\"dim\">{hint}</p>{}</section>{}",
            comments::thread(
                PAGE_BLOCK, s.world, page.node, &page.doc, &page.url, discussion, s.served
            ),
            comments::sign_out_form(&page.url, discussion, s.served)
        );
        let mut blocks = vec![(PAGE_BLOCK.to_string(), title.clone())];
        for (k, v) in std::mem::take(&mut ctx.blocks) {
            match blocks.iter_mut().find(|(x, _)| *x == k) {
                Some((_, old)) => *old = v,
                None => blocks.push((k, v)),
            }
        }
        out_blocks = Some(Blocks { doc: page.doc.clone(), title: title.clone(), blocks });
    }
    let tab = match text(n, "Tab") {
        t if !t.is_empty() => t,
        _ if page.url == "/" => title.clone(),
        _ => format!("{title} · blaze"),
    };
    let description = prose(&text(n, "Description"));
    // Where the page lives, for search engines and link previews: origin + base + path (none without an origin).
    let canonical = if s.served.origin.is_empty() {
        String::new()
    } else {
        let at = esc(&format!("{}{}", s.served.origin, s.served.href(&page.url)));
        let og_description = if description.is_empty() {
            String::new()
        } else {
            format!("\n<meta property=\"og:description\" content=\"{}\" />", esc(&description))
        };
        format!(
            "<link rel=\"canonical\" href=\"{at}\" />
<meta property=\"og:type\" content=\"website\" />
<meta property=\"og:site_name\" content=\"blaze\" />
<meta property=\"og:title\" content=\"{}\" />
<meta property=\"og:url\" content=\"{at}\" />{og_description}",
            esc(&title)
        )
    };
    let description = if description.is_empty() {
        String::new()
    } else {
        format!("<meta name=\"description\" content=\"{}\" />", esc(&description))
    };
    let width = if n.has_tag("Narrow") {
        " narrow"
    } else if n.has_tag("Wide") {
        " wide"
    } else {
        ""
    };
    // Every page has the client router (moving between pages without a load) and the sidebar's script (folds, the
    // phone drawer, search, the outline's scroll spy).
    ctx.script("nav");
    ctx.script("side");
    let scripts = ctx
        .scripts
        .iter()
        .map(|js| {
            format!(
                "<script type=\"module\" src=\"{}\"></script>",
                esc(&s.served.href(&format!("/js/{js}.js")))
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let doc_attr = if talk { format!(" data-doc=\"{}\"", esc(&page.doc)) } else { String::new() };
    let say_intro = if audio.is_some() {
        format!(" data-say=\"{}\"", crate::spoken::INTRO)
    } else {
        String::new()
    };
    let listen = match &audio {
        // The player (client/listen.ts) builds its controls in here; without JS it stays empty.
        Some(m) => {
            format!("\n<div class=\"listen\" data-audio=\"{}\"></div>", esc(&s.served.href(m)))
        }
        None => String::new(),
    };
    // The home page's title is the logo, large; every other page has the docs' header: where it sits, the title,
    // the RFC line.
    let head = if n.has_tag("Hero") {
        format!(
            "<header class=\"hero\"><h1 class=\"hero-logo\"{say_intro}><span class=\"sr\">{}</span>{}</h1>{listen}</header>",
            esc(&title),
            crate::side::picture(s.world, s.site, s.served, "logo", "")
        )
    } else {
        // A lesson's glyph above its title, as on its node on the course path (client/learn.ts moves it to the
        // player's cover).
        let glyph = if n.has("Lesson") || n.has("Update") {
            format!("<span class=\"lesson-glyph\">{}</span>", glyph(&ctx, page.node)?)
        } else {
            String::new()
        };
        format!(
            "<header class=\"head\">{}{glyph}<h1{say_intro}>{}</h1>{}{listen}</header>",
            crate::side::crumb(s.nav, page, s.served),
            esc(&title),
            meta(&ctx)?
        )
    };
    // "On this page": the page's top-level sections, then its references.
    let mut heads: Vec<(String, String)> = Vec::new();
    for &c in &n.children {
        if let Some(v) = s.world.get(c).value("Section") {
            let t = field(Some(v), "title").replace(['`', '*'], "");
            heads.push((ctx.block_id(c), t));
        }
    }
    if !ctx.cited.is_empty() {
        heads.push(("references".into(), "References".into()));
    }
    let toc = if n.has_tag("Wide") { String::new() } else { crate::side::toc(&heads) };
    // The square icon (docs/brand/fire/icon.py); the dark one carries its own background, so it reads on any tab bar.
    // The .ico and the 180px PNG for browsers and home screens that take no SVG.
    let icon = esc(&s.served.href("/brand/blaze-icon-dark.svg"));
    let ico = esc(&s.served.href("/brand/favicon.ico"));
    let touch = esc(&s.served.href("/brand/blaze-icon-dark-180.png"));
    let html = format!(
        "<!doctype html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\" />
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />
<meta name=\"color-scheme\" content=\"dark light\" />
<title>{}</title>
<link rel=\"icon\" href=\"{ico}\" sizes=\"32x32\" />
<link rel=\"icon\" type=\"image/svg+xml\" href=\"{icon}\" />
<link rel=\"apple-touch-icon\" href=\"{touch}\" />
{description}
{canonical}
<link rel=\"stylesheet\" href=\"{css}\" />
{scripts}
</head>
<body>
<div class=\"frame\">
{}
<script src=\"{keep}\"></script>
<div class=\"pane\">
{}
<div class=\"grid{width}\">
<main class=\"doc{width}{lesson_class}\"{doc_attr}{lesson_attr}>
{head}
{body}
{}
</main>
{toc}
</div>
</div>
</div>
</body>
</html>
",
        esc(&tab),
        crate::side::sidebar(s.nav, page, s.served, s.world, s.site),
        crate::side::mobile_bar(s.served, s.world, s.site),
        crate::side::pager(s.nav, page, s.served),
        css = esc(&s.served.href("/site.css")),
        // Classic and synchronous on purpose: it runs as soon as the sidebar is parsed, before the first paint.
        keep = esc(&s.served.href("/js/keep.js")),
    );
    Ok((html, out_blocks))
}

/// `.gen/discussable.json`'s shape: doc to title and blocks.
pub type Discussable = BTreeMap<String, Blocks>;
