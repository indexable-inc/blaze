//! The sidebar every page carries, after the ix docs' (packages/web src/lib/site/side): the logo, the search pill,
//! then sections with an icon and a fold caret, each a list of rows. It is built once from the world:
//!
//! - a section is a child of `nav` with `Group {title, order}` and `Icon {name}`;
//! - a page joins one with `Nav {label, order}`, `Icon` and `(InGroup, nav.<section>)`;
//! - a `Link {label, href, order}` child of a section is a row that goes elsewhere (another site, or an anchor);
//! - a page's RFCs (`World::rfcs`, by number) are its row's kids, folded under a caret;
//! - a page holding a `Course` (the home page) is followed by its lessons, numbered, each with a progress mark that
//!   client/side.ts fills from the reader's progress;
//! - a section with `FoldOnPhone` (Reference) starts folded in the phone drawer (client/keep.ts).
//!
//! Rows sort by `order` within their section, sections by theirs. Folding is `<details>`, so it works without JS;
//! client/side.ts remembers what the reader folded, opens the phone drawer, runs the search and the scroll spy.

use crate::icons;
use crate::inline::esc;
use crate::js;
use crate::render::{Page, Served};
use crate::world::{Entity, Id, World};

/// One row: a page or a link, with its kids (an RFC page's RFCs).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub label: String,
    /// A site path (`/install`, `/#speed`) or an outside URL.
    pub href: String,
    pub icon: String,
    pub order: f64,
    /// An RFC's number, shown before its title (`0001`).
    pub num: Option<String>,
    /// A lesson row: the lesson's name and its question ids, for its progress mark.
    pub lesson: Option<(String, String)>,
    pub kids: Vec<Row>,
}

impl Row {
    pub fn outside(&self) -> bool {
        self.href.starts_with("http:") || self.href.starts_with("https:")
    }
}

/// One sidebar section.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// The section entity's name under `nav` (`design`): the key client/side.ts remembers a fold by.
    pub key: String,
    pub title: String,
    pub icon: String,
    pub order: f64,
    /// Folded in the phone drawer until the reader opens it.
    pub phone_fold: bool,
    pub rows: Vec<Row>,
}

fn field(e: &Entity, comp: &str, k: &str) -> String {
    js::string(e.value(comp).and_then(|v| v.get(k)))
}

fn icon_of(e: &Entity, default: &str) -> Result<String, String> {
    let name = match field(e, "Icon", "name") {
        n if n.is_empty() => default.to_string(),
        n => n,
    };
    if icons::svg(&name).is_none() {
        let known: Vec<&str> = icons::names().collect();
        return Err(format!("{}: Icon {name:?} is not one of {}", e.path, known.join(", ")));
    }
    Ok(name)
}

fn by_order(rows: &mut [Row]) {
    // Stable, so equal orders keep declaration order.
    rows.sort_by(|a, b| a.order.total_cmp(&b.order));
}

/// The sections and their rows, checked: every child of `nav` is a section, every page with `Nav` is in one, every
/// icon exists, every link is http(s) or a site path.
pub fn build(world: &World, pages: &[Page]) -> Result<Vec<Group>, String> {
    let Some(nav) = world.lookup("nav") else { return Ok(Vec::new()) };
    let page_of = |n: Id| pages.iter().find(|p| p.node == n);
    let mut groups = Vec::new();
    for &g in &world.get(nav).children {
        let e = world.get(g);
        if !e.has("Group") {
            return Err(format!("{}: nav's children are sections (Group)", e.path));
        }
        let mut rows = Vec::new();
        for &c in &e.children {
            let l = world.get(c);
            if !l.has("Link") {
                return Err(format!("{}: a section's children are Links", l.path));
            }
            let href = field(l, "Link", "href");
            if !(href.starts_with("http://")
                || href.starts_with("https://")
                || href.starts_with('/'))
            {
                return Err(format!("{}: a Link's href is http(s) or /", l.path));
            }
            rows.push(Row {
                label: field(l, "Link", "label"),
                href,
                icon: icon_of(l, "out")?,
                order: js::to_number(l.value("Link").and_then(|v| v.get("order"))),
                num: None,
                lesson: None,
                kids: Vec::new(),
            });
        }
        groups.push((
            e.path.clone(),
            Group {
                key: e.name.clone(),
                title: field(e, "Group", "title"),
                icon: icon_of(e, "compass")?,
                order: js::to_number(e.value("Group").and_then(|v| v.get("order"))),
                phone_fold: e.has_tag("FoldOnPhone"),
                rows,
            },
        ));
    }
    for p in pages {
        let e = world.get(p.node);
        let section = e.targets("InGroup").and_then(|t| t.first());
        let Some(nav_value) = e.value("Nav") else {
            if section.is_some() {
                return Err(format!("{}: (InGroup, ..) without Nav {{label, order}}", e.path));
            }
            continue;
        };
        let section =
            section.ok_or_else(|| format!("{}: Nav without (InGroup, nav.<section>)", e.path))?;
        let mut kids = Vec::new();
        for r in world.rfcs(p.node) {
            let re = world.get(r);
            let rp = page_of(r).ok_or_else(|| format!("{}: an Rfc that is not a page", re.path))?;
            kids.push(Row {
                label: field(re, "Title", "text"),
                href: rp.url.clone(),
                icon: icon_of(re, "file")?,
                order: 0.0,
                num: Some(js::pad_zero(&field(re, "Number", "value"), 4)),
                lesson: None,
                kids: Vec::new(),
            });
        }
        let row = Row {
            label: js::string(nav_value.get("label")),
            href: p.url.clone(),
            icon: icon_of(e, "file")?,
            order: js::to_number(nav_value.get("order")),
            num: None,
            lesson: None,
            kids,
        };
        let (_, g) = groups
            .iter_mut()
            .find(|(path, _)| path == section)
            .ok_or_else(|| format!("{}: (InGroup, {section}) is not a section of nav", e.path))?;
        // The page holding the course path is followed by its lessons, numbered in course order.
        let mut lessons = Vec::new();
        if let Some(c) = world.course_in(p.node) {
            for (i, l) in world.lessons(world.course_source(c)?).into_iter().enumerate() {
                let le = world.get(l);
                let lp = page_of(l)
                    .ok_or_else(|| format!("{}: a Lesson that is not a page", le.path))?;
                let (_, quizzes) = crate::render::lesson_parts(world, l);
                lessons.push(Row {
                    label: field(le, "Title", "text"),
                    href: lp.url.clone(),
                    icon: String::new(),
                    order: row.order + (i + 1) as f64 / 1000.0,
                    num: Some((i + 1).to_string()),
                    lesson: Some((le.name.clone(), quizzes.join(" "))),
                    kids: Vec::new(),
                });
            }
        }
        g.rows.push(row);
        g.rows.extend(lessons);
    }
    let mut out: Vec<Group> = groups.into_iter().map(|(_, g)| g).collect();
    for g in &mut out {
        by_order(&mut g.rows);
    }
    out.sort_by(|a, b| a.order.total_cmp(&b.order));
    Ok(out)
}

fn svg(name: &str) -> String {
    icons::svg(name).unwrap_or_default()
}

/// The logo (`Brand {dark, light, width, height}` on `site`): the dark file by default, the light one when the
/// reader's system is light. `alt` is empty where the words sit beside it.
pub fn picture(world: &World, site: Id, served: &Served, class: &str, alt: &str) -> String {
    picture_of(world, site, served, ("dark", "light"), class, alt)
}

/// The logo where it is small (the sidebar, the phone bar): the still first frame, and the fire only while the
/// pointer is on it or it has keyboard focus (shell.css swaps `.still` for `.anim`). Without `Brand`'s still files
/// it is the animated logo.
pub fn hover_picture(world: &World, site: Id, served: &Served) -> String {
    if field(world.get(site), "Brand", "still_dark").is_empty() {
        return picture(world, site, served, "logo", "");
    }
    format!(
        "{}{}",
        picture_of(world, site, served, ("still_dark", "still_light"), "logo still", ""),
        picture(world, site, served, "logo anim", "")
    )
}

/// The logo from `Brand`'s two file fields `keys` (dark, light).
fn picture_of(
    world: &World,
    site: Id,
    served: &Served,
    keys: (&str, &str),
    class: &str,
    alt: &str,
) -> String {
    let e = world.get(site);
    let (dark, light) = (field(e, "Brand", keys.0), field(e, "Brand", keys.1));
    if dark.is_empty() {
        return format!("<span class=\"{}\">blaze</span>", esc(class));
    }
    let (w, h) = (
        js::number(js::to_number(e.value("Brand").and_then(|v| v.get("width")))),
        js::number(js::to_number(e.value("Brand").and_then(|v| v.get("height")))),
    );
    format!(
        "<picture class=\"{}\"><source media=\"(prefers-color-scheme: light)\" srcset=\"{}\" /><img src=\"{}\" width=\"{w}\" height=\"{h}\" alt=\"{}\" /></picture>",
        esc(class),
        esc(&served.href(&light)),
        esc(&served.href(&dark)),
        esc(alt)
    )
}

/// Is this row the page, or does it hold the page among its kids?
fn holds(row: &Row, url: &str) -> bool {
    row.href == url || row.kids.iter().any(|k| holds(k, url))
}

fn row_html(row: &Row, page: &Page, served: &Served, depth: usize) -> String {
    let here = row.href == page.url;
    let out = row.outside();
    let num = match &row.num {
        Some(n) => format!("<span class=\"num\">{}</span>", esc(n)),
        None => String::new(),
    };
    // A lesson's progress mark stands where a row's icon is (client/side.ts sets data-state).
    let (lead, lesson) = match &row.lesson {
        Some((name, quizzes)) => (
            "<span class=\"mark\" aria-hidden=\"true\"></span>".to_string(),
            format!(" data-lesson=\"{}\" data-quizzes=\"{}\"", esc(name), esc(quizzes)),
        ),
        None => (svg(&row.icon), String::new()),
    };
    let kids = if row.kids.is_empty() {
        String::new()
    } else {
        let inner: String = row.kids.iter().map(|k| row_html(k, page, served, depth + 1)).collect();
        format!(
            "<details class=\"fold\" open><summary class=\"caret\" aria-label=\"Show the {} pages\">{}</summary><div class=\"kids\">{inner}</div></details>",
            esc(&row.label),
            svg("caret")
        )
    };
    format!(
        "<div class=\"rowwrap{}\"><a class=\"row\" href=\"{}\" title=\"{}\"{}{}{lesson}>{lead}{num}<span class=\"label\">{}</span>{}</a>{kids}</div>",
        if depth > 0 { " kid" } else { "" },
        esc(&served.href(&row.href)),
        esc(&row.label),
        if here { " aria-current=\"page\"" } else { "" },
        if out { " rel=\"noopener\"" } else { "" },
        esc(&row.label),
        if out {
            svg("out").replace("class=\"icon\"", "class=\"icon outmark\"")
        } else {
            String::new()
        },
    )
}

/// The sidebar for one page: its row is the current one, its section open.
pub fn sidebar(groups: &[Group], page: &Page, served: &Served, world: &World, site: Id) -> String {
    let sections: String = groups
        .iter()
        .map(|g| {
            let rows: String = g.rows.iter().map(|r| row_html(r, page, served, 0)).collect();
            let has = g.rows.iter().any(|r| holds(r, &page.url));
            format!(
                "<details class=\"sec\" open data-sec=\"{}\"{}{}><summary class=\"sechead\">{}<span>{}</span>{}</summary><div class=\"items\">{rows}</div></details>",
                esc(&g.key),
                if has { " data-here=\"\"" } else { "" },
                if g.phone_fold { " data-phone-fold=\"\"" } else { "" },
                svg(&g.icon),
                esc(&g.title),
                svg("caret").replace("class=\"icon\"", "class=\"icon chev\"")
            )
        })
        .collect();
    format!(
        "<aside class=\"side\" id=\"menu\" aria-label=\"Site\">
<div class=\"sidetop\"><a class=\"brand\" href=\"{home}\" aria-label=\"blaze, home\">{logo}</a><a class=\"closebtn\" href=\"#\" aria-label=\"Close the menu\">{close}</a></div>
<button type=\"button\" class=\"searchbtn\" data-search hidden aria-keyshortcuts=\"Meta+K Control+K\">{search}<span>Search</span><kbd>⌘K</kbd></button>
<div class=\"scrollbox\"><nav class=\"navlist\" aria-label=\"Pages\">{sections}</nav></div>
<div class=\"foot\"><a href=\"https://ix.dev\">Made at ix</a><a href=\"https://github.com/indexable-inc/blaze/tree/main/site\">Site source</a></div>
</aside>
<a class=\"scrim\" href=\"#\" aria-label=\"Close the menu\" tabindex=\"-1\"></a>",
        home = esc(&served.href("/")),
        logo = hover_picture(world, site, served),
        close = svg("close"),
        search = svg("search"),
    )
}

/// The phone bar above the page: Menu (opens the drawer, `#menu`), the logo, search.
pub fn mobile_bar(served: &Served, world: &World, site: Id) -> String {
    format!(
        "<div class=\"mbar\"><a class=\"menubtn\" href=\"#menu\" aria-controls=\"menu\">{}<span>Menu</span></a><a class=\"mbrand\" href=\"{}\" aria-label=\"blaze, home\">{}</a><button type=\"button\" class=\"menubtn msearch\" data-search hidden aria-label=\"Search\">{}</button></div>",
        svg("menu"),
        esc(&served.href("/")),
        hover_picture(world, site, served),
        svg("search")
    )
}

/// The section holding the page's row (directly or as a kid), and the row.
fn find<'g>(groups: &'g [Group], url: &str) -> Option<(&'g Group, &'g Row)> {
    for g in groups {
        for r in &g.rows {
            if r.href == url {
                return Some((g, r));
            }
            if r.kids.iter().any(|k| k.href == url) {
                return Some((g, r));
            }
        }
    }
    None
}

/// Above the title: the section's icon and name, or for a page under another page (an RFC), a link to that page.
pub fn crumb(groups: &[Group], page: &Page, served: &Served) -> String {
    let Some((g, r)) = find(groups, &page.url) else { return String::new() };
    if r.href != page.url {
        return format!(
            "<p class=\"crumb\"><a href=\"{}\">{}<span>{}</span></a></p>",
            esc(&served.href(&r.href)),
            svg(&r.icon),
            esc(&r.label)
        );
    }
    format!("<p class=\"crumb\">{}<span>{}</span></p>", svg(&g.icon), esc(&g.title))
}

/// Every page in the sidebar, in reading order (a row, then its kids): what Previous and Next walk.
fn reading_order(groups: &[Group]) -> Vec<&Row> {
    fn walk<'g>(r: &'g Row, out: &mut Vec<&'g Row>) {
        if !r.outside() && !r.href.contains('#') {
            out.push(r);
        }
        for k in &r.kids {
            walk(k, out);
        }
    }
    let mut out = Vec::new();
    for g in groups {
        for r in &g.rows {
            walk(r, &mut out);
        }
    }
    out
}

/// Previous and Next under the page, in sidebar order. Empty for a page not in the sidebar.
pub fn pager(groups: &[Group], page: &Page, served: &Served) -> String {
    let order = reading_order(groups);
    let Some(at) = order.iter().position(|r| r.href == page.url) else { return String::new() };
    let link = |r: &Row, cls: &str, dir: String| {
        format!(
            "<a class=\"{cls}\" href=\"{}\"><span class=\"dir\">{dir}</span><span class=\"name\">{}<span>{}</span></span></a>",
            esc(&served.href(&r.href)),
            svg(&r.icon),
            esc(&r.label)
        )
    };
    let prev = if at > 0 {
        link(order[at - 1], "prev", format!("{}Previous", svg("left")))
    } else {
        String::new()
    };
    let next = match order.get(at + 1) {
        Some(r) => link(r, "next", format!("Next{}", svg("right"))),
        None => String::new(),
    };
    format!("<nav class=\"pager\" aria-label=\"Reading order\">{prev}{next}</nav>")
}

/// "On this page": the page's top-level sections (`(id, title)`), when there are two or more.
pub fn toc(heads: &[(String, String)]) -> String {
    if heads.len() < 2 {
        return String::new();
    }
    let items: String = heads
        .iter()
        .map(|(id, t)| format!("<li><a href=\"#{}\">{}</a></li>", esc(id), esc(t)))
        .collect();
    format!(
        "<aside class=\"toc\" aria-label=\"On this page\"><p>On this page</p><ul>{items}</ul></aside>"
    )
}
